use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::mpsc;

use crate::camera::{CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo};

// ---------------------------------------------------------------------------
// C bridge (webcam_macos_bridge.m)
// ---------------------------------------------------------------------------

const WC_MAX_STR: usize = 256;
const WC_MAX_DEVICES: usize = 32;

#[repr(C)]
struct WcDeviceInfo {
    unique_id: [c_char; WC_MAX_STR],
    name: [c_char; WC_MAX_STR],
}

extern "C" {
    fn wc_list_devices(out: *mut WcDeviceInfo, capacity: c_int) -> c_int;
    fn wc_open_session(unique_id: *const c_char) -> *mut c_void;
    fn wc_close_session(handle: *mut c_void);
    fn wc_capture_frame(
        handle: *mut c_void,
        out_data: *mut *mut u8,
        out_size: *mut usize,
    ) -> c_int;
    fn wc_free_frame(data: *mut u8);
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
    GetLiveViewFrame {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct AvFoundationBackend {
    tx: mpsc::Sender<Command>,
}

impl AvFoundationBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (tx, rx) = mpsc::channel::<Command>();

        std::thread::Builder::new()
            .name("avfoundation".to_string())
            .spawn(move || actor_thread(rx))
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;

        Ok(Self { tx })
    }
}

impl Drop for AvFoundationBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for AvFoundationBackend {
    fn backend_id(&self) -> &str {
        "avfoundation"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::ListDevices { reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Connect {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Disconnect {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
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

    fn get_parameters(&self, _native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        Ok(vec![])
    }

    fn set_parameter(&self, _native_id: &str, _kind: &str, _value: i32) -> Result<(), CameraError> {
        Err(CameraError::NotSupported)
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetLiveViewFrame {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
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
    // SAFETY: buf has capacity for WC_MAX_DEVICES uninitialised structs; wc_list_devices
    // writes at most `capacity` entries and returns the count actually written.
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
            let id = DeviceId::new("avfoundation", &native_id).encode();
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

    // Copy bytes into a Vec then free the bridge-allocated buffer.
    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size).to_vec() };
    unsafe { wc_free_frame(data_ptr) };

    Ok(bytes)
}
