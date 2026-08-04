use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gphoto2::widget::{RadioWidget, RangeWidget, ToggleWidget, Widget};

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

/// Interval between keep-alive pings. Canon bodies sleep after their
/// `autopoweroff` timer (as low as ~30 s on some menus), so we ping well under it.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// libgphoto2-backed camera backend.
///
/// `gphoto2::Context` and `gphoto2::Camera` are documented `Send + Sync`; the
/// crate serializes per-camera calls internally. Open `Camera` handles live in a
/// shared `Arc<Mutex<HashMap>>` so the background keep-alive thread can ping them.
pub struct GPhoto2Backend {
    context:   gphoto2::Context,
    connected: Arc<Mutex<HashMap<String, gphoto2::Camera>>>,
    /// Whether a body can actually drive its focus motor, keyed by native ID
    /// (port). Filled once per device by `focus_drive_works`, because the widget
    /// alone does not answer the question (see there). Never cleared: a body that
    /// refused once will refuse again, and re-probing on every parameter read
    /// would nudge the lens each time.
    focus_drive: Arc<Mutex<HashMap<String, bool>>>,
}

impl GPhoto2Backend {
    pub fn new() -> Result<Self, CameraError> {
        // libgphoto2 localizes parameter labels via gettext, following the system
        // locale (French here: "Automatique", "pose longue", even "0,5" for "0.5").
        // Force the C locale before the first gphoto2 call — which is what activates
        // gettext — so labels and numeric formatting are stable English/ASCII. This
        // also keeps the option `value`s we round-trip back to `set_choice`
        // consistent between reads and writes.
        std::env::set_var("LC_ALL", "C");

        // If libgphoto2's plugins were bundled next to the binary (build.rs
        // `copy_gphoto2_bundle`), point libgphoto2 at them so the server runs
        // without a system libgphoto2 install. No-op otherwise → dev builds use
        // the system install.
        Self::use_bundled_plugins();

        let context = gphoto2::Context::new().map_err(map_err)?;
        let connected: Arc<Mutex<HashMap<String, gphoto2::Camera>>> =
            Arc::new(Mutex::new(HashMap::new()));
        spawn_keepalive(connected.clone());
        Ok(Self { context, connected, focus_drive: Arc::new(Mutex::new(HashMap::new())) })
    }

    /// Point `CAMLIBS`/`IOLIBS` at plugin directories shipped next to the
    /// executable, if present. Must run before the first gphoto2 call.
    fn use_bundled_plugins() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(dir) = exe.parent() else {
            return;
        };
        for (var, sub) in [("CAMLIBS", "camlibs"), ("IOLIBS", "iolibs")] {
            let path = dir.join(sub);
            if path.is_dir() {
                std::env::set_var(var, &path);
            }
        }
    }

    /// Returns a clone of the live `Camera` handle for `native_id`.
    /// The clone is a refcount bump on the underlying gphoto2 camera, so we
    /// release the lock before doing any (potentially slow) SDK call.
    fn camera_for(&self, native_id: &str) -> Result<gphoto2::Camera, CameraError> {
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        connected
            .get(native_id)
            .cloned()
            .ok_or(CameraError::NotConnected)
    }

    /// Whether the body can really drive its focus motor — asked once, then cached.
    ///
    /// The widget alone does not answer this: libgphoto2 builds it from the PTP
    /// operations the camera *advertises*, and some bodies advertise more than they
    /// honour. The Nikon Z5 II exposes `manualfocusdrive` and then answers "not in
    /// live view" to every drive command, even with live view demonstrably running
    /// (`viewfinder` reads back on, previews stream fine). Nothing in the config
    /// tree tells the two apart, so the only honest test is to try it: drive the
    /// motor by one step and see. A step of 1 is the smallest move the body can
    /// make and is imperceptible in the preview.
    ///
    /// Called from `get_parameters`, so a body that cannot focus never advertises
    /// the control in the first place. Only bodies exposing the focus drive as a
    /// range (Nikon) are probed — a Canon-style radio has no neutral step to send,
    /// and Canon focus drive works, so those are taken at their word and fall back
    /// to learning from a refusal in `drive_focus`.
    fn focus_drive_works(&self, native_id: &str, camera: &gphoto2::Camera) -> bool {
        if let Some(known) = self.focus_drive.lock().expect("gphoto2 mutex poisoned").get(native_id)
        {
            return *known;
        }

        let Some(widget) = widget_for::<RangeWidget>(camera, ParameterType::Focus) else {
            return true;
        };

        // The motor only obeys in live view, and a preview grab is what gets the
        // body there — a no-op on one already streaming.
        let _ = camera.capture_preview().wait();
        widget.set_value(1.0);

        match camera.set_config(&widget).wait() {
            Ok(()) => {
                self.remember_focus_drive(native_id, true);
                true
            }
            Err(e) if is_focus_refusal(&e) => {
                eprintln!(
                    "[gphoto2] {native_id}: the body refuses to drive the focus motor ({e}) \
                     — hiding the Focus parameter for this device"
                );
                self.remember_focus_drive(native_id, false);
                false
            }
            // A busy or I/O error says nothing about the body's capabilities: leave
            // it unclassified and let the next parameter read ask again.
            Err(e) => {
                eprintln!("[gphoto2] {native_id}: focus-drive probe inconclusive ({e})");
                true
            }
        }
    }

    fn remember_focus_drive(&self, native_id: &str, works: bool) {
        self.focus_drive
            .lock()
            .expect("gphoto2 mutex poisoned")
            .insert(native_id.to_string(), works);
    }

    /// Drives the focus motor by a relative amount.
    ///
    /// A refusal here is the same signal `focus_drive_works` probes for, so record
    /// it: it is how the bodies that skip the probe (Canon-style radio widget) stop
    /// advertising a control they cannot honour. "Focus at limit" / "stepping too
    /// small" (`CameraError`) mean the motor *did* move, and a busy error is
    /// transient — neither counts.
    fn drive_focus(
        &self,
        native_id: &str,
        camera: &gphoto2::Camera,
        value: &str,
    ) -> Result<(), CameraError> {
        let _ = camera.capture_preview().wait();

        // Range on Nikon, Radio ("Near 1"/"Far 1"…) on Canon.
        let focus = ParameterType::Focus;
        let outcome = if let Some(widget) = widget_for::<RangeWidget>(camera, focus) {
            let v: f32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            widget.set_value(v);
            camera.set_config(&widget).wait()
        } else if let Some(widget) = widget_for::<RadioWidget>(camera, focus) {
            widget.set_choice(value).map_err(map_err)?;
            camera.set_config(&widget).wait()
        } else {
            return Err(CameraError::NotSupported);
        };

        match outcome {
            Ok(()) => Ok(()),
            Err(e) if is_focus_refusal(&e) => {
                eprintln!(
                    "[gphoto2] {native_id}: the body refused to drive the focus motor ({e}) \
                     — hiding the Focus parameter for this device"
                );
                self.remember_focus_drive(native_id, false);
                Err(CameraError::Backend(
                    "gphoto2: this camera does not support driving the focus motor".to_string(),
                ))
            }
            Err(e) => Err(map_err(e)),
        }
    }
}

/// Whether a failed focus drive means "this body cannot do it" rather than "not
/// right now". libgphoto2 reports the Nikon refusal as a bare `GP_ERROR`
/// (`Other`), and a body that never advertised the operation as `NotSupported`.
fn is_focus_refusal(err: &gphoto2::Error) -> bool {
    matches!(err.kind(), gphoto2::error::ErrorKind::Other | gphoto2::error::ErrorKind::NotSupported)
}

/// Background thread that keeps every connected camera awake. Canon bodies refuse
/// to have `autopoweroff` disabled (PTP `0x2019` Device Busy), so instead we
/// generate periodic activity — any PTP transaction resets the body's idle timer,
/// the same trick EOS Utility uses. Runs for the whole process lifetime; cameras
/// are pinged only while present in the map.
fn spawn_keepalive(connected: Arc<Mutex<HashMap<String, gphoto2::Camera>>>) {
    thread::Builder::new()
        .name("gphoto2-keepalive".into())
        .spawn(move || loop {
            thread::sleep(KEEPALIVE_INTERVAL);
            // Snapshot the handles, then release the lock before the (slow) PTP
            // reads so connect/disconnect/operations are never blocked on us.
            let cameras: Vec<gphoto2::Camera> = {
                let map = connected.lock().expect("gphoto2 mutex poisoned");
                map.values().cloned().collect()
            };
            for camera in cameras {
                // Reading the config tree is enough activity to reset the timer
                // and works on any gphoto2 body. Best-effort: ignore errors (a
                // camera may have just been unplugged).
                let _ = camera.config().wait();
            }
        })
        .expect("failed to spawn gphoto2 keep-alive thread");
}

/// libgphoto2 errors carry a human-readable reason ("Camera is busy", "Could not
/// claim the USB device", plus any driver detail) — keep it, so a failed
/// `set_parameter` tells the user what the body actually refused instead of an
/// opaque `SDK error: 0x00000000`.
fn map_err(err: gphoto2::Error) -> CameraError {
    eprintln!("[gphoto2] error: {err}");
    CameraError::Backend(format!("gphoto2: {err}"))
}

/// libgphoto2 reports cameras it has no specific driver for under a generic
/// PTP/MTP class name (e.g. a new body shows as "USB PTP Class Camera", not its
/// model). For those we take the model from the USB product string instead, so
/// the cross-backend dedup key still matches the dedicated SDK backend's.
fn is_generic_ptp_name(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("ptp class camera") || m == "ptp camera" || m == "mtp device"
}

/// Parses a libgphoto2 USB port (`"usb:BUS,DEV"`, zero-padded decimals) into the
/// `(bus number, device address)` pair libusb/nusb use. `None` for non-USB ports.
fn parse_usb_port(port: &str) -> Option<(u8, u8)> {
    let (bus, dev) = port.strip_prefix("usb:")?.split_once(',')?;
    Some((bus.trim().parse().ok()?, dev.trim().parse().ok()?))
}

/// The libusb-style bus number for a device, matching how libgphoto2 formats the
/// port. nusb exposes this per-platform: `location_id >> 24` on macOS (IOKit),
/// `busnum` on Linux (sysfs) — the same values libusb derives.
fn device_bus(d: &nusb::DeviceInfo) -> u8 {
    #[cfg(target_os = "macos")]
    {
        (d.location_id() >> 24) as u8
    }
    #[cfg(target_os = "linux")]
    {
        d.busnum()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = d;
        0
    }
}

/// Maps `(bus number, device address)` → `(USB vendor id, USB product string)`
/// for every connected USB device via nusb (IOKit on macOS, sysfs on Linux — the
/// same numbering libgphoto2's libusb uses). The product string is the device's
/// own model name, reliable even when libgphoto2 only knows the body generically.
/// Empty on enumeration failure (then no dedup key is emitted).
fn usb_camera_map() -> HashMap<(u8, u8), (u16, Option<String>)> {
    use nusb::MaybeFuture;
    match nusb::list_devices().wait() {
        Ok(devs) => devs
            .map(|d| {
                let key = (device_bus(&d), d.device_address());
                (key, (d.vendor_id(), d.product_string().map(str::to_owned)))
            })
            .collect(),
        Err(_) => HashMap::new(),
    }
}

/// Sony's USB vendor id. The dedicated Sony CrSDK backend enumerates bodies by
/// their ILCE/ILME/ZV model number (e.g. "ILCE-7M4"), which is exactly the USB
/// product string. libgphoto2 instead reports a marketing name ("Sony Alpha-A7 IV
/// (PC Control)") that normalizes to a different token, so for Sony bodies the
/// dedup key must be built from the USB product string too — otherwise the two
/// backends never agree and the camera shows up twice.
const USB_VENDOR_SONY: u16 = 0x054c;

/// The cross-backend dedup key for a gphoto2 device: its real USB vendor (from
/// nusb) plus its model. The model is libgphoto2's own name, except when that is
/// a generic PTP/MTP class name, or the body is a Sony (see [`USB_VENDOR_SONY`]) —
/// then the USB product string is used so the key matches the one a dedicated SDK
/// backend emits for the same body. `None` when the USB device can't be resolved
/// (non-USB port, enumeration blocked), in which case it is simply never deduped.
///
/// Note: this backend knows nothing about Canon/Nikon/Sony backends — it only
/// publishes its device's identity. The server decides which backend wins.
fn gphoto_dedup_key(
    gphoto_model: &str,
    port: &str,
    usb: &HashMap<(u8, u8), (u16, Option<String>)>,
) -> Option<String> {
    let (vendor, product) = parse_usb_port(port).and_then(|key| usb.get(&key))?;
    // Prefer the USB product string when libgphoto2 only knows the body under a
    // generic class name, or for Sony bodies (whose CrSDK keys off the model
    // number == the product string, not libgphoto2's marketing name). Fall back
    // to libgphoto2's own name if no product string is available.
    let prefer_product = is_generic_ptp_name(gphoto_model) || *vendor == USB_VENDOR_SONY;
    let model = if prefer_product {
        product.as_deref().filter(|s| !s.is_empty()).unwrap_or(gphoto_model)
    } else {
        gphoto_model
    };
    Some(crate::camera::dedup_key(*vendor, model))
}

impl CameraBackend for GPhoto2Backend {
    fn backend_id(&self) -> &str {
        "gphoto2"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let cameras = self.context.list_cameras().wait().map_err(map_err)?;
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        let usb = usb_camera_map();

        if crate::camera::dedup_debug_enabled() {
            eprintln!("[dedup] nusb sees {} USB device(s): {usb:?}", usb.len());
        }

        // List every camera and tag it with its cross-backend identity. The
        // server drops duplicates a higher-priority (SDK) backend also serves.
        let devices = cameras
            .map(|d| {
                let dedup_key = gphoto_dedup_key(&d.model, &d.port, &usb);
                if crate::camera::dedup_debug_enabled() {
                    eprintln!(
                        "[dedup] gphoto2 model={:?} port={:?} -> key={dedup_key:?}",
                        d.model, d.port
                    );
                }
                DeviceInfo {
                    connected: connected.contains_key(&d.port),
                    dedup_key,
                    id: DeviceId::new("gphoto2", &d.port).encode(),
                    name: d.model,
                }
            })
            .collect();

        Ok(devices)
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        // Idempotent: already connected → no-op.
        if self.is_connected(native_id) {
            return Ok(());
        }

        // gphoto2 does not let us open a camera by raw port string — we need
        // the matching `CameraDescriptor` from `list_cameras`.
        let descriptor = self
            .context
            .list_cameras()
            .wait()
            .map_err(map_err)?
            .find(|d| d.port == native_id)
            .ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))?;

        let camera = self
            .context
            .get_camera(&descriptor)
            .wait()
            .map_err(map_err)?;

        // A background keep-alive thread (see `spawn_keepalive`) pings this
        // camera periodically to stop the body from sleeping mid-session.
        let mut connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        connected.insert(native_id.to_string(), camera);
        Ok(())
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let mut connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        // Drop runs cleanup automatically; no explicit close call needed.
        connected.remove(native_id);
        Ok(())
    }

    fn is_connected(&self, native_id: &str) -> bool {
        let connected = self.connected.lock().expect("gphoto2 mutex poisoned");
        connected.contains_key(native_id)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let camera = self.camera_for(native_id)?;
        let root = camera.config().wait().map_err(map_err)?;

        // Each parameter is tagged with the config key it came from, so the dedup
        // below can keep the key `set_parameter` will write to.
        let mut tagged = Vec::new();
        for child in root.children_iter() {
            walk_widget(&child, &mut tagged);
        }

        // One entry per parameter type, keeping the key `CONFIG_KEYS` prefers.
        let mut params = dedup_by_type(tagged);

        // The `autoiso` toggle and the ISO list are separate widgets, so the ISO
        // selector can only learn it is read-only once both have been walked.
        reconcile_iso_auto(&mut params);

        // A select/range_select with fewer than two options offers no real choice
        // (e.g. exposure compensation in Manual mode, which the body reports as a
        // single "0"). Hide those so the UI only shows controls the user can act on.
        params.retain(|p| match p {
            CameraParameter::Select { options, .. }
            | CameraParameter::RangeSelect { options, .. } => options.len() >= 2,
            _ => true,
        });

        // A body can advertise a focus-drive widget it cannot honour, so only offer
        // the control once the body has proven it works (probed once, then cached).
        if params.iter().any(|p| type_of(p) == ParameterType::Focus)
            && !self.focus_drive_works(native_id, &camera)
        {
            params.retain(|p| type_of(p) != ParameterType::Focus);
        }

        Ok(params)
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let camera = self.camera_for(native_id)?;
        // capture_preview() — fast, low-res preview frame for live view streaming.
        // Different from capture_image(): no card write, no shutter actuation,
        // no DirItemRequestTransfer event. Designed to be called continuously.
        let preview = camera.capture_preview().wait().map_err(map_err)?;
        let data = preview.get_data(&self.context).wait().map_err(map_err)?;
        Ok(data.into_vec())
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let camera = self.camera_for(native_id)?;

        // capture_image() triggers the shutter and returns a path on the camera
        // (either internal RAM or SD card, depending on the `capturetarget`
        // config). The file's format follows the camera's image-quality
        // setting — make sure the camera is in JPEG mode for now (the HTTP
        // response is hardcoded to image/jpeg upstream).
        let path = camera.capture_image().wait().map_err(map_err)?;

        // Download into memory (no on-disk intermediate) via the in-memory
        // variant of `CameraFS::download`. The underlying gphoto2 call is
        // gp_camera_file_get(GP_FILE_TYPE_NORMAL).
        let file = camera
            .fs()
            .download(&path.folder(), &path.name())
            .wait()
            .map_err(map_err)?;

        let data = file.get_data(&self.context).wait().map_err(map_err)?;
        Ok(data.into_vec())
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let camera = self.camera_for(native_id)?;

        // IsoAuto is synthetic: some bodies back it with a dedicated toggle, others
        // with an "Auto" entry in the ISO list. See `set_iso_auto`.
        if param_type == ParameterType::IsoAuto {
            return set_iso_auto(&camera, is_truthy(value));
        }

        // FocusAuto is a synthetic toggle backed by the camera's "focusmode" radio
        // widget (there is no separate AF/MF boolean — MF is one focusmode value),
        // mirroring the IsoAuto handling above. true → first AF choice; false →
        // the manual choice, both queried live from the camera.
        if param_type == ParameterType::FocusAuto {
            let widget: RadioWidget = widget_for(&camera, ParameterType::FocusMode)
                .ok_or(CameraError::NotSupported)?;
            let on = is_truthy(value);
            let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
            let target = if on {
                choices.iter().find(|c| !is_manual_focus_choice(c))
            } else {
                choices.iter().find(|c| is_manual_focus_choice(c))
            }
            .ok_or(CameraError::NotSupported)?;
            widget.set_choice(target.as_str()).map_err(map_err)?;
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }

        // Focus is the one parameter a body can advertise and still be unable to
        // execute, so it gets its own path (which learns from a refusal).
        if param_type == ParameterType::Focus {
            return self.drive_focus(native_id, &camera, value);
        }

        // gphoto2 exposes the same logical parameter as Radio on most cameras but
        // as Range or Toggle on some (focus drive is a Range on Nikon, a Radio on
        // Canon), so probe each widget type over every key the parameter is known by.
        if let Some(widget) = widget_for::<RadioWidget>(&camera, param_type) {
            // An on/off radio is exposed as a Boolean, so clients send "true" /
            // "false" — translate those back to the camera's own On/Off choice.
            // Any other parameter round-trips the option value verbatim.
            let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
            let choice = on_off_choice(&choices, is_truthy(value))
                .cloned()
                .unwrap_or_else(|| value.to_string());
            widget.set_choice(choice.as_str()).map_err(map_err)?;
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }
        if let Some(widget) = widget_for::<RangeWidget>(&camera, param_type) {
            let v: f32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            widget.set_value(v);
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }
        if let Some(widget) = widget_for::<ToggleWidget>(&camera, param_type) {
            widget.set_toggled(is_truthy(value));
            camera.set_config(&widget).wait().map_err(map_err)?;
            return Ok(());
        }

        Err(CameraError::NotSupported)
    }
}

/// Turns auto-ISO on or off, whichever way the body models it.
///
/// Two families: Nikon keeps its ISO list concrete and exposes auto-ISO as its
/// own `autoiso` toggle widget; Canon folds an "Auto" choice into the ISO list.
/// Prefer the dedicated toggle when the body has one, otherwise fall back to
/// selecting the auto choice (turning auto off then means selecting the first
/// concrete ISO the camera offers, rather than hardcoding a value it may not have).
fn set_iso_auto(camera: &gphoto2::Camera, on: bool) -> Result<(), CameraError> {
    // A dedicated auto-ISO widget: a toggle on some drivers, an "On"/"Off" radio
    // on others — libgphoto2 models the same PTP on/off property either way.
    if let Some(toggle) = widget_for::<ToggleWidget>(camera, ParameterType::IsoAuto) {
        toggle.set_toggled(on);
        camera.set_config(&toggle).wait().map_err(map_err)?;
        return Ok(());
    }
    if let Some(radio) = widget_for::<RadioWidget>(camera, ParameterType::IsoAuto) {
        let choices: Vec<String> = radio.choices_iter().map(|c| c.to_string()).collect();
        let target = on_off_choice(&choices, on).ok_or(CameraError::NotSupported)?;
        radio.set_choice(target.as_str()).map_err(map_err)?;
        camera.set_config(&radio).wait().map_err(map_err)?;
        return Ok(());
    }

    let widget: RadioWidget =
        widget_for(camera, ParameterType::Iso).ok_or(CameraError::NotSupported)?;
    let choices: Vec<String> = widget.choices_iter().map(|c| c.to_string()).collect();
    let target = if on {
        auto_iso_choice(&choices).cloned()
    } else {
        choices.iter().find(|c| is_concrete_iso(c)).cloned()
    }
    .ok_or(CameraError::NotSupported)?;

    widget.set_choice(target.as_str()).map_err(map_err)?;
    camera.set_config(&widget).wait().map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Widget tree walking → CameraParameter list
// ---------------------------------------------------------------------------

/// Walks the config tree, tagging every parameter with the config key it was read
/// from — `dedup_by_type` needs it to keep the key writes will go to.
fn walk_widget(widget: &Widget, out: &mut Vec<(String, CameraParameter)>) {
    if let Widget::Group(g) = widget {
        for child in g.children_iter() {
            walk_widget(&child, out);
        }
        return;
    }

    let key = widget.name();
    let mut leaf = Vec::new();
    walk_leaf(widget, &mut leaf);
    out.extend(leaf.into_iter().map(|param| (key.clone(), param)));
}

fn walk_leaf(widget: &Widget, out: &mut Vec<CameraParameter>) {
    match widget {
        Widget::Radio(r) => {
            if r.readonly() {
                return;
            }
            let Some(pt) = param_type_for(&r.name()) else {
                return;
            };
            let current = r.choice();
            let choices: Vec<String> = r.choices_iter().map(|c| c.to_string()).collect();

            // An on/off property the driver models as a two-choice radio rather
            // than a toggle (Nikon's `autoiso`) is still a Boolean to our clients.
            if on_off_choice(&choices, true).is_some() {
                out.push(CameraParameter::Boolean {
                    param_type: pt,
                    current: is_on_choice(&current),
                    disabled: false,
                });
                return;
            }

            match pt {
                // ISO is split into an IsoAuto toggle + an Iso selector, mirroring
                // the Canon backend. libgphoto2 localizes the auto label (e.g.
                // "Automatique" in French), so we detect it locale-independently:
                // the auto choice is the only non-numeric one — every real ISO
                // value parses as an integer.
                ParameterType::Iso => {
                    let auto = auto_iso_choice(&choices).cloned();
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|c| Some(*c) != auto.as_ref())
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    if options.is_empty() {
                        return;
                    }
                    // Only this body's own ISO list can say whether auto is on. When
                    // it holds no auto entry (Nikon), auto-ISO lives in a separate
                    // `autoiso` toggle widget walked on its own — leave IsoAuto to it
                    // and let `reconcile_iso_auto` disable the selector afterwards.
                    let iso_auto = match &auto {
                        Some(auto) => {
                            let on = *auto == current;
                            out.push(CameraParameter::Boolean {
                                param_type: ParameterType::IsoAuto,
                                current: on,
                                disabled: false,
                            });
                            on
                        }
                        None => false,
                    };
                    // When auto is on the concrete ISO is read-only; show the first
                    // value as a placeholder and disable the control.
                    let iso_current = if iso_auto {
                        options[0].value.clone()
                    } else {
                        current
                    };
                    out.push(CameraParameter::RangeSelect {
                        param_type: ParameterType::Iso,
                        current: iso_current,
                        options,
                        disabled: iso_auto,
                    });
                }
                // Shutter speed: drop the bulb entry — it is a long-exposure mode,
                // not a discrete speed. Its label is localized too ("pose longue"
                // in French), so we exclude any choice with no digit; every real
                // speed has one ("30", "1/60", "0.5"…), bulb does not.
                ParameterType::ShutterSpeed => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|&c| is_real_shutter_speed(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(CameraParameter::RangeSelect {
                        param_type: ParameterType::ShutterSpeed,
                        current,
                        options,
                        disabled: false,
                    });
                }
                // Focus mode: split into a FocusAuto toggle + an AF-only FocusMode
                // select, mirroring the ISO split (and the Nikon backend). The
                // manual choice ("Manual"/"MF"…) maps to FocusAuto=false; the AF
                // sub-modes (AF-S/AF-C/…) stay in the select, disabled in MF. The
                // manual jog itself is a separate "manualfocusdrive" widget (→
                // Focus, handled by the Range arm below).
                ParameterType::FocusMode => {
                    let focus_auto = !is_manual_focus_choice(&current);
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|c| !is_manual_focus_choice(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(CameraParameter::Boolean {
                        param_type: ParameterType::FocusAuto,
                        current: focus_auto,
                        disabled: false,
                    });
                    if !options.is_empty() {
                        // In MF the current isn't an AF mode; show the first as a
                        // disabled-time placeholder.
                        let mode_current = if focus_auto {
                            current
                        } else {
                            options[0].value.clone()
                        };
                        out.push(CameraParameter::Select {
                            param_type: ParameterType::FocusMode,
                            current: mode_current,
                            options,
                            disabled: !focus_auto,
                        });
                    }
                }
                // Image quality: the server only ever returns JPEG (capture is
                // hardcoded to image/jpeg), so hide RAW / RAW+JPEG formats —
                // selecting one would break capture.
                ParameterType::ImageQuality => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .filter(|&c| is_jpeg_format(c))
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(CameraParameter::Select {
                        param_type: ParameterType::ImageQuality,
                        current,
                        options,
                        disabled: false,
                    });
                }
                _ => {
                    let options: Vec<ParameterOption> = choices
                        .iter()
                        .map(|c| ParameterOption { label: c.clone(), value: c.clone() })
                        .collect();
                    out.push(if is_ordered(pt) {
                        CameraParameter::RangeSelect {
                            param_type: pt,
                            current,
                            options,
                            disabled: false,
                        }
                    } else {
                        CameraParameter::Select {
                            param_type: pt,
                            current,
                            options,
                            disabled: false,
                        }
                    });
                }
            }
        }
        Widget::Range(r) => {
            if r.readonly() {
                return;
            }
            if let Some(pt) = param_type_for(&r.name()) {
                let (range, step) = r.range_and_step();
                out.push(CameraParameter::Range {
                    param_type: pt,
                    current: r.value() as i32,
                    min: *range.start() as i32,
                    max: *range.end() as i32,
                    step: step as i32,
                    disabled: false,
                });
            }
        }
        // A toggle is a plain on/off widget (e.g. Nikon's `autoiso`) → Boolean.
        Widget::Toggle(t) => {
            if t.readonly() {
                return;
            }
            if let Some(pt) = param_type_for(&t.name()) {
                out.push(CameraParameter::Boolean {
                    param_type: pt,
                    current: t.toggled().unwrap_or(false),
                    disabled: false,
                });
            }
        }
        _ => {} // Group (walked above), Button, Date, Text → not parameters
    }
}

// ---------------------------------------------------------------------------
// Mapping between gphoto2 config-key names and our ParameterType enum.
//
// Cameras name the same logical parameter differently depending on the camlib
// (Nikon exposes aperture as "f-number", Canon as "aperture"), so several keys
// can map to one type. This table is the single source of truth for both
// directions: reads look a key up with `param_type_for`, writes probe every key
// a type is known by with `config_keys_for`. Extend it as you observe new names.
// ---------------------------------------------------------------------------

const CONFIG_KEYS: &[(&str, ParameterType)] = &[
    ("iso", ParameterType::Iso),
    ("isospeed", ParameterType::Iso),
    ("iso speed", ParameterType::Iso),
    ("iso_speed", ParameterType::Iso),
    // Nikon bodies keep the ISO list concrete and expose auto-ISO as its own
    // toggle widget; Canon folds an "Auto" choice into the ISO list instead
    // (handled in `walk_widget` / `set_iso_auto`).
    ("autoiso", ParameterType::IsoAuto),
    ("isoauto", ParameterType::IsoAuto),
    ("iso_auto", ParameterType::IsoAuto),
    ("shutterspeed", ParameterType::ShutterSpeed),
    ("shutterspeed2", ParameterType::ShutterSpeed),
    ("shutter_speed", ParameterType::ShutterSpeed),
    ("shutter speed", ParameterType::ShutterSpeed),
    ("aperture", ParameterType::Aperture),
    ("f-number", ParameterType::Aperture),
    ("f_number", ParameterType::Aperture),
    ("fnumber", ParameterType::Aperture),
    ("whitebalance", ParameterType::WhiteBalance),
    ("white_balance", ParameterType::WhiteBalance),
    ("white balance", ParameterType::WhiteBalance),
    ("colortemperature", ParameterType::ColorTemperature),
    ("color_temperature", ParameterType::ColorTemperature),
    ("exposurecompensation", ParameterType::ExposureCompensation),
    ("exposure_compensation", ParameterType::ExposureCompensation),
    ("imageformat", ParameterType::ImageQuality),
    ("image_format", ParameterType::ImageQuality),
    ("imagequality", ParameterType::ImageQuality),
    ("image_quality", ParameterType::ImageQuality),
    // Focus mode (AF-S/AF-C/Manual…) → split into FocusAuto + FocusMode (see
    // walk_widget). "manualfocusdrive" is the relative manual-focus jog → Focus.
    ("focusmode", ParameterType::FocusMode),
    ("focusmode2", ParameterType::FocusMode),
    ("focus_mode", ParameterType::FocusMode),
    ("manualfocusdrive", ParameterType::Focus),
    ("manual_focus_drive", ParameterType::Focus),
];

fn param_type_for(name: &str) -> Option<ParameterType> {
    let name = name.to_ascii_lowercase();
    CONFIG_KEYS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, pt)| *pt)
}

/// Every config key `param_type` is known by, in preference order.
fn config_keys_for(param_type: ParameterType) -> impl Iterator<Item = &'static str> {
    CONFIG_KEYS
        .iter()
        .filter(move |(_, pt)| *pt == param_type)
        .map(|(key, _)| *key)
}

/// Fetches the camera's widget for `param_type` as `W`, trying the type's config
/// keys in `CONFIG_KEYS` order. `None` when the body exposes none of them, or
/// exposes one but not as a `W`.
///
/// A body may expose several keys of the same type, and they need not agree — see
/// `dedup_by_type`, which resolves the read path against this same order so that a
/// write always lands on the widget the client is looking at.
fn widget_for<W>(camera: &gphoto2::Camera, param_type: ParameterType) -> Option<W>
where
    W: TryFrom<Widget, Error = gphoto2::Error> + 'static + Send,
{
    config_keys_for(param_type).find_map(|key| camera.config_key::<W>(key).wait().ok())
}

/// Boolean values arrive as strings from the API (`"true"` / `"false"`), but be
/// lenient about what clients send.
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Whether a radio choice denotes the "on" / "off" state of an on/off property.
/// libgphoto2 spells them "On"/"Off" (English under the `LC_ALL=C` we force in
/// `new()`), some drivers "1"/"0".
fn is_on_choice(choice: &str) -> bool {
    matches!(choice.trim().to_ascii_lowercase().as_str(), "on" | "1")
}

fn is_off_choice(choice: &str) -> bool {
    matches!(choice.trim().to_ascii_lowercase().as_str(), "off" | "0")
}

/// The choice standing for `on` in an on/off radio, or `None` when `choices` is
/// not such a pair. libgphoto2 models the same PTP on/off property as a toggle
/// widget on some drivers and as a two-choice radio on others, so both shapes
/// have to be recognised and mapped onto our `Boolean` parameter kind.
fn on_off_choice(choices: &[String], on: bool) -> Option<&String> {
    let is_on_off_pair = choices.len() == 2
        && choices.iter().any(|c| is_on_choice(c))
        && choices.iter().any(|c| is_off_choice(c));
    if !is_on_off_pair {
        return None;
    }
    choices
        .iter()
        .find(|c| if on { is_on_choice(c) } else { is_off_choice(c) })
}

/// True when a gphoto2 `focusmode` choice denotes manual focus — the one mode
/// that is NOT autofocus. The label is localized by libgphoto2 (LC_ALL=C is set
/// in `new()`, but be defensive), so match "manual"/"mf" case-insensitively.
/// Mirrors the Nikon backend's `is_manual_focus`.
fn is_manual_focus_choice(choice: &str) -> bool {
    let c = choice.trim().to_ascii_lowercase();
    c == "mf" || c.contains("manual")
}

// ---------------------------------------------------------------------------
// Post-processing of the walked parameter list
// ---------------------------------------------------------------------------

fn type_of(param: &CameraParameter) -> ParameterType {
    match param {
        CameraParameter::Boolean { param_type, .. }
        | CameraParameter::Range { param_type, .. }
        | CameraParameter::Select { param_type, .. }
        | CameraParameter::RangeSelect { param_type, .. } => *param_type,
    }
}

/// Reduces the walked `(config key, parameter)` pairs to one parameter per type,
/// keeping the one read from the key `CONFIG_KEYS` ranks first — the same key
/// `widget_for` will write to.
///
/// A body can expose the same logical parameter under two keys, and they need not
/// behave the same. The Nikon Z5 II has *both* auto-ISO spellings, as two distinct
/// PTP properties that disagree: `autoiso` (`ISOAuto`) writes and reads back fine,
/// while `isoauto` (`ISO_Auto`) is stuck on "On" and rejects every write with "Bad
/// parameters". Keeping whichever came first in the camera's config tree meant
/// reading `isoauto` while writing `autoiso`: turning auto-ISO off silently
/// succeeded on one property and the next refresh re-read the other, still "On".
/// Nikon's `focusmode` / `focusmode2` pair is resolved the same way.
///
/// A parameter the walk synthesized rather than read from a key of its own (the
/// IsoAuto folded into a Canon ISO list) ranks last, so a body that also has a
/// dedicated widget for it wins — that widget is the one a write can reach.
fn dedup_by_type(tagged: Vec<(String, CameraParameter)>) -> Vec<CameraParameter> {
    let rank = |key: &str, param_type: ParameterType| {
        CONFIG_KEYS
            .iter()
            .position(|(k, pt)| *k == key && *pt == param_type)
            .unwrap_or(usize::MAX)
    };

    let mut best: HashMap<ParameterType, usize> = HashMap::new();
    for (key, param) in &tagged {
        let param_type = type_of(param);
        let r = rank(key, param_type);
        best.entry(param_type).and_modify(|b| *b = (*b).min(r)).or_insert(r);
    }

    let mut kept: HashSet<ParameterType> = HashSet::new();
    tagged
        .into_iter()
        .filter(|(key, param)| {
            let param_type = type_of(param);
            rank(key, param_type) == best[&param_type] && kept.insert(param_type)
        })
        .map(|(_, param)| param)
        .collect()
}

/// Marks the ISO selector read-only while auto-ISO is on. Needed for the bodies
/// that back IsoAuto with a standalone toggle widget (Nikon): the ISO list is a
/// different widget and cannot know about it on its own. A no-op for the bodies
/// whose ISO list carries its own "Auto" choice — `walk_widget` already set the
/// flag there.
fn reconcile_iso_auto(params: &mut [CameraParameter]) {
    let auto_on = params.iter().any(|p| {
        matches!(
            p,
            CameraParameter::Boolean {
                param_type: ParameterType::IsoAuto,
                current: true,
                ..
            }
        )
    });
    if !auto_on {
        return;
    }
    for param in params.iter_mut() {
        if let CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            disabled,
            ..
        } = param
        {
            *disabled = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Value classification — locale-independent, since libgphoto2 localizes labels.
// ---------------------------------------------------------------------------

/// The "Auto" entry of an ISO choice list, if the body has one (Canon does,
/// Nikon does not — it has a separate `autoiso` toggle instead).
///
/// libgphoto2 localizes the label ("Auto" / "Automatique"…), so we identify it
/// by shape: every real ISO is a plain integer, so the auto entry is the only
/// non-numeric choice. Bodies that list extended values ("Hi 1", "Lo 0.3") have
/// several non-numeric choices and no auto entry — then none of them is auto.
fn auto_iso_choice(choices: &[String]) -> Option<&String> {
    let mut non_numeric = choices.iter().filter(|c| !is_concrete_iso(c));
    let candidate = non_numeric.next()?;
    non_numeric.next().is_none().then_some(candidate)
}

/// A concrete (non-auto) ISO value: a plain integer, as every real ISO is.
fn is_concrete_iso(choice: &str) -> bool {
    choice.parse::<u32>().is_ok()
}

/// A real shutter speed. The bulb entry is localized too ("Bulb" / "pose
/// longue"), but unlike every real speed ("30", "1/60", "0.5") it has no digit.
fn is_real_shutter_speed(choice: &str) -> bool {
    choice.chars().any(|c| c.is_ascii_digit())
}

/// A JPEG image-quality choice. Capture is hardcoded to image/jpeg, so every mode
/// that writes a raw file — alone or alongside a JPEG — is hidden: selecting one
/// would break capture.
///
/// Matching on "RAW" alone is not enough. Vendors name the format after their own
/// extension and only sometimes spell out "raw": the Nikon Z5 II offers "NEF
/// (Raw)" but also "NEF+Basic" and "NEF+Fine", which are RAW+JPEG modes with no
/// "raw" in the label. So match the raw extensions themselves.
///
/// Choices the driver could not decode ("Unknown value 0003") are hidden too: they
/// may well be raw modes, and there is no way to tell — offering one would be a
/// coin flip on whether capture still works.
fn is_jpeg_format(choice: &str) -> bool {
    const RAW_FORMATS: &[&str] = &[
        "RAW", // generic, and Canon's cRAW
        "NEF", "NRW", // Nikon
        "CR2", "CR3", "CRW", // Canon
        "ARW", "SR2", // Sony
        "ORF", // Olympus / OM System
        "RW2", // Panasonic
        "RAF", // Fujifilm
        "PEF", "DNG", // Pentax
    ];
    let upper = choice.to_uppercase();
    !RAW_FORMATS.iter().any(|raw| upper.contains(raw)) && !upper.starts_with("UNKNOWN VALUE")
}

/// Parameters whose values form an ordered numeric progression (ISO, aperture,
/// shutter speed, exposure compensation) should render as `RangeSelect` so the
/// UI knows the order is meaningful.
fn is_ordered(pt: ParameterType) -> bool {
    matches!(
        pt,
        ParameterType::Iso
            | ParameterType::Aperture
            | ParameterType::ShutterSpeed
            | ParameterType::ExposureCompensation
            | ParameterType::ColorTemperature
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_config_keys_and_ignores_unknown() {
        assert_eq!(param_type_for("iso"), Some(ParameterType::Iso));
        assert_eq!(param_type_for("autoiso"), Some(ParameterType::IsoAuto));
        assert_eq!(param_type_for("shutterspeed"), Some(ParameterType::ShutterSpeed));
        assert_eq!(param_type_for("aperture"), Some(ParameterType::Aperture));
        assert_eq!(param_type_for("imageformat"), Some(ParameterType::ImageQuality));
        assert_eq!(param_type_for("focusmode"), Some(ParameterType::FocusMode));
        assert_eq!(param_type_for("manualfocusdrive"), Some(ParameterType::Focus));
        assert_eq!(param_type_for("somethingelse"), None);
    }

    /// The Nikon spellings must be writable, not just readable: `set_parameter`
    /// looks the widget up by every key of the type, and Nikon names aperture
    /// "f-number" (issue #28 — "Failed to set Aperture: unknown parameter type").
    #[test]
    fn vendor_key_aliases_are_reachable_from_the_param_type() {
        let keys: Vec<&str> = config_keys_for(ParameterType::Aperture).collect();
        assert!(keys.contains(&"aperture")); // Canon
        assert!(keys.contains(&"f-number")); // Nikon
    }

    #[test]
    fn config_keys_round_trip_through_param_type_for() {
        for (key, pt) in CONFIG_KEYS {
            assert_eq!(param_type_for(key), Some(*pt), "key {key:?} should round-trip");
            assert!(
                config_keys_for(*pt).any(|k| k == *key),
                "key {key:?} should be probed for {pt:?}"
            );
        }
    }

    #[test]
    fn every_settable_param_type_has_at_least_one_key() {
        for pt in [
            ParameterType::Iso,
            ParameterType::IsoAuto,
            ParameterType::ShutterSpeed,
            ParameterType::Aperture,
            ParameterType::WhiteBalance,
            ParameterType::ColorTemperature,
            ParameterType::ExposureCompensation,
            ParameterType::ImageQuality,
            ParameterType::FocusMode,
            ParameterType::Focus,
        ] {
            assert!(config_keys_for(pt).next().is_some(), "{pt:?} has no config key");
        }
    }

    #[test]
    fn truthy_values() {
        assert!(is_truthy("true"));
        assert!(is_truthy("True"));
        assert!(is_truthy("1"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy("0"));
    }

    #[test]
    fn manual_focus_choice_detection() {
        assert!(is_manual_focus_choice("Manual"));
        assert!(is_manual_focus_choice("MF"));
        assert!(is_manual_focus_choice("Manual focus"));
        assert!(!is_manual_focus_choice("AF-S"));
        assert!(!is_manual_focus_choice("AF-C"));
        assert!(!is_manual_focus_choice("AI Servo"));
    }

    #[test]
    fn iso_auto_is_the_only_non_numeric_choice() {
        assert!(is_concrete_iso("100"));
        assert!(is_concrete_iso("6400"));
        assert!(!is_concrete_iso("Auto"));
        assert!(!is_concrete_iso("Automatique")); // localized label still detected
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    /// Drivers model an on/off property either as a toggle or as an On/Off radio
    /// (Nikon's `autoiso`); both must map onto our Boolean parameter kind.
    #[test]
    fn on_off_radio_pairs_map_to_booleans() {
        let on_off = strings(&["On", "Off"]);
        assert_eq!(on_off_choice(&on_off, true), Some(&"On".to_string()));
        assert_eq!(on_off_choice(&on_off, false), Some(&"Off".to_string()));

        let numeric = strings(&["0", "1"]);
        assert_eq!(on_off_choice(&numeric, true), Some(&"1".to_string()));

        // A real multi-choice list is not a boolean, even one holding "off".
        let wb = strings(&["Auto", "Daylight", "Off"]);
        assert_eq!(on_off_choice(&wb, true), None);
        let iso = strings(&["100", "200"]);
        assert_eq!(on_off_choice(&iso, true), None);
    }

    #[test]
    fn auto_iso_choice_detection() {
        // Canon-style list: the auto entry is the only non-numeric choice, whatever
        // libgphoto2 localized it to.
        let canon = strings(&["Auto", "100", "200", "6400"]);
        assert_eq!(auto_iso_choice(&canon), Some(&"Auto".to_string()));
        let localized = strings(&["Automatique", "100", "200"]);
        assert_eq!(auto_iso_choice(&localized), Some(&"Automatique".to_string()));

        // Nikon-style list: concrete values only — auto-ISO is a separate toggle.
        let nikon = strings(&["100", "200", "25600"]);
        assert_eq!(auto_iso_choice(&nikon), None);

        // Extended values are not an auto entry: several non-numeric choices → none.
        let extended = strings(&["100", "25600", "Hi 1", "Hi 2"]);
        assert_eq!(auto_iso_choice(&extended), None);
    }

    /// Bodies that expose auto-ISO as a standalone toggle (Nikon) walk it as a
    /// separate widget, so the ISO selector only learns it is read-only here.
    #[test]
    fn iso_selector_is_disabled_while_the_auto_iso_toggle_is_on() {
        let iso = || CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            current: "100".into(),
            options: vec![ParameterOption { label: "100".into(), value: "100".into() }],
            disabled: false,
        };
        let auto = |current| CameraParameter::Boolean {
            param_type: ParameterType::IsoAuto,
            current,
            disabled: false,
        };

        let mut on = vec![iso(), auto(true)];
        reconcile_iso_auto(&mut on);
        assert!(matches!(on[0], CameraParameter::RangeSelect { disabled: true, .. }));

        let mut off = vec![iso(), auto(false)];
        reconcile_iso_auto(&mut off);
        assert!(matches!(off[0], CameraParameter::RangeSelect { disabled: false, .. }));
    }

    /// Nikon exposes both `focusmode` and `focusmode2`; only one control should
    /// reach the client, and it must be the key `CONFIG_KEYS` ranks first — the one
    /// `set_parameter` writes to.
    #[test]
    fn duplicate_parameter_types_are_dropped() {
        let mode = |key: &str, current: &str| {
            (
                key.to_string(),
                CameraParameter::Select {
                    param_type: ParameterType::FocusMode,
                    current: current.into(),
                    options: vec![],
                    disabled: false,
                },
            )
        };
        // Tree order puts `focusmode2` first; `CONFIG_KEYS` prefers `focusmode`.
        let params = dedup_by_type(vec![
            mode("focusmode2", "AF-C"),
            mode("focusmode", "AF-S"),
            ("iso".to_string(), iso_param()),
        ]);
        assert_eq!(params.len(), 2);
        assert!(params
            .iter()
            .any(|p| matches!(p, CameraParameter::Select { current, .. } if current == "AF-S")));
    }

    /// The Nikon Z5 II exposes two auto-ISO properties that disagree: `autoiso` is
    /// the writable one, `isoauto` is stuck on. Reading the stuck one while writing
    /// the other made "turn auto-ISO off" look like a no-op, so the read must land
    /// on the key `CONFIG_KEYS` prefers regardless of the camera's tree order.
    #[test]
    fn auto_iso_read_follows_the_key_writes_go_to() {
        let toggle = |key: &str, current: bool| {
            (
                key.to_string(),
                CameraParameter::Boolean {
                    param_type: ParameterType::IsoAuto,
                    current,
                    disabled: false,
                },
            )
        };
        let params = dedup_by_type(vec![toggle("isoauto", true), toggle("autoiso", false)]);
        assert_eq!(params.len(), 1);
        assert!(matches!(params[0], CameraParameter::Boolean { current: false, .. }));
    }

    /// A parameter the walk synthesized from another widget's choices (Canon folds
    /// "Auto" into its ISO list) has no key of its own, so it must not outrank a
    /// body's dedicated widget — but it must still survive when it stands alone.
    #[test]
    fn folded_iso_auto_is_kept_when_it_is_the_only_one() {
        let folded = (
            "iso".to_string(),
            CameraParameter::Boolean {
                param_type: ParameterType::IsoAuto,
                current: true,
                disabled: false,
            },
        );
        let params = dedup_by_type(vec![folded]);
        assert_eq!(params.len(), 1);
        assert!(matches!(params[0], CameraParameter::Boolean { current: true, .. }));
    }

    fn iso_param() -> CameraParameter {
        CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            current: "100".into(),
            options: vec![],
            disabled: false,
        }
    }

    #[test]
    fn bulb_is_the_digitless_shutter_choice() {
        assert!(is_real_shutter_speed("30"));
        assert!(is_real_shutter_speed("1/4000"));
        assert!(is_real_shutter_speed("0.5"));
        assert!(!is_real_shutter_speed("Bulb"));
        assert!(!is_real_shutter_speed("pose longue")); // localized label still detected
    }

    #[test]
    fn raw_formats_are_filtered_out() {
        assert!(is_jpeg_format("L"));
        assert!(is_jpeg_format("cL"));
        assert!(is_jpeg_format("S2"));
        assert!(!is_jpeg_format("RAW"));
        assert!(!is_jpeg_format("RAW + L"));
        assert!(!is_jpeg_format("cRAW")); // compact RAW is still RAW
    }

    /// The Nikon Z5 II names its raw modes after the NEF extension, and only one of
    /// them says "Raw" — the RAW+JPEG pairs do not.
    #[test]
    fn vendor_raw_extensions_are_filtered_out() {
        assert!(is_jpeg_format("JPEG Basic"));
        assert!(is_jpeg_format("JPEG Normal"));
        assert!(is_jpeg_format("JPEG Fine"));

        assert!(!is_jpeg_format("NEF (Raw)"));
        assert!(!is_jpeg_format("NEF+Basic"));
        assert!(!is_jpeg_format("NEF+Fine"));
        assert!(!is_jpeg_format("ARW"), "Sony raw");
        assert!(!is_jpeg_format("RAF+F"), "Fujifilm raw + JPEG");
    }

    /// Choices libgphoto2 could not decode may be raw modes — never offer them.
    #[test]
    fn undecoded_choices_are_filtered_out() {
        assert!(!is_jpeg_format("Unknown value 0003"));
        assert!(!is_jpeg_format("unknown value 000a"));
    }

    #[test]
    fn ordered_covers_numeric_progressions_only() {
        assert!(is_ordered(ParameterType::Iso));
        assert!(is_ordered(ParameterType::Aperture));
        assert!(is_ordered(ParameterType::ShutterSpeed));
        assert!(!is_ordered(ParameterType::WhiteBalance));
        assert!(!is_ordered(ParameterType::ImageQuality));
    }

    #[test]
    fn usb_enumeration_runs() {
        // Smoke test: the nusb path (IOKit on macOS, sysfs on Linux) must run
        // without panicking. Contents depend on what's plugged in / sandboxing.
        let _ = usb_camera_map();
    }

    #[test]
    fn usb_port_parsing() {
        assert_eq!(parse_usb_port("usb:020,007"), Some((20, 7))); // zero-padded
        assert_eq!(parse_usb_port("usb:1,8"), Some((1, 8)));
        assert_eq!(parse_usb_port("ptpip:192.168.1.1"), None);
        assert_eq!(parse_usb_port("usb:"), None);
        assert_eq!(parse_usb_port("disk:/x"), None);
    }

    #[test]
    fn generic_ptp_name_detection() {
        assert!(is_generic_ptp_name("USB PTP Class Camera"));
        assert!(is_generic_ptp_name("PTP Camera"));
        assert!(is_generic_ptp_name("MTP Device"));
        assert!(!is_generic_ptp_name("Nikon Z 6"));
        assert!(!is_generic_ptp_name("Canon EOS 600D"));
    }

    #[test]
    fn dedup_key_uses_product_string_for_generic_names() {
        // A new Nikon body libgphoto2 only knows generically: the key comes from
        // the USB product string and must equal the one the Nikon SDK emits.
        let mut usb = HashMap::new();
        usb.insert((20u8, 7u8), (0x04B0u16, Some("Nikon Z 5_2".to_string())));
        let key = gphoto_dedup_key("USB PTP Class Camera", "usb:020,007", &usb);
        // Nikon SDK lists the same body as "Z5_2" → same key.
        assert_eq!(key, Some(crate::camera::dedup_key(0x04B0, "Z5_2")));
    }

    #[test]
    fn dedup_key_uses_gphoto_name_when_specific() {
        // A Canon EOS libgphoto2 names specifically: keep that name (its USB
        // product string is often the generic "Canon Digital Camera"). Matches
        // the EDSDK device description.
        let mut usb = HashMap::new();
        usb.insert((20u8, 7u8), (0x04A9u16, Some("Canon Digital Camera".to_string())));
        let key = gphoto_dedup_key("Canon EOS R5", "usb:020,007", &usb);
        assert_eq!(key, Some(crate::camera::dedup_key(0x04A9, "Canon EOS R5")));
    }

    #[test]
    fn dedup_key_uses_product_string_for_sony() {
        // libgphoto2 names a Sony body by its marketing name, but the Sony CrSDK
        // keys off the ILCE model number (== the USB product string). gphoto2 must
        // use the product string so both backends agree and the α7 IV isn't shown
        // twice ("Sony Alpha-A7 IV (PC Control)" + "Sony ILCE-7M4").
        let mut usb = HashMap::new();
        usb.insert((0u8, 3u8), (USB_VENDOR_SONY, Some("ILCE-7M4".to_string())));
        let key = gphoto_dedup_key("Sony Alpha-A7 IV (PC Control)", "usb:000,003", &usb);
        assert_eq!(key, Some(crate::camera::dedup_key(USB_VENDOR_SONY, "ILCE-7M4")));
    }

    #[test]
    fn dedup_key_sony_falls_back_to_gphoto_name_without_product() {
        // No USB product string available → keep libgphoto2's name (best effort).
        let mut usb = HashMap::new();
        usb.insert((0u8, 3u8), (USB_VENDOR_SONY, None));
        let key = gphoto_dedup_key("Sony Alpha-A7 IV (PC Control)", "usb:000,003", &usb);
        assert_eq!(
            key,
            Some(crate::camera::dedup_key(USB_VENDOR_SONY, "Sony Alpha-A7 IV (PC Control)"))
        );
    }

    #[test]
    fn dedup_key_none_when_usb_unresolved() {
        // No USB entry for the port → no key → the device is never deduped.
        let usb = HashMap::new();
        assert_eq!(gphoto_dedup_key("Nikon Z 5", "usb:020,007", &usb), None);
        assert_eq!(gphoto_dedup_key("Some Camera", "ptpip:1.2.3.4", &usb), None);
    }
}
