use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::mpsc;
use std::time::Duration;

use crate::camera::{CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption};

// ---------------------------------------------------------------------------
// EDSDK types
// ---------------------------------------------------------------------------

const EDS_MAX_NAME: usize = 256;
const EDS_ERR_OK: u32 = 0x00000000;

type EdsBaseRef = *mut std::ffi::c_void;
type EdsCameraListRef = EdsBaseRef;
type EdsCameraRef = EdsBaseRef;
type EdsStreamRef = EdsBaseRef;
type EdsEvfImageRef = EdsBaseRef;

// kEdsPropID_Evf_OutputDevice
const EDS_PROP_EVF_OUTPUT_DEVICE: u32 = 0x00000500;
// kEdsEvfOutputDevice_PC
const EDS_EVF_OUTPUT_DEVICE_PC: u32 = 2;

// Shooting property IDs
const PROP_TV:                   u32 = 0x00000400; // Shutter speed
const PROP_AV:                   u32 = 0x00000401; // Aperture
const PROP_ISO:                  u32 = 0x00000402; // ISO speed
const PROP_WHITE_BALANCE:        u32 = 0x00000106;
const PROP_COLOR_TEMPERATURE:    u32 = 0x00000107;
const PROP_METERING_MODE:        u32 = 0x00000413;
const PROP_AF_MODE:              u32 = 0x00000304;
const PROP_DRIVE_MODE:           u32 = 0x00000224;
const PROP_EXPOSURE_COMP:        u32 = 0x00000416;

#[repr(C)]
struct EdsPropertyDesc {
    form:         i32,
    access:       u32,
    num_elements: i32,
    prop_desc:    [i32; 128],
}

#[repr(C)]
struct EdsDeviceInfo {
    sz_port_name: [c_char; EDS_MAX_NAME],
    sz_device_description: [c_char; EDS_MAX_NAME],
    device_sub_type: u32,
    reserved: u32,
}

// ---------------------------------------------------------------------------
// EDSDK FFI
// ---------------------------------------------------------------------------

#[link(name = "EDSDK")]
extern "C" {
    fn EdsInitializeSDK() -> u32;
    fn EdsTerminateSDK() -> u32;
    fn EdsGetCameraList(out_camera_list_ref: *mut EdsCameraListRef) -> u32;
    fn EdsGetChildCount(in_ref: EdsBaseRef, out_count: *mut u32) -> u32;
    fn EdsGetChildAtIndex(in_ref: EdsBaseRef, in_index: i32, out_ref: *mut EdsBaseRef) -> u32;
    fn EdsGetDeviceInfo(in_camera_ref: EdsCameraRef, out_device_info: *mut EdsDeviceInfo) -> u32;
    fn EdsOpenSession(in_camera_ref: EdsCameraRef) -> u32;
    fn EdsCloseSession(in_camera_ref: EdsCameraRef) -> u32;
    fn EdsSetPropertyData(
        in_ref: EdsBaseRef,
        in_property_id: u32,
        in_param: i32,
        in_property_size: u32,
        in_property_data: *const std::ffi::c_void,
    ) -> u32;
    fn EdsCreateMemoryStream(in_buffer_size: u64, out_stream: *mut EdsStreamRef) -> u32;
    fn EdsCreateEvfImageRef(in_stream: EdsStreamRef, out_evf_image: *mut EdsEvfImageRef) -> u32;
    fn EdsDownloadEvfImage(in_camera_ref: EdsCameraRef, in_evf_image: EdsEvfImageRef) -> u32;
    fn EdsGetPointer(in_stream: EdsStreamRef, out_pointer: *mut *mut std::ffi::c_void) -> u32;
    fn EdsGetLength(in_stream: EdsStreamRef, out_length: *mut u64) -> u32;
    fn EdsGetPropertyData(
        in_ref: EdsBaseRef,
        in_property_id: u32,
        in_param: i32,
        in_property_size: u32,
        out_property_data: *mut std::ffi::c_void,
    ) -> u32;
    fn EdsGetPropertyDesc(
        in_ref: EdsBaseRef,
        in_property_id: u32,
        out_property_desc: *mut EdsPropertyDesc,
    ) -> u32;
    fn EdsRelease(in_ref: EdsBaseRef) -> u32;
    fn EdsGetEvent() -> u32;
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
    GetLiveViewFrame {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Canon EOS backend.
///
/// All EDSDK calls are dispatched to a dedicated OS thread that pumps
/// `EdsGetEvent()` on every tick. This is required because the EDSDK relies
/// on Windows messages internally and does not work correctly on threads
/// without a message pump (e.g. tokio worker threads).
pub struct CanonBackend {
    tx: mpsc::Sender<Command>,
}

impl CanonBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), CameraError>>();

        std::thread::Builder::new()
            .name("canon-sdk".to_string())
            .spawn(move || sdk_thread(cmd_rx, init_tx))
            .expect("failed to spawn canon-sdk thread");

        init_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))?;

        Ok(Self { tx: cmd_tx })
    }
}

impl Drop for CanonBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for CanonBackend {
    fn backend_id(&self) -> &str {
        "canon"
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

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::ListDevices { reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn connect(&self, device_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Connect {
                device_id: device_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn disconnect(&self, device_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Disconnect {
                device_id: device_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetParameters {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
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
// SDK thread
// ---------------------------------------------------------------------------

/// Runs on a dedicated OS thread. Initializes the EDSDK, pumps events every
/// 16 ms, and processes incoming commands.
fn sdk_thread(rx: mpsc::Receiver<Command>, init_tx: mpsc::Sender<Result<(), CameraError>>) {
    let err = unsafe { EdsInitializeSDK() };
    if err != EDS_ERR_OK {
        let _ = init_tx.send(Err(CameraError::SdkError(err)));
        return;
    }
    let _ = init_tx.send(Ok(()));
    drop(init_tx);

    // Camera refs for open sessions. Raw pointers never leave this thread.
    let mut connected: HashMap<String, EdsCameraRef> = HashMap::new();

    loop {
        unsafe { EdsGetEvent() };

        match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(Command::ListDevices { reply }) => {
                let _ = reply.send(list_devices_impl(&connected));
            }
            Ok(Command::IsConnected { device_id, reply }) => {
                let _ = reply.send(connected.contains_key(&device_id));
            }
            Ok(Command::Connect { device_id, reply }) => {
                let _ = reply.send(connect_impl(&device_id, &mut connected));
            }
            Ok(Command::Disconnect { device_id, reply }) => {
                let _ = reply.send(disconnect_impl(&device_id, &mut connected));
            }
            Ok(Command::GetParameters { device_id, reply }) => {
                let _ = reply.send(get_parameters_impl(&device_id, &connected));
            }
            Ok(Command::GetLiveViewFrame { device_id, reply }) => {
                let _ = reply.send(get_live_view_frame_impl(&device_id, &connected));
            }
            Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    // Close all open sessions before terminating.
    for (_, camera_ref) in connected.drain() {
        unsafe {
            EdsCloseSession(camera_ref);
            EdsRelease(camera_ref);
        }
    }

    unsafe { EdsTerminateSDK() };
}

// ---------------------------------------------------------------------------
// SDK operations (run exclusively on the SDK thread)
// ---------------------------------------------------------------------------

fn list_devices_impl(connected: &HashMap<String, EdsCameraRef>) -> Result<Vec<DeviceInfo>, CameraError> {
    let mut camera_list: EdsCameraListRef = std::ptr::null_mut();
    let err = unsafe { EdsGetCameraList(&mut camera_list) };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }

    let mut count: u32 = 0;
    let err = unsafe { EdsGetChildCount(camera_list, &mut count) };
    if err != EDS_ERR_OK {
        unsafe { EdsRelease(camera_list) };
        return Err(CameraError::SdkError(err));
    }

    let mut devices = Vec::with_capacity(count as usize);

    for i in 0..count {
        let mut camera_ref: EdsCameraRef = std::ptr::null_mut();
        if unsafe { EdsGetChildAtIndex(camera_list, i as i32, &mut camera_ref) } != EDS_ERR_OK {
            continue;
        }

        let mut info = EdsDeviceInfo {
            sz_port_name: [0; EDS_MAX_NAME],
            sz_device_description: [0; EDS_MAX_NAME],
            device_sub_type: 0,
            reserved: 0,
        };

        if unsafe { EdsGetDeviceInfo(camera_ref, &mut info) } == EDS_ERR_OK {
            let name = unsafe {
                CStr::from_ptr(info.sz_device_description.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };
            let port = unsafe {
                CStr::from_ptr(info.sz_port_name.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };
            let id = DeviceId::new("canon", &port).encode();
            let is_connected = connected.contains_key(port.as_ref() as &str);
            devices.push(DeviceInfo { id, name, connected: is_connected });
        }

        unsafe { EdsRelease(camera_ref) };
    }

    unsafe { EdsRelease(camera_list) };
    Ok(devices)
}

/// Finds a camera by its port name and returns its ref WITHOUT releasing it.
/// The caller is responsible for releasing the ref.
fn find_camera_ref(device_id: &str) -> Result<EdsCameraRef, CameraError> {
    let mut camera_list: EdsCameraListRef = std::ptr::null_mut();
    let err = unsafe { EdsGetCameraList(&mut camera_list) };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }

    let mut count: u32 = 0;
    unsafe { EdsGetChildCount(camera_list, &mut count) };

    let mut found: Option<EdsCameraRef> = None;

    for i in 0..count {
        let mut camera_ref: EdsCameraRef = std::ptr::null_mut();
        if unsafe { EdsGetChildAtIndex(camera_list, i as i32, &mut camera_ref) } != EDS_ERR_OK {
            continue;
        }

        let mut info = EdsDeviceInfo {
            sz_port_name: [0; EDS_MAX_NAME],
            sz_device_description: [0; EDS_MAX_NAME],
            device_sub_type: 0,
            reserved: 0,
        };

        if unsafe { EdsGetDeviceInfo(camera_ref, &mut info) } == EDS_ERR_OK {
            let port = unsafe {
                CStr::from_ptr(info.sz_port_name.as_ptr())
                    .to_string_lossy()
            };
            if port == device_id {
                found = Some(camera_ref);
                // Do NOT release — caller keeps ownership.
            } else {
                unsafe { EdsRelease(camera_ref) };
            }
        } else {
            unsafe { EdsRelease(camera_ref) };
        }
    }

    unsafe { EdsRelease(camera_list) };

    found.ok_or_else(|| CameraError::DeviceNotFound(device_id.to_string()))
}

fn connect_impl(
    device_id: &str,
    connected: &mut HashMap<String, EdsCameraRef>,
) -> Result<(), CameraError> {
    if connected.contains_key(device_id) {
        return Ok(()); // idempotent
    }

    let camera_ref = find_camera_ref(device_id)?;

    let err = unsafe { EdsOpenSession(camera_ref) };
    if err != EDS_ERR_OK {
        unsafe { EdsRelease(camera_ref) };
        return Err(CameraError::SdkError(err));
    }

    // Enable EVF output to the host PC once at connect time.
    let output_device: u32 = EDS_EVF_OUTPUT_DEVICE_PC;
    let err = unsafe {
        EdsSetPropertyData(
            camera_ref,
            EDS_PROP_EVF_OUTPUT_DEVICE,
            0,
            std::mem::size_of::<u32>() as u32,
            &output_device as *const u32 as *const std::ffi::c_void,
        )
    };
    if err != EDS_ERR_OK {
        unsafe {
            EdsCloseSession(camera_ref);
            EdsRelease(camera_ref);
        }
        return Err(CameraError::SdkError(err));
    }

    connected.insert(device_id.to_string(), camera_ref);
    Ok(())
}

fn disconnect_impl(
    device_id: &str,
    connected: &mut HashMap<String, EdsCameraRef>,
) -> Result<(), CameraError> {
    let camera_ref = connected
        .remove(device_id)
        .ok_or_else(|| CameraError::DeviceNotFound(device_id.to_string()))?;

    unsafe {
        EdsCloseSession(camera_ref);
        EdsRelease(camera_ref);
    }
    Ok(())
}

fn get_live_view_frame_impl(
    device_id: &str,
    connected: &HashMap<String, EdsCameraRef>,
) -> Result<Vec<u8>, CameraError> {
    let camera_ref = connected
        .get(device_id)
        .copied()
        .ok_or(CameraError::NotConnected)?;

    // Allocate an in-memory stream to receive the JPEG.
    let mut stream: EdsStreamRef = std::ptr::null_mut();
    let err = unsafe { EdsCreateMemoryStream(0, &mut stream) };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }

    // Create an EVF image ref bound to the stream.
    let mut evf_image: EdsEvfImageRef = std::ptr::null_mut();
    let err = unsafe { EdsCreateEvfImageRef(stream, &mut evf_image) };
    if err != EDS_ERR_OK {
        unsafe { EdsRelease(stream) };
        return Err(CameraError::SdkError(err));
    }

    // Download the current live view frame into the stream.
    let err = unsafe { EdsDownloadEvfImage(camera_ref, evf_image) };
    if err != EDS_ERR_OK {
        unsafe {
            EdsRelease(evf_image);
            EdsRelease(stream);
        }
        return Err(CameraError::SdkError(err));
    }

    // Read the JPEG bytes from the stream.
    let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut length: u64 = 0;
    unsafe {
        EdsGetPointer(stream, &mut ptr);
        EdsGetLength(stream, &mut length);
    }

    // SAFETY: ptr points to the SDK-managed buffer valid until EdsRelease(stream).
    let jpeg = unsafe {
        std::slice::from_raw_parts(ptr as *const u8, length as usize).to_vec()
    };

    unsafe {
        EdsRelease(evf_image);
        EdsRelease(stream);
    }

    Ok(jpeg)
}

// ---------------------------------------------------------------------------
// Parameter reading
// ---------------------------------------------------------------------------

fn get_parameters_impl(
    device_id: &str,
    connected: &HashMap<String, EdsCameraRef>,
) -> Result<Vec<CameraParameter>, CameraError> {
    let camera_ref = connected
        .get(device_id)
        .copied()
        .ok_or(CameraError::NotConnected)?;

    // Each entry: (api name, property ID, decode fn)
    let specs: &[(&str, u32, fn(i32) -> String)] = &[
        ("aperture",             PROP_AV,            decode_av),
        ("shutter_speed",        PROP_TV,            decode_tv),
        ("iso",                  PROP_ISO,           decode_iso),
        ("white_balance",        PROP_WHITE_BALANCE, decode_wb),
        ("color_temperature",    PROP_COLOR_TEMPERATURE, decode_color_temp),
        ("metering_mode",        PROP_METERING_MODE, decode_metering),
        ("af_mode",              PROP_AF_MODE,       decode_af),
        ("drive_mode",           PROP_DRIVE_MODE,    decode_drive),
        ("exposure_compensation",PROP_EXPOSURE_COMP, decode_ev),
    ];

    let mut result = Vec::new();

    for &(name, prop_id, decode) in specs {
        let mut desc = EdsPropertyDesc {
            form: 0,
            access: 0,
            num_elements: 0,
            prop_desc: [0; 128],
        };

        let err = unsafe { EdsGetPropertyDesc(camera_ref, prop_id, &mut desc) };

        // Skip: SDK error, no options, or read-only (access == 0).
        if err != EDS_ERR_OK || desc.num_elements <= 0 || desc.access == 0 {
            continue;
        }

        // Read current value.
        let mut current_code: i32 = 0;
        let err = unsafe {
            EdsGetPropertyData(
                camera_ref,
                prop_id,
                0,
                std::mem::size_of::<i32>() as u32,
                &mut current_code as *mut i32 as *mut std::ffi::c_void,
            )
        };
        let current = if err == EDS_ERR_OK {
            decode(current_code)
        } else {
            "Unknown".to_string()
        };

        let options = desc.prop_desc[..desc.num_elements as usize]
            .iter()
            .map(|&code| ParameterOption {
                label: decode(code),
                value: code,
            })
            .collect();

        result.push(CameraParameter {
            kind: name.to_string(),
            current,
            options,
        });
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Code tables
// ---------------------------------------------------------------------------

fn decode_av(code: i32) -> String {
    let label = match code {
        0x08 => "f/1",
        0x0B => "f/1.1",
        0x0C => "f/1.2",
        0x0D => "f/1.2",
        0x10 => "f/1.4",
        0x13 => "f/1.6",
        0x14 => "f/1.8",
        0x15 => "f/1.8",
        0x18 => "f/2",
        0x1B => "f/2.2",
        0x1C => "f/2.5",
        0x1D => "f/2.5",
        0x20 => "f/2.8",
        0x23 => "f/3.2",
        0x24 => "f/3.5",
        0x25 => "f/3.5",
        0x28 => "f/4",
        0x2B => "f/4.5",
        0x2C => "f/4.5",
        0x2D => "f/5",
        0x30 => "f/5.6",
        0x33 => "f/6.3",
        0x34 => "f/6.7",
        0x35 => "f/7.1",
        0x38 => "f/8",
        0x3B => "f/9",
        0x3C => "f/9.5",
        0x3D => "f/10",
        0x40 => "f/11",
        0x43 => "f/13",
        0x44 => "f/13",
        0x45 => "f/14",
        0x48 => "f/16",
        0x4B => "f/18",
        0x4C => "f/19",
        0x4D => "f/20",
        0x50 => "f/22",
        0x53 => "f/25",
        0x54 => "f/27",
        0x55 => "f/29",
        0x58 => "f/32",
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_tv(code: i32) -> String {
    let label = match code {
        0x0C => "30\"",
        0x10 => "20\"",
        0x13 => "15\"",
        0x14 => "13\"",
        0x15 => "10\"",
        0x18 => "8\"",
        0x1B => "6\"",
        0x1C => "5\"",
        0x1D => "4\"",
        0x20 => "3\"",
        0x23 => "2.5\"",
        0x24 => "2\"",
        0x25 => "1.6\"",
        0x28 => "1.3\"",
        0x2B => "1\"",
        0x2C => "0.8\"",
        0x2D => "0.6\"",
        0x30 => "1/2",
        0x33 => "1/2.5",
        0x34 => "1/3",
        0x35 => "1/3.2",
        0x38 => "1/4",
        0x3B => "1/5",
        0x3C => "1/6",
        0x3D => "1/6",
        0x40 => "1/8",
        0x43 => "1/10",
        0x44 => "1/10",
        0x45 => "1/13",
        0x48 => "1/15",
        0x4B => "1/20",
        0x4C => "1/20",
        0x4D => "1/25",
        0x50 => "1/30",
        0x53 => "1/40",
        0x54 => "1/45",
        0x55 => "1/50",
        0x58 => "1/60",
        0x5B => "1/80",
        0x5C => "1/90",
        0x5D => "1/100",
        0x60 => "1/125",
        0x63 => "1/160",
        0x64 => "1/180",
        0x65 => "1/200",
        0x68 => "1/250",
        0x6B => "1/320",
        0x6C => "1/350",
        0x6D => "1/400",
        0x70 => "1/500",
        0x73 => "1/640",
        0x74 => "1/750",
        0x75 => "1/800",
        0x78 => "1/1000",
        0x7B => "1/1250",
        0x7C => "1/1500",
        0x7D => "1/1600",
        0x80 => "1/2000",
        0x83 => "1/2500",
        0x84 => "1/3000",
        0x85 => "1/3200",
        0x88 => "1/4000",
        0x8B => "1/5000",
        0x8C => "1/6000",
        0x8D => "1/6400",
        0x90 => "1/8000",
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_iso(code: i32) -> String {
    match code {
        0x00 => "Auto".to_string(),
        0x40 => "50".to_string(),
        0x48 => "100".to_string(),
        0x4B => "125".to_string(),
        0x4D => "160".to_string(),
        0x50 => "200".to_string(),
        0x53 => "250".to_string(),
        0x55 => "320".to_string(),
        0x58 => "400".to_string(),
        0x5B => "500".to_string(),
        0x5D => "640".to_string(),
        0x60 => "800".to_string(),
        0x63 => "1000".to_string(),
        0x65 => "1250".to_string(),
        0x68 => "1600".to_string(),
        0x6B => "2000".to_string(),
        0x6D => "2500".to_string(),
        0x70 => "3200".to_string(),
        0x73 => "4000".to_string(),
        0x75 => "5000".to_string(),
        0x78 => "6400".to_string(),
        0x7B => "8000".to_string(),
        0x7D => "10000".to_string(),
        0x80 => "12800".to_string(),
        0x88 => "25600".to_string(),
        0x90 => "51200".to_string(),
        0x98 => "102400".to_string(),
        _ => format!("0x{code:02X}"),
    }
}

fn decode_wb(code: i32) -> String {
    let label = match code {
        0  => "Auto",
        1  => "Daylight",
        2  => "Cloudy",
        3  => "Tungsten",
        4  => "Fluorescent",
        5  => "Flash",
        6  => "Custom",
        8  => "Shade",
        9  => "Color temperature",
        10 => "Custom WB 1",
        11 => "Custom WB 2",
        12 => "Custom WB 3",
        20 => "Custom WB 4",
        21 => "Custom WB 5",
        -1 => "Auto (white priority)",
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_color_temp(code: i32) -> String {
    format!("{code}K")
}

fn decode_metering(code: i32) -> String {
    let label = match code {
        1 => "Spot",
        3 => "Evaluative",
        4 => "Partial",
        5 => "Center-weighted",
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_af(code: i32) -> String {
    let label = match code {
        0 => "One-Shot",
        1 => "AI Servo",
        2 => "AI Focus",
        3 => "Manual",
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_drive(code: i32) -> String {
    let label = match code {
        0  => "Single",
        1  => "Continuous high",
        2  => "Video",
        4  => "Self-timer 2s",
        5  => "Self-timer 10s",
        6  => "Silent single",
        7  => "AF servo high",
        10 => "Continuous low",
        16 => "Silent continuous",
        17 => "Silent continuous low",
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_ev(code: i32) -> String {
    match code {
        24  => "+3".to_string(),
        21  => "+2⅔".to_string(),
        19  => "+2⅓".to_string(),
        16  => "+2".to_string(),
        13  => "+1⅔".to_string(),
        11  => "+1⅓".to_string(),
        8   => "+1".to_string(),
        5   => "+⅔".to_string(),
        3   => "+⅓".to_string(),
        0   => "0".to_string(),
        -3  => "-⅓".to_string(),
        -5  => "-⅔".to_string(),
        -8  => "-1".to_string(),
        -11 => "-1⅓".to_string(),
        -13 => "-1⅔".to_string(),
        -16 => "-2".to_string(),
        -19 => "-2⅓".to_string(),
        -21 => "-2⅔".to_string(),
        -24 => "-3".to_string(),
        _   => format!("{code:+}"),
    }
}
