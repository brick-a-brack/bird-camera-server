use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::mpsc;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo,
    ParameterOption, ParameterType,
};

// ---------------------------------------------------------------------------
// C bridge constants — must match bridge.h
// ---------------------------------------------------------------------------

const WC_MAX_STR: usize = 256;
const WC_MAX_DEVICES: usize = 32;
const WC_MAX_PARAMS: usize = 20;
const WC_MAX_OPTIONS: usize = 4;
const WC_MAX_KIND: usize = 32;
const WC_MAX_LABEL: usize = 32;

// ---------------------------------------------------------------------------
// C bridge types
// ---------------------------------------------------------------------------

#[repr(C)]
struct WcDeviceInfo {
    unique_id: [c_char; WC_MAX_STR],
    name:      [c_char; WC_MAX_STR],
}

#[repr(C)]
struct WcParamOption {
    value: c_int,
    label: [c_char; WC_MAX_LABEL],
}

#[repr(C)]
struct WcParamDesc {
    kind:        [c_char; WC_MAX_KIND],
    current:     c_int,
    is_range:    c_int,
    min:         c_int,
    max:         c_int,
    step:        c_int,
    num_options: c_int,
    options:     [WcParamOption; WC_MAX_OPTIONS],
}

// ---------------------------------------------------------------------------
// C bridge FFI
// ---------------------------------------------------------------------------

extern "C" {
    fn wc_list_devices(out: *mut WcDeviceInfo, capacity: c_int) -> c_int;
    fn wc_open_session(unique_id: *const c_char) -> *mut c_void;
    fn wc_close_session(handle: *mut c_void);
    fn wc_capture_frame(
        handle:   *mut c_void,
        out_data: *mut *mut u8,
        out_size: *mut usize,
    ) -> c_int;
    fn wc_free_frame(data: *mut u8);
    fn wc_get_parameters(handle: *mut c_void, out: *mut WcParamDesc, capacity: c_int) -> c_int;
    fn wc_set_parameter(handle: *mut c_void, kind: *const c_char, value: c_int) -> c_int;
}

// ---------------------------------------------------------------------------
// Actor commands
// ---------------------------------------------------------------------------

enum Command {
    ListDevices {
        reply: mpsc::Sender<Result<Vec<DeviceInfo>, CameraError>>,
    },
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
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct WebcamMacosBackend {
    tx: mpsc::Sender<Command>,
}

impl WebcamMacosBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (tx, rx) = mpsc::channel::<Command>();

        std::thread::Builder::new()
            .name("webcam-macos".to_string())
            .spawn(move || actor_thread(rx))
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;

        Ok(Self { tx })
    }
}

impl Drop for WebcamMacosBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for WebcamMacosBackend {
    fn backend_id(&self) -> &str {
        "webcam-macos"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::ListDevices { reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Connect {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Disconnect {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn is_connected(&self, native_id: &str) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(Command::IsConnected {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetParameters {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::SetParameter {
                device_id: native_id.to_string(),
                param_type,
                value: value.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetLiveViewFrame {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }
}

// ---------------------------------------------------------------------------
// Actor thread
// ---------------------------------------------------------------------------

// Raw session handles live exclusively on this thread.
struct SessionHandle(*mut c_void);
unsafe impl Send for SessionHandle {}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        unsafe { wc_close_session(self.0) };
    }
}

fn actor_thread(rx: mpsc::Receiver<Command>) {
    let mut sessions: HashMap<String, SessionHandle> = HashMap::new();

    loop {
        match rx.recv() {
            Ok(Command::ListDevices { reply }) => {
                let _ = reply.send(list_devices_impl(&sessions));
            }
            Ok(Command::IsConnected { device_id, reply }) => {
                let _ = reply.send(sessions.contains_key(&device_id));
            }
            Ok(Command::Connect { device_id, reply }) => {
                let _ = reply.send(connect_impl(&device_id, &mut sessions));
            }
            Ok(Command::Disconnect { device_id, reply }) => {
                let _ = reply.send(disconnect_impl(&device_id, &mut sessions));
            }
            Ok(Command::GetParameters { device_id, reply }) => {
                let _ = reply.send(get_parameters_impl(&device_id, &sessions));
            }
            Ok(Command::SetParameter { device_id, param_type, value, reply }) => {
                let _ = reply.send(set_parameter_impl(&device_id, param_type, &value, &sessions));
            }
            Ok(Command::GetLiveViewFrame { device_id, reply }) => {
                let _ = reply.send(capture_frame_impl(&device_id, &sessions));
            }
            Ok(Command::Shutdown) | Err(_) => break,
        }
    }
    // SessionHandle::drop closes each session on cleanup.
}

// ---------------------------------------------------------------------------
// Bridge wrappers (run exclusively on the actor thread)
// ---------------------------------------------------------------------------

fn list_devices_impl(sessions: &HashMap<String, SessionHandle>) -> Result<Vec<DeviceInfo>, CameraError> {
    let mut buf = Vec::<WcDeviceInfo>::with_capacity(WC_MAX_DEVICES);
    let count = unsafe {
        buf.set_len(WC_MAX_DEVICES);
        wc_list_devices(buf.as_mut_ptr(), WC_MAX_DEVICES as c_int)
    };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let devices = buf
        .iter()
        .map(|d| {
            let native_id = unsafe { CStr::from_ptr(d.unique_id.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let name = unsafe { CStr::from_ptr(d.name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let id = DeviceId::new("webcam-macos", &native_id).encode();
            let connected = sessions.contains_key(&native_id);
            DeviceInfo { id, name, connected }
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
    let handle = unsafe { wc_open_session(c_id.as_ptr()) };

    if handle.is_null() {
        return Err(CameraError::DeviceNotFound(device_id.to_string()));
    }

    sessions.insert(device_id.to_string(), SessionHandle(handle));
    Ok(())
}

fn disconnect_impl(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    sessions
        .remove(device_id)
        .ok_or_else(|| CameraError::DeviceNotFound(device_id.to_string()))?;
    // SessionHandle::drop calls wc_close_session.
    Ok(())
}

fn get_parameters_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<CameraParameter>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut buf: Vec<WcParamDesc> = (0..WC_MAX_PARAMS)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();

    let count =
        unsafe { wc_get_parameters(handle, buf.as_mut_ptr(), WC_MAX_PARAMS as c_int) };

    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let params: Vec<CameraParameter> = buf[..buf.len().min(count as usize)]
        .iter()
        .filter_map(|d| {
            let c_kind = unsafe { CStr::from_ptr(d.kind.as_ptr()) }
                .to_string_lossy();
            let param_type = c_kind_to_param_type(&c_kind)?;

            if d.is_range != 0 {
                Some(CameraParameter::Range {
                    param_type,
                    current: d.current,
                    min: d.min,
                    max: d.max,
                    step: if d.step > 0 { d.step } else { 1 },
                })
            } else {
                let num_options = d.num_options as usize;
                let options: Vec<ParameterOption> = d.options[..num_options]
                    .iter()
                    .map(|o| {
                        let label = unsafe { CStr::from_ptr(o.label.as_ptr()) }
                            .to_string_lossy()
                            .into_owned();
                        ParameterOption { label, value: o.value.to_string() }
                    })
                    .collect();
                let current = d.current.to_string();
                Some(CameraParameter::Select { param_type, current, options })
            }
        })
        .collect();

    Ok(hide_value_params_in_auto_mode(params))
}

fn set_parameter_impl(
    device_id: &str,
    param_type: ParameterType,
    value: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;
    let c_kind = param_type_to_c_kind(param_type).ok_or(CameraError::NotSupported)?;
    let c_kind = CString::new(c_kind).map_err(|_| CameraError::NotSupported)?;
    let int_val: i32 = value.parse().map_err(|_| CameraError::NotSupported)?;

    let ret = unsafe { wc_set_parameter(handle, c_kind.as_ptr(), int_val) };
    if ret != 0 { Err(CameraError::NotSupported) } else { Ok(()) }
}

/// Removes value parameters whose corresponding *_mode parameter is set to "auto" ("1").
fn hide_value_params_in_auto_mode(params: Vec<CameraParameter>) -> Vec<CameraParameter> {
    // (value_type, mode_type): hide value_type when mode_type current == "1" (auto)
    const PAIRS: &[(ParameterType, ParameterType)] = &[
        (ParameterType::WhiteBalance, ParameterType::WhiteBalanceMode),
        (ParameterType::Exposure,     ParameterType::ExposureMode),
        (ParameterType::Gain,         ParameterType::GainMode),
        (ParameterType::Brightness,   ParameterType::BrightnessMode),
        (ParameterType::Contrast,     ParameterType::ContrastMode),
        (ParameterType::Hue,          ParameterType::HueMode),
        (ParameterType::Saturation,   ParameterType::SaturationMode),
        (ParameterType::Focus,        ParameterType::FocusMode),
        (ParameterType::Pan,          ParameterType::PanMode),
        (ParameterType::Tilt,         ParameterType::TiltMode),
        (ParameterType::Roll,         ParameterType::RollMode),
    ];

    // Collect the mode types that are currently in auto mode.
    let auto_modes: Vec<ParameterType> = params
        .iter()
        .filter_map(|p| {
            if let CameraParameter::Select { param_type, current, .. } = p {
                PAIRS
                    .iter()
                    .find(|&&(_, mode_type)| mode_type == *param_type && current == "1")
                    .map(|&(_, mode_type)| mode_type)
            } else {
                None
            }
        })
        .collect();

    if auto_modes.is_empty() {
        return params;
    }

    params
        .into_iter()
        .filter(|p| {
            let pt = match p {
                CameraParameter::Range { param_type, .. }
                | CameraParameter::Select { param_type, .. }
                | CameraParameter::RangeSelect { param_type, .. } => *param_type,
            };
            !PAIRS
                .iter()
                .any(|&(value_type, mode_type)| pt == value_type && auto_modes.contains(&mode_type))
        })
        .collect()
}

/// Maps a C bridge kind string (from wc_get_parameters) to a ParameterType.
/// Returns None for unknown kinds, which causes the parameter to be silently skipped.
fn c_kind_to_param_type(kind: &str) -> Option<ParameterType> {
    match kind {
        "brightness"                => Some(ParameterType::Brightness),
        "contrast"                  => Some(ParameterType::Contrast),
        "hue"                       => Some(ParameterType::Hue),
        "saturation"                => Some(ParameterType::Saturation),
        "sharpness"                 => Some(ParameterType::Sharpness),
        "gain"                      => Some(ParameterType::Gain),
        "backlight_compensation"    => Some(ParameterType::BacklightCompensation),
        "zoom_absolute"             => Some(ParameterType::Zoom),
        "white_balance_temperature" => Some(ParameterType::WhiteBalance),
        "white_balance_auto"        => Some(ParameterType::WhiteBalanceMode),
        "exposure_time_absolute"    => Some(ParameterType::Exposure),
        "exposure_auto"             => Some(ParameterType::ExposureMode),
        _ => None,
    }
}

/// Maps a ParameterType back to the C bridge string expected by wc_set_parameter.
fn param_type_to_c_kind(pt: ParameterType) -> Option<&'static str> {
    match pt {
        ParameterType::Brightness           => Some("brightness"),
        ParameterType::Contrast             => Some("contrast"),
        ParameterType::Hue                  => Some("hue"),
        ParameterType::Saturation           => Some("saturation"),
        ParameterType::Sharpness            => Some("sharpness"),
        ParameterType::Gain                 => Some("gain"),
        ParameterType::BacklightCompensation=> Some("backlight_compensation"),
        ParameterType::Zoom                 => Some("zoom_absolute"),
        ParameterType::WhiteBalance         => Some("white_balance_temperature"),
        ParameterType::WhiteBalanceMode     => Some("white_balance_auto"),
        ParameterType::Exposure             => Some("exposure_time_absolute"),
        ParameterType::ExposureMode         => Some("exposure_auto"),
        _ => None,
    }
}

fn capture_frame_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions
        .get(device_id)
        .ok_or(CameraError::NotConnected)?
        .0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size: usize = 0;

    let ret = unsafe { wc_capture_frame(handle, &mut data_ptr, &mut size) };

    if ret != 0 || data_ptr.is_null() {
        return Err(CameraError::SdkError(0xFFFF_FFFE));
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size).to_vec() };
    unsafe { wc_free_frame(data_ptr) };

    Ok(bytes)
}
