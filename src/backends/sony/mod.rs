//! Sony camera backend over the Camera Remote SDK (CrSDK).
//!
//! The CrSDK is C++ with asynchronous callbacks, so all SDK access goes through a
//! small C++ shim (`bridge.cpp`) that exposes a flat, synchronous C API. This
//! module drives that shim from a single dedicated OS thread (the "sony-sdk"
//! actor) — mirroring the Canon backend — because the SDK holds process-global
//! state and its session handles must not be touched concurrently.
//!
//! Property values cross the FFI as raw SDK integers; the human-readable labels
//! (f/5.6, 1/500, ISO 400, …) are decoded here, the same split as the Canon
//! backend's decode tables.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::camera::{
    dedup_key, CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

// ---------------------------------------------------------------------------
// C bridge constants / types — must match bridge.h
// ---------------------------------------------------------------------------

const SN_MAX_MODEL: usize = 128;
const SN_MAX_ID: usize = 256;
const SN_MAX_CONN: usize = 32;
const SN_MAX_DEVICES: usize = 32;
const SN_MAX_PARAMS: usize = 256;
const SN_MAX_OPTIONS: usize = 512;

const SN_OK: c_int = 0;
const SN_NOT_READY: c_int = 2;
/// `sn_capture` return code: autofocus could not lock, so nothing was shot.
const SN_ERR_AF: c_int = 3;

/// Sony's USB vendor id — used to build the cross-backend dedup key so the same
/// body seen by gphoto2 (PTP) is recognised as this SDK's device.
const SONY_VENDOR_ID: u16 = 0x054c;

#[repr(C)]
struct SnDeviceInfo {
    model: [c_char; SN_MAX_MODEL],
    id: [c_char; SN_MAX_ID],
    conn_type: [c_char; SN_MAX_CONN],
}

#[repr(C)]
struct SnParam {
    code: u32,
    current: u64,
    writable: i32,
    value_type: u32,
    num_options: i32,
    options: [i64; SN_MAX_OPTIONS],
}

extern "C" {
    fn sn_init() -> c_int;
    fn sn_release();
    fn sn_list_devices(out: *mut SnDeviceInfo, capacity: c_int, time_in_sec: c_int) -> c_int;
    fn sn_connect(native_id: *const c_char, err: *mut u32) -> *mut c_void;
    fn sn_disconnect(cam: *mut c_void);
    fn sn_is_alive(cam: *mut c_void) -> c_int;
    fn sn_get_parameters(cam: *mut c_void, out: *mut SnParam, capacity: c_int) -> c_int;
    fn sn_set_parameter(cam: *mut c_void, code: u32, value: u64) -> c_int;
    fn sn_set_iso_auto(cam: *mut c_void, enable: c_int) -> c_int;
    fn sn_get_live_view(cam: *mut c_void, out: *mut *mut u8, size: *mut u32) -> c_int;
    fn sn_capture(cam: *mut c_void, out: *mut *mut u8, size: *mut u32) -> c_int;
    fn sn_free(p: *mut u8);
}

// ---------------------------------------------------------------------------
// Sony property codes (CrDevicePropertyCode) we expose
// ---------------------------------------------------------------------------

const CODE_FNUMBER: u32 = 0x0100;
const CODE_EXPOSURE_BIAS: u32 = 0x0101;
const CODE_SHUTTER_SPEED: u32 = 0x0103;
const CODE_ISO: u32 = 0x0104;
const CODE_STILL_QUALITY: u32 = 0x0107;
const CODE_WHITE_BALANCE: u32 = 0x0108;
const CODE_FOCUS_MODE: u32 = 0x0109;

/// Human name for a CrDevicePropertyCode (photo-relevant subset), for the
/// `SONY_DUMP` diagnostic only.
fn prop_name(code: u32) -> &'static str {
    match code {
        0x0100 => "FNumber",
        0x0101 => "ExposureBiasCompensation",
        0x0102 => "FlashCompensation",
        0x0103 => "ShutterSpeed",
        0x0104 => "IsoSensitivity",
        0x0105 => "ExposureProgramMode",
        0x0106 => "FileType",
        0x0107 => "StillImageQuality",
        0x0108 => "WhiteBalance",
        0x0109 => "FocusMode",
        0x010A => "MeteringMode",
        0x010B => "FlashMode",
        0x010C => "WirelessFlash",
        0x010D => "RedEyeReduction",
        0x010E => "DriveMode",
        0x010F => "DRO",
        0x0110 => "ImageSize",
        0x0111 => "AspectRatio",
        0x0112 => "PictureEffect",
        0x0113 => "FocusArea",
        0x0115 => "ColorTemp",
        0x0116 => "ColorTuningAB",
        0x0117 => "ColorTuningGM",
        0x0118 => "LiveViewDisplayEffect",
        0x0119 => "StillImageStoreDestination",
        0x011A => "PriorityKeySettings",
        0x011B => "AFTrackingSensitivity",
        _ => "-",
    }
}

fn code_to_param_type(code: u32) -> Option<ParameterType> {
    match code {
        CODE_FNUMBER => Some(ParameterType::Aperture),
        CODE_EXPOSURE_BIAS => Some(ParameterType::ExposureCompensation),
        CODE_SHUTTER_SPEED => Some(ParameterType::ShutterSpeed),
        CODE_ISO => Some(ParameterType::Iso),
        CODE_STILL_QUALITY => Some(ParameterType::ImageQuality),
        CODE_WHITE_BALANCE => Some(ParameterType::WhiteBalance),
        CODE_FOCUS_MODE => Some(ParameterType::FocusMode),
        _ => None,
    }
}

fn param_type_to_code(pt: ParameterType) -> Option<u32> {
    match pt {
        ParameterType::Aperture => Some(CODE_FNUMBER),
        ParameterType::ExposureCompensation => Some(CODE_EXPOSURE_BIAS),
        ParameterType::ShutterSpeed => Some(CODE_SHUTTER_SPEED),
        ParameterType::Iso => Some(CODE_ISO),
        ParameterType::ImageQuality => Some(CODE_STILL_QUALITY),
        ParameterType::WhiteBalance => Some(CODE_WHITE_BALANCE),
        ParameterType::FocusMode => Some(CODE_FOCUS_MODE),
        _ => None,
    }
}

/// Value parameters (numerically ordered) render as `range_select`; enum choices
/// render as `select`.
fn is_range_select(pt: ParameterType) -> bool {
    matches!(
        pt,
        ParameterType::Aperture
            | ParameterType::ShutterSpeed
            | ParameterType::Iso
            | ParameterType::ExposureCompensation
    )
}

// ---------------------------------------------------------------------------
// Label decoding (raw SDK code → human label)
// ---------------------------------------------------------------------------

/// Byte width of one element for a CrDataType (0 = unsupported / string).
fn element_width(value_type: u32) -> u32 {
    match value_type & 0x000F {
        0x0001 => 1,
        0x0002 => 2,
        0x0003 => 4,
        0x0004 => 8,
        _ => 0,
    }
}

/// Options dropped from the choice list. Shutter speed hides Bulb (`0x00000000`)
/// and the "nothing to display" sentinel (`0xFFFFFFFF`): capture is single-shot
/// JPEG, so a bulb exposure has no meaning here.
fn is_hidden_option(pt: ParameterType, raw: u64) -> bool {
    matches!(pt, ParameterType::ShutterSpeed) && (raw as u32 == 0 || raw as u32 == 0xFFFF_FFFF)
}

fn decode_label(pt: ParameterType, raw: u64) -> String {
    match pt {
        ParameterType::Aperture => fmt_aperture(raw),
        ParameterType::ShutterSpeed => fmt_shutter(raw),
        ParameterType::Iso => fmt_iso(raw),
        ParameterType::ExposureCompensation => fmt_exposure_comp(raw),
        ParameterType::WhiteBalance => fmt_white_balance(raw),
        ParameterType::FocusMode => fmt_focus_mode(raw),
        ParameterType::ImageQuality => fmt_image_quality(raw),
        _ => raw.to_string(),
    }
}

/// FNumber: raw = f-number × 100.
fn fmt_aperture(raw: u64) -> String {
    let f = raw as f64 / 100.0;
    if (f.fract()).abs() < 0.05 {
        format!("f/{}", f.round() as i64)
    } else {
        format!("f/{f:.1}")
    }
}

/// ShutterSpeed: upper 16 bits numerator, lower 16 bits denominator; 0 = Bulb.
fn fmt_shutter(raw: u64) -> String {
    let u = raw as u32;
    if u == 0 {
        return "Bulb".to_string();
    }
    if u == 0xFFFF_FFFF {
        return "—".to_string();
    }
    let num = (u >> 16) & 0xFFFF;
    let den = u & 0xFFFF;
    if den == 0 || num == 0 {
        return "Bulb".to_string();
    }
    if num >= den {
        if num.is_multiple_of(den) {
            format!("{}\"", num / den)
        } else {
            format!("{:.1}\"", num as f64 / den as f64)
        }
    } else {
        format!("1/{}", (den as f64 / num as f64).round() as u32)
    }
}

/// IsoSensitivity: bits 0-23 ISO value, bits 24-27 mode, bits 28-31 extension;
/// 0xFFFFFF = AUTO.
fn fmt_iso(raw: u64) -> String {
    let u = raw as u32;
    let iso = u & 0x00FF_FFFF;
    let ext = (u >> 28) & 0xF;
    if iso == 0x00FF_FFFF {
        return "ISO AUTO".to_string();
    }
    if ext != 0 {
        format!("ISO {iso} (ext)")
    } else {
        format!("ISO {iso}")
    }
}

/// ExposureBiasCompensation: signed 16-bit, value = EV × 1000.
fn fmt_exposure_comp(raw: u64) -> String {
    let ev = (raw as u16) as i16 as f64 / 1000.0;
    format!("{ev:+.1} EV")
}

fn fmt_white_balance(raw: u64) -> String {
    let label = match raw as u16 {
        0x0000 => "Auto",
        0x0001 => "Underwater Auto",
        0x0011 => "Daylight",
        0x0012 => "Shade",
        0x0013 => "Cloudy",
        0x0014 => "Incandescent",
        0x0020 => "Fluorescent",
        0x0021 => "Fluorescent: Warm White",
        0x0022 => "Fluorescent: Cool White",
        0x0023 => "Fluorescent: Day White",
        0x0024 => "Fluorescent: Daylight",
        0x0030 => "Flash",
        0x0100 => "Color Temperature",
        0x0101 => "Custom 1",
        0x0102 => "Custom 2",
        0x0103 => "Custom 3",
        0x0104 => "Custom",
        _ => return format!("0x{:04X}", raw as u16),
    };
    label.to_string()
}

fn fmt_focus_mode(raw: u64) -> String {
    let label = match raw as u16 {
        0x0001 => "Manual",
        0x0002 => "AF-S",
        0x0003 => "AF-C",
        0x0004 => "AF-A",
        0x0005 => "AF-D",
        0x0006 => "DMF",
        0x0007 => "PF",
        _ => return format!("0x{:04X}", raw as u16),
    };
    label.to_string()
}

fn fmt_image_quality(raw: u64) -> String {
    let label = match raw as u16 {
        0x0001 => "Light",
        0x0002 => "Standard",
        0x0003 => "Fine",
        0x0004 => "Extra Fine",
        _ => return format!("0x{:04X}", raw as u16),
    };
    label.to_string()
}

// ---------------------------------------------------------------------------
// Actor commands
// ---------------------------------------------------------------------------

enum Command {
    Connect {
        device_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    Disconnect {
        device_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    IsConnected {
        device_id: String,
        reply: mpsc::Sender<bool>,
    },
    GetParameters {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<CameraParameter>, CameraError>>,
    },
    SetParameter {
        device_id: String,
        param_type: ParameterType,
        value: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    GetLiveViewFrame {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    CapturePhoto {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    PrepareExit {
        ack: mpsc::Sender<()>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct SonyBackend {
    tx: mpsc::Sender<Command>,
    /// Latest enumerated devices, refreshed on the idle SDK thread. `list_devices`
    /// reads this directly (no actor round-trip) so `/cameras` returns instantly —
    /// the CrSDK USB enumeration takes ~3 s and would otherwise blow the route's
    /// per-backend timeout. A freshly plugged camera appears within one refresh.
    cache: Arc<Mutex<Vec<DeviceInfo>>>,
}

impl SonyBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (tx, rx) = mpsc::channel::<Command>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), CameraError>>();
        let cache: Arc<Mutex<Vec<DeviceInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let thread_cache = cache.clone();

        std::thread::Builder::new()
            .name("sony-sdk".to_string())
            .spawn(move || actor_thread(rx, init_tx, thread_cache))
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;

        init_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))?;

        Ok(Self { tx, cache })
    }

    fn call<T>(&self, make: impl FnOnce(mpsc::Sender<T>) -> Command, on_err: T) -> T {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(make(reply_tx)).is_err() {
            return on_err;
        }
        reply_rx.recv().unwrap_or(on_err)
    }
}

impl Drop for SonyBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for SonyBackend {
    fn backend_id(&self) -> &str {
        "sony"
    }

    /// Above the generic backends (like Canon's EDSDK): the Sony SDK gives native
    /// live view and the full parameter set, so it wins dedup over gphoto2 for the
    /// same body.
    fn dedup_priority(&self) -> i32 {
        10
    }

    /// Releases the SDK before the process exits: closes all sessions and calls
    /// Release on the SDK thread, bounded so an abrupt Ctrl-C never hangs.
    fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Command::PrepareExit { ack: ack_tx }).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(3));
        }
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        // Served from the background-refreshed cache — never blocks on the SDK.
        Ok(self.cache.lock().map(|c| c.clone()).unwrap_or_default())
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        self.call(
            |reply| Command::Connect {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        self.call(
            |reply| Command::Disconnect {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.call(
            |reply| Command::IsConnected {
                device_id: native_id.to_string(),
                reply,
            },
            false,
        )
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        self.call(
            |reply| Command::GetParameters {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        self.call(
            |reply| Command::SetParameter {
                device_id: native_id.to_string(),
                param_type,
                value: value.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.call(
            |reply| Command::GetLiveViewFrame {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.call(
            |reply| Command::CapturePhoto {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }
}

// ---------------------------------------------------------------------------
// Actor thread — the only place SDK / bridge calls happen
// ---------------------------------------------------------------------------

/// An open session handle. Raw pointers live exclusively on the SDK thread.
struct SessionHandle(*mut c_void);
unsafe impl Send for SessionHandle {}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        unsafe { sn_disconnect(self.0) };
    }
}

fn actor_thread(
    rx: mpsc::Receiver<Command>,
    init_tx: mpsc::Sender<Result<(), CameraError>>,
    cache: Arc<Mutex<Vec<DeviceInfo>>>,
) {
    let init = unsafe { sn_init() };
    if init != SN_OK {
        let _ = init_tx.send(Err(CameraError::SdkError(0xFFFF_FFFF)));
        return;
    }
    let _ = init_tx.send(Ok(()));

    let mut sessions: HashMap<String, SessionHandle> = HashMap::new();

    // Warm the cache up front so a camera present at startup shows on an early poll.
    // A fast scan (1 s floor, ~2.2 s) reliably finds a body that is already plugged
    // in at launch — the common case — instead of paying the full ~6.5 s thorough
    // scan every start.
    refresh_cache(&cache, &sessions, 1);

    // Re-enumerate after this much idle time to pick up (un)plugged cameras. Kept
    // short because a powered-off *connected* body is only ever noticed here — the
    // SDK does not reliably deliver OnDisconnected for a power cut, so enumeration
    // dropping the device is the sole signal, and this interval bounds how long a
    // gone camera lingers in the list. During active use (e.g. live view) commands
    // arrive faster than this, so the refresh never fires and the blocking
    // enumeration stays entirely off the hot path.
    const REFRESH: Duration = Duration::from_secs(2);

    loop {
        match rx.recv_timeout(REFRESH) {
            Ok(Command::IsConnected { device_id, reply }) => {
                let _ = reply.send(prune_dead_session(&device_id, &mut sessions, &cache));
            }
            Ok(Command::Connect { device_id, reply }) => {
                let result = connect_impl(&device_id, &mut sessions);
                if result.is_ok() {
                    set_cached_connected(&cache, &device_id, true);
                }
                let _ = reply.send(result);
            }
            Ok(Command::Disconnect { device_id, reply }) => {
                let result = disconnect_impl(&device_id, &mut sessions);
                if result.is_ok() {
                    set_cached_connected(&cache, &device_id, false);
                }
                let _ = reply.send(result);
            }
            Ok(Command::GetParameters { device_id, reply }) => {
                let _ = reply.send(get_parameters_impl(&device_id, &sessions));
            }
            Ok(Command::SetParameter {
                device_id,
                param_type,
                value,
                reply,
            }) => {
                let _ = reply.send(set_parameter_impl(
                    &device_id, param_type, &value, &sessions,
                ));
            }
            Ok(Command::GetLiveViewFrame { device_id, reply }) => {
                let _ = reply.send(get_live_view_impl(&device_id, &sessions));
            }
            Ok(Command::CapturePhoto { device_id, reply }) => {
                let _ = reply.send(capture_photo_impl(&device_id, &sessions));
            }
            // Release everything here (SDK calls are only valid on this thread),
            // ack, then return so the process can exit cleanly.
            Ok(Command::PrepareExit { ack }) => {
                sessions.clear(); // SessionHandle::drop closes each session
                unsafe { sn_release() };
                let _ = ack.send(());
                return;
            }
            Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Fast refresh (~2.2 s) once a camera is known; escalate to a thorough
                // scan (~6.5 s) only while the cache is empty, to give a cold / freshly
                // hot-plugged body the longer enumeration it may need to appear.
                let thorough = cache.lock().map(|c| c.is_empty()).unwrap_or(true);
                refresh_cache(&cache, &sessions, if thorough { 3 } else { 1 });
            }
        }
    }

    sessions.clear();
    unsafe { sn_release() };
}

/// True while the device still has a live session. The SDK can drop a body at any
/// time (OnDisconnected); its handle then answers CrError_Api_InvalidCalled to
/// everything, so a session we keep around would make the camera look connected
/// while every call against it fails. Drop it instead, and let the client reconnect.
fn prune_dead_session(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
    cache: &Arc<Mutex<Vec<DeviceInfo>>>,
) -> bool {
    match sessions.get(device_id) {
        None => false,
        Some(session) => {
            if unsafe { sn_is_alive(session.0) } != 0 {
                return true;
            }
            eprintln!("[sony] device {device_id} was dropped by the SDK — closing session");
            sessions.remove(device_id);
            set_cached_connected(cache, device_id, false);
            false
        }
    }
}

/// Re-enumerates and replaces the shared device cache (runs on the SDK thread).
/// `time_in_sec` picks the SDK enumeration floor — see the call sites for the
/// thorough (3, ~6.5 s) vs fast (1, ~2.2 s) policy.
fn refresh_cache(
    cache: &Arc<Mutex<Vec<DeviceInfo>>>,
    sessions: &HashMap<String, SessionHandle>,
    time_in_sec: c_int,
) {
    if let Ok(devices) = list_devices_impl(sessions, time_in_sec) {
        if let Ok(mut c) = cache.lock() {
            *c = devices;
        }
    }
}

/// Patches a cached device's `connected` flag after a connect/disconnect, so the
/// UI reflects it immediately instead of waiting for the next enumeration.
fn set_cached_connected(cache: &Arc<Mutex<Vec<DeviceInfo>>>, native_id: &str, connected: bool) {
    let id = DeviceId::new("sony", native_id).encode();
    if let Ok(mut c) = cache.lock() {
        for d in c.iter_mut().filter(|d| d.id == id) {
            d.connected = connected;
        }
    }
}

// ---------------------------------------------------------------------------
// Implementations (run exclusively on the SDK thread)
// ---------------------------------------------------------------------------

fn list_devices_impl(
    sessions: &HashMap<String, SessionHandle>,
    time_in_sec: c_int,
) -> Result<Vec<DeviceInfo>, CameraError> {
    let mut buf: Vec<SnDeviceInfo> = (0..SN_MAX_DEVICES)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();

    let count = unsafe { sn_list_devices(buf.as_mut_ptr(), SN_MAX_DEVICES as c_int, time_in_sec) };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let devices = buf
        .iter()
        .map(|d| {
            let native_id = cstr(&d.id);
            let model = cstr(&d.model);
            let id = DeviceId::new("sony", &native_id).encode();
            let connected = sessions.contains_key(&native_id);
            DeviceInfo {
                id,
                name: format!("Sony {model}"),
                connected,
                dedup_key: Some(dedup_key(SONY_VENDOR_ID, &model)),
            }
        })
        .collect();

    Ok(devices)
}

fn connect_impl(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    if sessions.contains_key(device_id) {
        return Ok(()); // idempotent
    }
    let c_id = CString::new(device_id).map_err(|_| CameraError::InvalidDeviceId)?;
    let mut err: u32 = 0;
    let handle = unsafe { sn_connect(c_id.as_ptr(), &mut err) };
    if handle.is_null() {
        return Err(connect_error(device_id, err));
    }
    sessions.insert(device_id.to_string(), SessionHandle(handle));
    Ok(())
}

/// A camera that still believes a previous PC Remote session is open refuses the
/// next Connect and stays that way until it is power-cycled — no amount of retrying
/// clears it. The body says so with one of three CrErrors:
/// - `CrError_Connect_TimeOut` (0x8208): on the USB bus but never completes the handshake.
/// - `CrError_Reconnect_TimeOut` (0x8209): the SDK's auto-reconnect gave up.
/// - `CrError_Connect_SessionAlreadyOpened` (0x8210): a session is explicitly still open.
/// Map all three to the same actionable message: reporting them as "device not found"
/// (or a raw hex code) sends people looking for a camera that is plugged in and visibly on.
fn connect_error(device_id: &str, err: u32) -> CameraError {
    const CONNECT_TIMEOUT: u32 = 0x0000_8208;
    const RECONNECT_TIMEOUT: u32 = 0x0000_8209;
    const SESSION_ALREADY_OPENED: u32 = 0x0000_8210;
    const NOT_FOUND: u32 = 0xFFFF_FFFF;
    match err {
        CONNECT_TIMEOUT | RECONNECT_TIMEOUT | SESSION_ALREADY_OPENED => CameraError::Backend(
            "the camera did not answer: it is still holding an earlier PC Remote \
             session. Turn it off and on again (or unplug and replug it)."
                .to_string(),
        ),
        NOT_FOUND | 0 => CameraError::DeviceNotFound(device_id.to_string()),
        code => CameraError::SdkError(code),
    }
}

fn disconnect_impl(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    sessions
        .remove(device_id)
        .ok_or_else(|| CameraError::DeviceNotFound(device_id.to_string()))?;
    // SessionHandle::drop calls sn_disconnect.
    Ok(())
}

fn get_parameters_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<CameraParameter>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut buf: Vec<SnParam> = (0..SN_MAX_PARAMS)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();
    let count = unsafe { sn_get_parameters(handle, buf.as_mut_ptr(), SN_MAX_PARAMS as c_int) };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    // Diagnostic: `SONY_DUMP=1` lists every property the body exposes (code, name,
    // whether we map it, settable flag, option count) so we can see what the SDK
    // offers beyond the handful we surface.
    if std::env::var_os("SONY_DUMP").is_some() {
        eprintln!("[sony-dump] {} properties:", buf.len());
        for p in &buf {
            eprintln!(
                "  0x{:04X} {:32} mapped={} settable={} options={}",
                p.code,
                prop_name(p.code),
                code_to_param_type(p.code).is_some() || p.code == CODE_ISO,
                p.writable != 0,
                p.num_options
            );
        }
    }

    let mut params: Vec<CameraParameter> = Vec::new();
    for p in &buf {
        if p.code == CODE_ISO {
            // ISO is split into an IsoAuto toggle + an Iso selector (AUTO removed),
            // mirroring the Canon / gphoto2 backends.
            params.extend(build_iso_params(p));
        } else if let Some(cp) = build_parameter(p) {
            params.push(cp);
        }
    }
    Ok(params)
}

/// Sony encodes ISO AUTO as the value `0x00FFFFFF` (bits 0-23 all set) in the ISO
/// list; there is no separate auto toggle property.
const ISO_AUTO: u32 = 0x00FF_FFFF;

fn iso_is_auto(raw: u64) -> bool {
    (raw as u32) & ISO_AUTO == ISO_AUTO
}

/// Splits the ISO property into an `IsoAuto` boolean plus an `Iso` selector with
/// the AUTO entry removed; the selector is `disabled` while auto is on.
fn build_iso_params(p: &SnParam) -> Vec<CameraParameter> {
    let mask = 0xFFFF_FFFFu64; // ISO is UInt32
    let writable = p.writable != 0;
    let current_raw = p.current & mask;
    let auto_on = iso_is_auto(current_raw);

    let options: Vec<ParameterOption> = p.options[..p.num_options.max(0) as usize]
        .iter()
        .map(|&raw| raw as u64 & mask)
        .filter(|&v| !iso_is_auto(v))
        .map(|v| ParameterOption {
            label: fmt_iso(v),
            value: v.to_string(),
        })
        .collect();

    let mut out = vec![CameraParameter::Boolean {
        param_type: ParameterType::IsoAuto,
        current: auto_on,
        disabled: !writable,
    }];

    if options.len() >= 2 {
        out.push(CameraParameter::RangeSelect {
            param_type: ParameterType::Iso,
            current: current_raw.to_string(),
            options,
            disabled: auto_on || !writable,
        });
    }
    out
}

/// Turns one raw SDK property into a CameraParameter, or `None` if we don't map
/// the code.
///
/// When the body currently offers no real choice — e.g. exposure compensation
/// locked by the physical dial or neutralised in Manual mode, where the SDK
/// reports fewer than two options — the control is kept **visible but disabled**
/// (showing its current value) rather than made to vanish, so the parameter list
/// doesn't flicker as the camera state changes.
fn build_parameter(p: &SnParam) -> Option<CameraParameter> {
    let param_type = code_to_param_type(p.code)?;

    let width = element_width(p.value_type);
    let mask = if width == 0 || width >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * width)) - 1
    };

    let current_val = p.current & mask;
    let current = current_val.to_string();

    let mut options: Vec<ParameterOption> = p.options[..p.num_options.max(0) as usize]
        .iter()
        .map(|&raw| raw as u64 & mask)
        .filter(|&v| !is_hidden_option(param_type, v))
        .map(|v| ParameterOption {
            label: decode_label(param_type, v),
            value: v.to_string(),
        })
        .collect();

    // Fewer than two options → no real choice: show it greyed out. Ensure at least
    // the current value is present so the disabled control still displays it.
    let no_choice = options.len() < 2;
    if options.is_empty() {
        options.push(ParameterOption {
            label: decode_label(param_type, current_val),
            value: current.clone(),
        });
    }
    let disabled = p.writable == 0 || no_choice;

    Some(if is_range_select(param_type) {
        CameraParameter::RangeSelect {
            param_type,
            current,
            options,
            disabled,
        }
    } else {
        CameraParameter::Select {
            param_type,
            current,
            options,
            disabled,
        }
    })
}

fn set_parameter_impl(
    device_id: &str,
    param_type: ParameterType,
    value: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    // ISO Auto has no dedicated SDK property — the bridge translates the toggle to
    // an ISO value (AUTO sentinel, or the lowest concrete ISO when turned off).
    if param_type == ParameterType::IsoAuto {
        let enable = match value {
            "true" => 1,
            "false" => 0,
            _ => return Err(CameraError::NotSupported),
        };
        let ret = unsafe { sn_set_iso_auto(handle, enable) };
        return if ret == SN_OK {
            Ok(())
        } else {
            Err(CameraError::SdkError(ret as u32))
        };
    }

    let code = param_type_to_code(param_type).ok_or(CameraError::NotSupported)?;
    let raw: u64 = value.parse().map_err(|_| CameraError::NotSupported)?;

    let ret = unsafe { sn_set_parameter(handle, code, raw) };
    if ret != SN_OK {
        return Err(CameraError::SdkError(ret as u32));
    }
    Ok(())
}

fn get_live_view_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size: u32 = 0;
    let ret = unsafe { sn_get_live_view(handle, &mut data_ptr, &mut size) };

    if ret == SN_NOT_READY {
        // Same "not ready" convention as the other backends.
        return Err(CameraError::SdkError(0x0000_A102));
    }
    if ret != SN_OK || data_ptr.is_null() {
        return Err(CameraError::SdkError(0xFFFF_FFFE));
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size as usize).to_vec() };
    unsafe { sn_free(data_ptr) };
    Ok(bytes)
}

fn capture_photo_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size: u32 = 0;
    let ret = unsafe { sn_capture(handle, &mut data_ptr, &mut size) };

    if ret == SN_ERR_AF {
        return Err(CameraError::Backend(
            "autofocus could not lock, so no photo was taken. Aim at something with \
             more contrast, or switch the camera to manual focus."
                .to_string(),
        ));
    }
    if ret != SN_OK || data_ptr.is_null() {
        return Err(CameraError::SdkError(0xFFFF_FFFD));
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size as usize).to_vec() };
    unsafe { sn_free(data_ptr) };
    Ok(bytes)
}

/// Reads a NUL-terminated C char array into an owned String (lossy UTF-8).
fn cstr(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aperture_labels() {
        assert_eq!(fmt_aperture(280), "f/2.8");
        assert_eq!(fmt_aperture(800), "f/8");
        assert_eq!(fmt_aperture(180), "f/1.8");
        assert_eq!(fmt_aperture(560), "f/5.6");
    }

    #[test]
    fn shutter_labels() {
        // 1/500 → num=1, den=500
        assert_eq!(fmt_shutter((1 << 16) | 500), "1/500");
        // 2" → num=2, den=1
        assert_eq!(fmt_shutter((2 << 16) | 1), "2\"");
        // 1.3" → num=13, den=10
        assert_eq!(fmt_shutter((13 << 16) | 10), "1.3\"");
        assert_eq!(fmt_shutter(0), "Bulb");
    }

    #[test]
    fn iso_labels() {
        assert_eq!(fmt_iso(400), "ISO 400");
        assert_eq!(fmt_iso(0x00FF_FFFF), "ISO AUTO");
    }

    #[test]
    fn exposure_comp_labels() {
        assert_eq!(fmt_exposure_comp(300), "+0.3 EV");
        // -1.0 EV = -1000 stored as signed 16-bit
        assert_eq!(fmt_exposure_comp((-1000i16) as u16 as u64), "-1.0 EV");
        assert_eq!(fmt_exposure_comp(0), "+0.0 EV");
    }

    #[test]
    fn enum_labels() {
        assert_eq!(fmt_white_balance(0x0000), "Auto");
        assert_eq!(fmt_white_balance(0x0011), "Daylight");
        assert_eq!(fmt_focus_mode(0x0002), "AF-S");
        assert_eq!(fmt_image_quality(0x0003), "Fine");
    }

    #[test]
    fn code_mapping_roundtrip() {
        for pt in [
            ParameterType::Aperture,
            ParameterType::ShutterSpeed,
            ParameterType::Iso,
            ParameterType::ExposureCompensation,
            ParameterType::WhiteBalance,
            ParameterType::FocusMode,
            ParameterType::ImageQuality,
        ] {
            let code = param_type_to_code(pt).unwrap();
            assert_eq!(code_to_param_type(code), Some(pt));
        }
    }
}
