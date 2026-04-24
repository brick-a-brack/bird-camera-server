use std::collections::HashMap;
use std::sync::mpsc;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::DirectShow::{
    CameraControlProperty, IAMCameraControl, IAMVideoProcAmp,
    VideoProcAmpProperty,
    CameraControl_Exposure, CameraControl_Flags_Auto, CameraControl_Flags_Manual,
    CameraControl_Focus, CameraControl_Pan, CameraControl_Roll, CameraControl_Tilt,
    CameraControl_Zoom,
    VideoProcAmp_BacklightCompensation, VideoProcAmp_Brightness, VideoProcAmp_Contrast,
    VideoProcAmp_Flags_Auto, VideoProcAmp_Flags_Manual, VideoProcAmp_Gain, VideoProcAmp_Gamma,
    VideoProcAmp_Hue, VideoProcAmp_Saturation, VideoProcAmp_Sharpness, VideoProcAmp_WhiteBalance,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFShutdown, MFStartup, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MFMediaType_Video, MFVideoFormat_MJPG, MFVideoFormat_YUY2,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
};

// MF_VERSION = (MF_SDK_VERSION << 16 | MF_API_VERSION) = (0x0002 << 16 | 0x0070)
const MF_SDK_VERSION_VALUE: u32 = 0x0002_0070;

// IMFSourceReader stream-flags (MFSTREAMSINK_MARKER_FLAG)
const MF_SOURCE_READERF_ERROR: u32 = 0x0001;
const MF_SOURCE_READERF_STREAMTICK: u32 = 0x0100;

// Convenience: cast MF_SOURCE_READER_CONSTANTS to u32 for all calls.
fn video_stream() -> u32 {
    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

enum Command {
    ListDevices {
        reply: mpsc::Sender<Result<Vec<DeviceInfo>, CameraError>>,
    },
    Connect {
        native_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    Disconnect {
        native_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    IsConnected {
        native_id: String,
        reply: mpsc::Sender<bool>,
    },
    GetParameters {
        native_id: String,
        reply: mpsc::Sender<Result<Vec<CameraParameter>, CameraError>>,
    },
    GetLiveViewFrame {
        native_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    SetParameter {
        native_id: String,
        kind: String,
        value: i32,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Per-device state — lives exclusively on the SDK thread
// ---------------------------------------------------------------------------

struct DeviceState {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    video_proc_amp: Option<IAMVideoProcAmp>,
    camera_control: Option<IAMCameraControl>,
    is_mjpeg: bool,
    width: u32,
    height: u32,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Windows webcam backend using Media Foundation.
///
/// All Media Foundation and DirectShow COM calls are dispatched to a single
/// dedicated OS thread (actor pattern), keeping them off the tokio thread pool.
pub struct WebcamWindowsBackend {
    tx: mpsc::Sender<Command>,
}

impl WebcamWindowsBackend {
    pub fn new() -> Result<Self, CameraError> {
        eprintln!("[webcam-windows] WebcamWindowsBackend::new() called");
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), CameraError>>();

        std::thread::Builder::new()
            .name("webcam-windows-sdk".to_string())
            .spawn(move || sdk_thread(cmd_rx, init_tx))
            .expect("failed to spawn webcam-windows-sdk thread");

        let init_result = init_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)));
        eprintln!("[webcam-windows] SDK thread init result: {:?}", init_result.is_ok());
        init_result?;

        eprintln!("[webcam-windows] backend ready");
        Ok(Self { tx: cmd_tx })
    }
}

impl Drop for WebcamWindowsBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for WebcamWindowsBackend {
    fn backend_id(&self) -> &str {
        "webcam-windows"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        eprintln!("[webcam-windows] list_devices() called");
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::ListDevices { reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        eprintln!("[webcam-windows] ListDevices command sent, waiting for reply");
        let result = reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)));
        eprintln!("[webcam-windows] list_devices() reply received");
        result
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Connect { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Disconnect { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn is_connected(&self, native_id: &str) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(Command::IsConnected { native_id: native_id.to_string(), reply: reply_tx })
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetParameters { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetLiveViewFrame { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn set_parameter(&self, native_id: &str, kind: &str, value: i32) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::SetParameter {
                native_id: native_id.to_string(),
                kind: kind.to_string(),
                value,
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }
}

// ---------------------------------------------------------------------------
// SDK thread
// ---------------------------------------------------------------------------

fn sdk_thread(rx: mpsc::Receiver<Command>, init_tx: mpsc::Sender<Result<(), CameraError>>) {
    // Initialize COM in a single-threaded apartment (STA). Many webcam drivers
    // are STA COM components and will not enumerate under MTA (COINIT_MULTITHREADED).
    // S_FALSE (already initialized) is also fine.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    if let Err(e) = unsafe { MFStartup(MF_SDK_VERSION_VALUE, 1 /* MFSTARTUP_NOSOCKET */) } {
        let _ = init_tx.send(Err(CameraError::SdkError(e.code().0 as u32)));
        unsafe { CoUninitialize() };
        return;
    }

    let _ = init_tx.send(Ok(()));
    drop(init_tx);

    // Device state lives exclusively on this thread.
    let mut connected: HashMap<String, DeviceState> = HashMap::new();

    // STA COM requires this thread to pump Windows messages so that inter-apartment
    // calls and driver callbacks can be dispatched. We poll for commands with a short
    // timeout and pump the message queue between iterations.
    loop {
        // Pump all pending Windows messages before blocking on the next command.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        match rx.recv_timeout(std::time::Duration::from_millis(16)) {
            Ok(Command::ListDevices { reply }) => {
                eprintln!("[webcam-windows] ListDevices command received");
                let result = list_devices_impl(&connected);
                eprintln!("[webcam-windows] ListDevices result: {:?}", result.as_ref().map(|v| v.len()));
                let _ = reply.send(result);
            }
            Ok(Command::Connect { native_id, reply }) => {
                let _ = reply.send(connect_impl(&native_id, &mut connected));
            }
            Ok(Command::Disconnect { native_id, reply }) => {
                let _ = reply.send(disconnect_impl(&native_id, &mut connected));
            }
            Ok(Command::IsConnected { native_id, reply }) => {
                let _ = reply.send(connected.contains_key(&native_id));
            }
            Ok(Command::GetParameters { native_id, reply }) => {
                let result = connected
                    .get(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(get_parameters_impl);
                let _ = reply.send(result);
            }
            Ok(Command::GetLiveViewFrame { native_id, reply }) => {
                let result = connected
                    .get(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(get_live_view_frame_impl);
                let _ = reply.send(result);
            }
            Ok(Command::SetParameter { native_id, kind, value, reply }) => {
                let result = connected
                    .get(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(|state| set_parameter_impl(state, &kind, value));
                let _ = reply.send(result);
            }
            Ok(Command::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }

    for (_, state) in connected.drain() {
        unsafe { let _ = state.source.Shutdown(); }
    }

    let _ = unsafe { MFShutdown() };
    unsafe { CoUninitialize() };
}

// ---------------------------------------------------------------------------
// SDK operations (run exclusively on the SDK thread)
// ---------------------------------------------------------------------------

fn win_err(e: windows::core::Error) -> CameraError {
    CameraError::SdkError(e.code().0 as u32)
}

fn list_devices_impl(
    connected: &HashMap<String, DeviceState>,
) -> Result<Vec<DeviceInfo>, CameraError> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(win_err)?;
        let attrs = attrs.ok_or(CameraError::SdkError(0xFFFF_FFFF))?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(win_err)?;

        let mut devices_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        let hr = MFEnumDeviceSources(&attrs, &mut devices_ptr, &mut count);
        eprintln!("[webcam-windows] MFEnumDeviceSources hr={:?} count={count}", hr);
        hr.map_err(win_err)?;

        let mut result = Vec::with_capacity(count as usize);

        for i in 0..count as usize {
            // Take ownership of the activate pointer from the CoTask-allocated array.
            // Replacing with None prevents double-Release when the array is freed.
            let activate = match std::ptr::replace(devices_ptr.add(i), None) {
                Some(a) => a,
                None => continue,
            };

            let name = read_string_attr(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                .unwrap_or_else(|| "Unknown".to_string());

            let native_id = match read_string_attr(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            ) {
                Some(s) if !s.is_empty() => s,
                _ => continue, // activate dropped here, calls Release
            };

            let id = DeviceId::new("webcam-windows", &native_id).encode();
            let is_connected = connected.contains_key(&native_id);
            result.push(DeviceInfo { id, name, connected: is_connected });
            // activate dropped here, calls Release
        }

        CoTaskMemFree(Some(devices_ptr.cast()));
        Ok(result)
    }
}

fn connect_impl(
    native_id: &str,
    connected: &mut HashMap<String, DeviceState>,
) -> Result<(), CameraError> {
    if connected.contains_key(native_id) {
        return Ok(()); // idempotent
    }

    unsafe {
        let activate = find_activate(native_id)?;

        // Create IMFMediaSource from the activate object.
        let source: IMFMediaSource = activate.ActivateObject().map_err(win_err)?;

        // Build source reader with video processing enabled for format conversion.
        let mut reader_attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut reader_attrs, 1).map_err(win_err)?;
        let reader_attrs = reader_attrs.ok_or(CameraError::SdkError(0xFFFF_FFFF))?;
        reader_attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(win_err)?;

        let reader: IMFSourceReader =
            MFCreateSourceReaderFromMediaSource(&source, &reader_attrs).map_err(win_err)?;

        let (is_mjpeg, width, height) = select_video_format(&reader)?;

        // Query optional control interfaces via QueryInterface on the source.
        let video_proc_amp = source.cast::<IAMVideoProcAmp>().ok();
        let camera_control = source.cast::<IAMCameraControl>().ok();

        connected.insert(
            native_id.to_string(),
            DeviceState { reader, source, video_proc_amp, camera_control, is_mjpeg, width, height },
        );
        Ok(())
    }
}

fn disconnect_impl(
    native_id: &str,
    connected: &mut HashMap<String, DeviceState>,
) -> Result<(), CameraError> {
    let state = connected
        .remove(native_id)
        .ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))?;

    unsafe { let _ = state.source.Shutdown(); }
    // All COM interfaces in state are released via Drop.
    Ok(())
}

fn get_live_view_frame_impl(state: &DeviceState) -> Result<Vec<u8>, CameraError> {
    // ReadSample blocks until a frame is available (~33ms for 30fps).
    // Retry up to 10 times to skip stream ticks (gaps without payload).
    for _ in 0..10 {
        let mut flags: u32 = 0;
        let mut sample: Option<IMFSample> = None;

        unsafe {
            state
                .reader
                .ReadSample(video_stream(), 0, None, Some(&mut flags), None, Some(&mut sample))
                .map_err(win_err)?;
        }

        if flags & MF_SOURCE_READERF_ERROR != 0 {
            return Err(CameraError::SdkError(0xA102_0001));
        }
        if flags & MF_SOURCE_READERF_STREAMTICK != 0 {
            continue;
        }

        let Some(sample) = sample else { continue };

        let data = unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(win_err)?;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut current_len: u32 = 0;
            buffer.Lock(&mut data_ptr, None, Some(&mut current_len)).map_err(win_err)?;
            let bytes = std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec();
            let _ = buffer.Unlock();
            bytes
        };

        if state.is_mjpeg {
            return Ok(data);
        }
        return yuyv_to_jpeg(&data, state.width, state.height);
    }

    Err(CameraError::SdkError(0xA102_0002)) // no frame after retries
}

fn get_parameters_impl(state: &DeviceState) -> Result<Vec<CameraParameter>, CameraError> {
    let mut params = Vec::new();

    if let Some(vpa) = &state.video_proc_amp {
        // (property, api kind name)
        let specs: &[(VideoProcAmpProperty, &str)] = &[
            (VideoProcAmp_Brightness, "brightness"),
            (VideoProcAmp_Contrast, "contrast"),
            (VideoProcAmp_Hue, "hue"),
            (VideoProcAmp_Saturation, "saturation"),
            (VideoProcAmp_Sharpness, "sharpness"),
            (VideoProcAmp_Gamma, "gamma"),
            (VideoProcAmp_WhiteBalance, "white_balance"),
            (VideoProcAmp_BacklightCompensation, "backlight_compensation"),
            (VideoProcAmp_Gain, "gain"),
        ];

        for &(prop, kind) in specs {
            let mut min = 0i32;
            let mut max = 0i32;
            let mut step = 0i32;
            let mut default = 0i32;
            let mut caps = 0i32;

            if unsafe { vpa.GetRange(prop.0, &mut min, &mut max, &mut step, &mut default, &mut caps) }
                .is_err()
            {
                continue;
            }

            let mut cur_value = 0i32;
            let mut cur_flags = 0i32;
            let current = if unsafe { vpa.Get(prop.0, &mut cur_value, &mut cur_flags) }.is_ok() {
                cur_value
            } else {
                default
            };

            params.push(CameraParameter {
                kind: kind.to_string(),
                current: current.to_string(),
                options: vec![],
                min: Some(min),
                max: Some(max),
                step: Some(step),
            });

            // Expose an auto/manual toggle when the device supports automatic mode.
            if caps & VideoProcAmp_Flags_Auto.0 != 0 {
                let is_auto = cur_flags & VideoProcAmp_Flags_Auto.0 != 0;
                params.push(CameraParameter {
                    kind: format!("{kind}_auto"),
                    current: if is_auto { "1" } else { "0" }.to_string(),
                    options: vec![
                        ParameterOption { label: "manual".to_string(), value: 0 },
                        ParameterOption { label: "auto".to_string(), value: 1 },
                    ],
                    min: None,
                    max: None,
                    step: None,
                });
            }
        }
    }

    if let Some(cc) = &state.camera_control {
        let specs: &[(CameraControlProperty, &str)] = &[
            (CameraControl_Pan, "pan"),
            (CameraControl_Tilt, "tilt"),
            (CameraControl_Roll, "roll"),
            (CameraControl_Zoom, "zoom"),
            (CameraControl_Exposure, "exposure"),
            (CameraControl_Focus, "focus"),
        ];

        for &(prop, kind) in specs {
            let mut min = 0i32;
            let mut max = 0i32;
            let mut step = 0i32;
            let mut default = 0i32;
            let mut caps = 0i32;

            if unsafe { cc.GetRange(prop.0, &mut min, &mut max, &mut step, &mut default, &mut caps) }
                .is_err()
            {
                continue;
            }

            let mut cur_value = 0i32;
            let mut cur_flags = 0i32;
            let current = if unsafe { cc.Get(prop.0, &mut cur_value, &mut cur_flags) }.is_ok() {
                cur_value
            } else {
                default
            };

            params.push(CameraParameter {
                kind: kind.to_string(),
                current: current.to_string(),
                options: vec![],
                min: Some(min),
                max: Some(max),
                step: Some(step),
            });

            if caps & CameraControl_Flags_Auto.0 != 0 {
                let is_auto = cur_flags & CameraControl_Flags_Auto.0 != 0;
                params.push(CameraParameter {
                    kind: format!("{kind}_auto"),
                    current: if is_auto { "1" } else { "0" }.to_string(),
                    options: vec![
                        ParameterOption { label: "manual".to_string(), value: 0 },
                        ParameterOption { label: "auto".to_string(), value: 1 },
                    ],
                    min: None,
                    max: None,
                    step: None,
                });
            }
        }
    }

    Ok(params)
}

fn set_parameter_impl(
    state: &DeviceState,
    kind: &str,
    value: i32,
) -> Result<(), CameraError> {
    // Auto/manual toggle: kind ends with "_auto", value 0 = manual, 1 = auto.
    if let Some(base) = kind.strip_suffix("_auto") {
        return set_auto_impl(state, base, value != 0);
    }

    if let Some(vpa) = &state.video_proc_amp {
        if let Some(prop) = vpa_prop(kind) {
            // Setting a manual value always uses MANUAL flag to disable auto mode.
            unsafe { vpa.Set(prop.0, value, VideoProcAmp_Flags_Manual.0) }.map_err(win_err)?;
            return Ok(());
        }
    }

    if let Some(cc) = &state.camera_control {
        if let Some(prop) = cc_prop(kind) {
            unsafe { cc.Set(prop.0, value, CameraControl_Flags_Manual.0) }.map_err(win_err)?;
            return Ok(());
        }
    }

    Err(CameraError::NotSupported)
}

fn set_auto_impl(state: &DeviceState, base_kind: &str, auto: bool) -> Result<(), CameraError> {
    if let Some(vpa) = &state.video_proc_amp {
        if let Some(prop) = vpa_prop(base_kind) {
            // Preserve the current manual value when switching modes.
            let mut cur_value = 0i32;
            let mut cur_flags = 0i32;
            unsafe { vpa.Get(prop.0, &mut cur_value, &mut cur_flags) }.ok();
            let flags = if auto { VideoProcAmp_Flags_Auto.0 } else { VideoProcAmp_Flags_Manual.0 };
            unsafe { vpa.Set(prop.0, cur_value, flags) }.map_err(win_err)?;
            return Ok(());
        }
    }

    if let Some(cc) = &state.camera_control {
        if let Some(prop) = cc_prop(base_kind) {
            let mut cur_value = 0i32;
            let mut cur_flags = 0i32;
            unsafe { cc.Get(prop.0, &mut cur_value, &mut cur_flags) }.ok();
            let flags = if auto { CameraControl_Flags_Auto.0 } else { CameraControl_Flags_Manual.0 };
            unsafe { cc.Set(prop.0, cur_value, flags) }.map_err(win_err)?;
            return Ok(());
        }
    }

    Err(CameraError::NotSupported)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Finds the IMFActivate for the device whose symbolic link matches `native_id`.
unsafe fn find_activate(native_id: &str) -> Result<IMFActivate, CameraError> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1).map_err(win_err)?;
    let attrs = attrs.ok_or(CameraError::SdkError(0xFFFF_FFFF))?;
    attrs
        .SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
        .map_err(win_err)?;

    let mut devices_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFEnumDeviceSources(&attrs, &mut devices_ptr, &mut count).map_err(win_err)?;

    let mut found: Option<IMFActivate> = None;

    for i in 0..count as usize {
        let activate = match std::ptr::replace(devices_ptr.add(i), None) {
            Some(a) => a,
            None => continue,
        };

        if found.is_none() {
            if let Some(link) = read_string_attr(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            ) {
                if link == native_id {
                    found = Some(activate);
                    continue; // moved into found — not dropped
                }
            }
        }
        // activate dropped here for non-matching devices
    }

    CoTaskMemFree(Some(devices_ptr.cast()));
    found.ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))
}

/// Reads and frees an allocated string attribute from an IMFAttributes object.
unsafe fn read_string_attr(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut ptr = PWSTR(std::ptr::null_mut());
    let mut len: u32 = 0;
    attrs.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
    let s = ptr.to_string().ok();
    CoTaskMemFree(Some(ptr.0.cast()));
    s
}

/// Selects the best output video format for the source reader.
///
/// Priority: MJPEG (frames are already JPEG) → YUY2 (manual conversion via
/// `yuyv_to_jpeg`) → YUY2 via MF video processing MFT.
/// Returns `(is_mjpeg, width, height)`.
unsafe fn select_video_format(reader: &IMFSourceReader) -> Result<(bool, u32, u32), CameraError> {
    // Pass 1: look for a native MJPEG type.
    let mut index = 0u32;
    loop {
        let Ok(mt) = reader.GetNativeMediaType(video_stream(), index) else { break };
        let subtype = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or(GUID::zeroed());
        if subtype == MFVideoFormat_MJPG {
            reader
                .SetCurrentMediaType(video_stream(), None, &mt)
                .map_err(win_err)?;
            let (w, h) = frame_size(&mt);
            return Ok((true, w, h));
        }
        index += 1;
    }

    // Get dimensions from the first native type for the YUY2 fallback.
    let first_mt = reader.GetNativeMediaType(video_stream(), 0).map_err(win_err)?;
    let (w, h) = frame_size(&first_mt);

    // Pass 2: request YUY2 output. MF inserts a video processor MFT if needed.
    let yuy2_mt: IMFMediaType = MFCreateMediaType().map_err(win_err)?;
    yuy2_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(win_err)?;
    yuy2_mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_YUY2).map_err(win_err)?;
    yuy2_mt
        .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)
        .map_err(win_err)?;

    if reader.SetCurrentMediaType(video_stream(), None, &yuy2_mt).is_ok() {
        return Ok((false, w, h));
    }

    // Last resort: accept the first native format and attempt YUY2 interpretation.
    reader
        .SetCurrentMediaType(video_stream(), None, &first_mt)
        .map_err(win_err)?;
    Ok((false, w, h))
}

/// Extracts (width, height) from an MF_MT_FRAME_SIZE attribute (width<<32 | height).
unsafe fn frame_size(mt: &IMFMediaType) -> (u32, u32) {
    let packed = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0x0000_0280_0000_01E0); // 640×480
    let w = (packed >> 32) as u32;
    let h = (packed & 0xFFFF_FFFF) as u32;
    (w.max(1), h.max(1))
}

/// Converts a YUY2 (YUYV) frame to a JPEG buffer.
///
/// YUY2 packs two pixels into 4 bytes: Y0 U0 Y1 V0.
fn yuyv_to_jpeg(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CameraError> {
    let mut rgb: Vec<u8> = Vec::with_capacity((width * height * 3) as usize);

    for chunk in data.chunks_exact(4) {
        let y0 = chunk[0] as f32;
        let u = chunk[1] as f32 - 128.0;
        let y1 = chunk[2] as f32;
        let v = chunk[3] as f32 - 128.0;

        for y in [y0, y1] {
            let r = (y + 1.402 * v).clamp(0.0, 255.0) as u8;
            let g = (y - 0.344_136 * u - 0.714_136 * v).clamp(0.0, 255.0) as u8;
            let b = (y + 1.772 * u).clamp(0.0, 255.0) as u8;
            rgb.extend_from_slice(&[r, g, b]);
        }
    }

    let img = image::RgbImage::from_raw(width, height, rgb)
        .ok_or(CameraError::SdkError(0xDEAD_0001))?;
    let mut jpeg_buf: Vec<u8> = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)
        .map_err(|_| CameraError::SdkError(0xDEAD_0002))?;
    Ok(jpeg_buf)
}

fn vpa_prop(kind: &str) -> Option<VideoProcAmpProperty> {
    match kind {
        "brightness" => Some(VideoProcAmp_Brightness),
        "contrast" => Some(VideoProcAmp_Contrast),
        "hue" => Some(VideoProcAmp_Hue),
        "saturation" => Some(VideoProcAmp_Saturation),
        "sharpness" => Some(VideoProcAmp_Sharpness),
        "gamma" => Some(VideoProcAmp_Gamma),
        "white_balance" => Some(VideoProcAmp_WhiteBalance),
        "backlight_compensation" => Some(VideoProcAmp_BacklightCompensation),
        "gain" => Some(VideoProcAmp_Gain),
        _ => None,
    }
}

fn cc_prop(kind: &str) -> Option<CameraControlProperty> {
    match kind {
        "pan" => Some(CameraControl_Pan),
        "tilt" => Some(CameraControl_Tilt),
        "roll" => Some(CameraControl_Roll),
        "zoom" => Some(CameraControl_Zoom),
        "exposure" => Some(CameraControl_Exposure),
        "focus" => Some(CameraControl_Focus),
        _ => None,
    }
}
