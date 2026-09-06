use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::mpsc;
use std::time::Duration;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo,
    ParameterOption, ParameterType,
};

// ---------------------------------------------------------------------------
// EDSDK types
// ---------------------------------------------------------------------------

const EDS_MAX_NAME: usize = 256;
const EDS_ERR_OK: u32 = 0x00000000;

/// Canon's USB vendor id — used to build the cross-backend dedup key so gphoto2
/// yields EOS bodies to this (higher-priority) backend.
const USB_VENDOR_CANON: u16 = 0x04A9;

type EdsBaseRef = *mut std::ffi::c_void;
type EdsCameraListRef = EdsBaseRef;
type EdsCameraRef = EdsBaseRef;
type EdsStreamRef = EdsBaseRef;
type EdsEvfImageRef = EdsBaseRef;
type EdsDirectoryItemRef = EdsBaseRef;

// kEdsPropID_Evf_OutputDevice
const EDS_PROP_EVF_OUTPUT_DEVICE: u32 = 0x00000500;
// kEdsEvfOutputDevice_PC
const EDS_EVF_OUTPUT_DEVICE_PC: u32 = 2;

// kEdsCameraCommand_ExtendShutDownTimer
const CMD_EXTEND_SHUTDOWN_TIMER: u32 = 0x00000001;
// kEdsCameraCommand_PressShutterButton
const CMD_PRESS_SHUTTER: u32 = 0x00000004;
// kEdsCameraCommand_ShutterButton_Completely / _OFF
const SHUTTER_COMPLETELY: i32 = 0x00000003;
const SHUTTER_OFF: i32 = 0x00000000;

// kEdsObjectEvent_All / kEdsObjectEvent_DirItemRequestTransfer
const OBJ_EVENT_ALL: u32 = 0x00000200;
const OBJ_EVENT_DIR_ITEM_REQUEST_TRANSFER: u32 = 0x00000208;

// kEdsStateEvent_All / kEdsStateEvent_Shutdown / kEdsStateEvent_WillSoonShutDown
const STATE_EVENT_ALL: u32 = 0x00000300;
const STATE_EVENT_SHUTDOWN: u32 = 0x00000301;
const STATE_EVENT_WILL_SOON_SHUTDOWN: u32 = 0x00000303;

// kEdsPropID_SaveTo / kEdsSaveTo_Host
const PROP_SAVE_TO: u32 = 0x0000000b;
const SAVE_TO_HOST: u32 = 2;

// ImageQuality is two 16-bit components: high word (bits 16-31) = 1st image,
// low word (bits 0-15) = 2nd image. Each word is [size:8 | compression:8].
// A RAW component is marked by compression byte 0x64; 0xff0f means "no image".
const IQ_RAW_MARKER: u32 = 0x64; // compression byte value that denotes RAW
const IQ_NO_IMAGE: u32 = 0xff0f; // "no second image" word (size 0xff, comp 0x0f)
const IQ_DEFAULT_JPEG: i32 = 0x0013ff0f; // L JPEG Fine, JPEG-only (fallback)

// Capture timeout. Must cover the longest non-bulb exposure (30 s) plus the
// long-exposure noise-reduction "dark frame" (another ~30 s when enabled on the
// body) plus processing and download. A tight 30 s value made 30 s exposures
// always time out mid-shot.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

// Keep-alive interval: reset the sleep timer every 30 s for connected cameras.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

// Shooting property IDs — from EDSDKTypes.h
const PROP_DRIVE_MODE:           u32 = 0x00000401; // kEdsPropID_DriveMode
const PROP_ISO:                  u32 = 0x00000402; // kEdsPropID_ISOSpeed
const PROP_AV:                   u32 = 0x00000405; // kEdsPropID_Av
const PROP_TV:                   u32 = 0x00000406; // kEdsPropID_Tv
const PROP_EXPOSURE_COMP:        u32 = 0x00000407; // kEdsPropID_ExposureCompensation
const PROP_IMAGE_QUALITY:        u32 = 0x00000100; // kEdsPropID_ImageQuality
const PROP_WHITE_BALANCE:        u32 = 0x00000106; // kEdsPropID_WhiteBalance
const PROP_COLOR_TEMPERATURE:    u32 = 0x00000107; // kEdsPropID_ColorTemperature
const PROP_EVF_ZOOM:             u32 = 0x00000507; // kEdsPropID_Evf_Zoom
const PROP_EVF_ZOOM_POSITION:    u32 = 0x00000508; // kEdsPropID_Evf_ZoomPosition
const PROP_EVF_COORDINATE_SYS:   u32 = 0x00000540; // kEdsPropID_Evf_CoordinateSystem

#[repr(C)]
struct EdsDirectoryItemInfo {
    size:       u64,
    is_folder:  u32, // EdsBool
    group_id:   u32,
    option:     u32,
    sz_file_name: [c_char; EDS_MAX_NAME],
    format:     u32,
    date_time:  u32,
}

#[repr(C)]
struct EdsCapacity {
    number_of_free_clusters: i32,
    bytes_per_sector:        i32,
    reset:                   u32, // EdsBool
}

#[repr(C)]
struct EdsPoint {
    x: i32,
    y: i32,
}

#[repr(C)]
struct EdsSize {
    width:  i32,
    height: i32,
}

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
    fn EdsSendCommand(
        in_camera_ref: EdsCameraRef,
        in_command: u32,
        in_param: i32,
    ) -> u32;
    fn EdsRelease(in_ref: EdsBaseRef) -> u32;
    fn EdsGetEvent() -> u32;
    fn EdsSetObjectEventHandler(
        in_camera_ref: EdsCameraRef,
        in_event: u32,
        in_handler: Option<unsafe extern "C" fn(u32, EdsBaseRef, *mut std::ffi::c_void) -> u32>,
        in_context: *mut std::ffi::c_void,
    ) -> u32;
    fn EdsSetCameraStateEventHandler(
        in_camera_ref: EdsCameraRef,
        in_event: u32,
        in_handler: Option<unsafe extern "C" fn(u32, u32, *mut std::ffi::c_void) -> u32>,
        in_context: *mut std::ffi::c_void,
    ) -> u32;
    fn EdsGetDirectoryItemInfo(
        in_dir_item_ref: EdsDirectoryItemRef,
        out_dir_item_info: *mut EdsDirectoryItemInfo,
    ) -> u32;
    fn EdsDownload(
        in_dir_item_ref: EdsDirectoryItemRef,
        in_read_size: u64,
        out_stream: EdsStreamRef,
    ) -> u32;
    fn EdsDownloadComplete(in_dir_item_ref: EdsDirectoryItemRef) -> u32;
    fn EdsDownloadCancel(in_dir_item_ref: EdsDirectoryItemRef) -> u32;
    fn EdsSetCapacity(in_camera_ref: EdsCameraRef, in_capacity: EdsCapacity) -> u32;
}

// ---------------------------------------------------------------------------
// Object event callback
// ---------------------------------------------------------------------------

// Only accessed on the canon-sdk thread.
thread_local! {
    static PENDING_DIR_ITEM: RefCell<Option<EdsDirectoryItemRef>> = RefCell::new(None);
    // Camera refs that have reported a shutdown/disconnect state event.
    static SHUTDOWN_CAMERA_REFS: RefCell<Vec<EdsCameraRef>> = RefCell::new(Vec::new());
    // Last zoom level set via set_parameter_impl, keyed by device_id.
    // Used as fallback in get_parameters_impl when the EVF frame is not yet ready.
    static EVF_ZOOM_CACHE: RefCell<std::collections::HashMap<String, u32>> = RefCell::new(std::collections::HashMap::new());
    // Last known coordinate system and position from a successful EVF download,
    // keyed by device_id: (coord_width, coord_height, pos_x, pos_y).
    static EVF_COORD_CACHE: RefCell<std::collections::HashMap<String, (i32, i32, i32, i32)>> = RefCell::new(std::collections::HashMap::new());
}

/// Called by the EDSDK on the SDK thread when a camera state event fires.
///
/// On `kEdsStateEvent_Shutdown` or `kEdsStateEvent_WillSoonShutDown` the camera
/// has physically disconnected. We record its ref (passed as context) so the
/// SDK thread loop can remove it from the connected map on the next tick.
unsafe extern "C" fn state_event_callback(
    event: u32,
    _event_data: u32,
    context: *mut std::ffi::c_void,
) -> u32 {
    if event == STATE_EVENT_SHUTDOWN || event == STATE_EVENT_WILL_SOON_SHUTDOWN {
        let camera_ref = context as EdsCameraRef;
        if !camera_ref.is_null() {
            SHUTDOWN_CAMERA_REFS.with(|s| s.borrow_mut().push(camera_ref));
        }
    }
    EDS_ERR_OK
}

/// Called by the EDSDK on the SDK thread when a camera object event fires.
///
/// On `kEdsObjectEvent_DirItemRequestTransfer` the camera is ready to transfer
/// the newly shot image. We store the ref in a thread-local so the blocking
/// capture loop can pick it up on the next tick.
unsafe extern "C" fn object_event_callback(
    event: u32,
    object_ref: EdsBaseRef,
    _context: *mut std::ffi::c_void,
) -> u32 {
    if event == OBJ_EVENT_DIR_ITEM_REQUEST_TRANSFER {
        PENDING_DIR_ITEM.with(|p| *p.borrow_mut() = Some(object_ref));
        // Do NOT release — the download will take ownership.
    } else if !object_ref.is_null() {
        EdsRelease(object_ref);
    }
    EDS_ERR_OK
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
    SetParameter {
        device_id: String,
        prop_id: u32,
        value: i32, // already parsed from the string value at the trait boundary
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    SetIsoManual {
        device_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    CapturePhoto {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    SetEvfZoomAxis {
        device_id: String,
        axis_is_x: bool,
        value: i32,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    /// Graceful shutdown: close all sessions and terminate the SDK on the SDK
    /// thread, then ack so the caller can exit without leaving a body claimed.
    /// Driven by `CanonBackend::shutdown`.
    PrepareExit {
        ack: mpsc::Sender<()>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Canon EOS backend.
///
/// All EDSDK calls are dispatched to a dedicated OS thread that pumps
/// `EdsGetEvent()` on every tick. This is required because the EDSDK is not
/// thread-safe and must run on a single OS thread with an event pump
/// (Windows message loop on Windows, run loop on macOS).
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

    /// Above the generic backends: the EDSDK gives native EVF live view and the
    /// full Canon parameter set, so it wins dedup over gphoto2 for the same body.
    fn dedup_priority(&self) -> i32 {
        10
    }

    /// Releases the EDSDK before the process exits: asks the SDK thread to close
    /// all sessions and terminate the SDK, then waits — bounded — for it to finish
    /// so an abrupt Ctrl-C doesn't leave a body claimed. The thread may be mid-call,
    /// so the wait is capped and the exit never hangs.
    fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Command::PrepareExit { ack: ack_tx }).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(3));
        }
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

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        // IsoAuto maps to PROP_ISO: 0x00 = auto; switching to manual uses the first available
        // non-auto ISO value queried live from the camera (avoids hardcoding a value the camera
        // may not support).
        if param_type == ParameterType::IsoAuto {
            let (reply_tx, reply_rx) = mpsc::channel();
            if value == "true" {
                self.tx
                    .send(Command::SetParameter {
                        device_id: native_id.to_string(),
                        prop_id: PROP_ISO,
                        value: 0x00,
                        reply: reply_tx,
                    })
                    .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
            } else {
                self.tx
                    .send(Command::SetIsoManual {
                        device_id: native_id.to_string(),
                        reply: reply_tx,
                    })
                    .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
            }
            return reply_rx
                .recv()
                .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)));
        }

let value: i32 = value.parse().map_err(|_| CameraError::NotSupported)?;

        // Zoom position axes are composite (EdsPoint) — handled via a dedicated command.
        match param_type {
            ParameterType::LiveViewPan | ParameterType::LiveViewTilt => {
                let (reply_tx, reply_rx) = mpsc::channel();
                self.tx
                    .send(Command::SetEvfZoomAxis {
                        device_id: native_id.to_string(),
                        axis_is_x: param_type == ParameterType::LiveViewPan,
                        value,
                        reply: reply_tx,
                    })
                    .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
                return reply_rx
                    .recv()
                    .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)));
            }
            _ => {}
        }

        let prop_id = type_to_prop_id(param_type).ok_or(CameraError::NotSupported)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::SetParameter {
                device_id: native_id.to_string(),
                prop_id,
                value,
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::CapturePhoto {
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
    let mut last_keepalive = std::time::Instant::now();

    loop {
        unsafe { EdsGetEvent() };

        // Remove cameras that have physically disconnected (state event fired).
        SHUTDOWN_CAMERA_REFS.with(|s| {
            for stale_ref in s.borrow_mut().drain(..) {
                if let Some(device_id) = connected
                    .iter()
                    .find(|(_, &r)| r == stale_ref)
                    .map(|(id, _)| id.clone())
                {
                    eprintln!("[canon] camera {device_id} disconnected unexpectedly");
                    connected.remove(&device_id);
                    unsafe {
                        EdsCloseSession(stale_ref);
                        EdsRelease(stale_ref);
                    }
                }
            }
        });

        // Reset the auto-power-off timer for every connected camera every 30 s.
        if last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            for &camera_ref in connected.values() {
                unsafe { EdsSendCommand(camera_ref, CMD_EXTEND_SHUTDOWN_TIMER, 0) };
            }
            last_keepalive = std::time::Instant::now();
        }

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
            Ok(Command::SetParameter { device_id, prop_id, value, reply }) => {
                let _ = reply.send(set_parameter_impl(&device_id, prop_id, value, &connected));
            }
            Ok(Command::SetIsoManual { device_id, reply }) => {
                let _ = reply.send(set_iso_manual_impl(&device_id, &connected));
            }
            Ok(Command::CapturePhoto { device_id, reply }) => {
                let _ = reply.send(capture_photo_impl(&device_id, &connected));
            }
            Ok(Command::SetEvfZoomAxis { device_id, axis_is_x, value, reply }) => {
                let _ = reply.send(set_evf_zoom_axis_impl(&device_id, axis_is_x, value, &connected));
            }
            // Graceful shutdown: release everything here (EDSDK calls are only
            // valid on this thread), ack, then return so the process can exit
            // without leaving a body claimed. `return` skips the post-loop
            // teardown below so the SDK isn't terminated twice.
            Ok(Command::PrepareExit { ack }) => {
                for (_, camera_ref) in connected.drain() {
                    unsafe {
                        EdsCloseSession(camera_ref);
                        EdsRelease(camera_ref);
                    }
                }
                unsafe { EdsTerminateSDK() };
                let _ = ack.send(());
                return;
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
            // Dedup key so gphoto2 yields EOS bodies to the EDSDK backend. EDSDK
            // only lists Canon bodies it supports, so unsupported Canons (no
            // entry here) stay on gphoto2.
            let dedup_key = Some(crate::camera::dedup_key(USB_VENDOR_CANON, &name));
            devices.push(DeviceInfo { id, name, connected: is_connected, dedup_key });
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

/// Returns true when the ImageQuality code's first image component is RAW.
fn image_quality_has_raw(code: i32) -> bool {
    (code as u32 >> 16) & 0xFF == IQ_RAW_MARKER
}

/// Force the connected camera into a JPEG-only ImageQuality.
///
/// Some bodies are left set to RAW or RAW+JPEG, and host-side capture only
/// downloads JPEG — a RAW-only setting makes `capture_photo` fail. If the
/// current setting includes RAW, switch to a JPEG-only mode: keep the existing
/// JPEG component when the camera is on RAW+JPEG, otherwise pick a supported
/// JPEG option from the property description (falling back to L JPEG Fine).
///
/// Best-effort: any SDK error is logged and ignored so connect still succeeds.
fn force_jpeg_only_quality(camera_ref: EdsCameraRef) {
    let mut current: i32 = 0;
    let err = unsafe {
        EdsGetPropertyData(
            camera_ref,
            PROP_IMAGE_QUALITY,
            0,
            std::mem::size_of::<i32>() as u32,
            &mut current as *mut i32 as *mut std::ffi::c_void,
        )
    };
    if err != EDS_ERR_OK {
        eprintln!("canon: could not read ImageQuality ({err:#010x}); leaving as-is");
        return;
    }

    if !image_quality_has_raw(current) {
        return; // already JPEG-only, nothing to do
    }

    // Prefer keeping the camera's own JPEG component (RAW+JPEG → JPEG-only).
    let jpeg_word = (current as u32) & 0xFFFF;
    let mut target = if jpeg_word != IQ_NO_IMAGE {
        ((jpeg_word << 16) | IQ_NO_IMAGE) as i32
    } else {
        IQ_DEFAULT_JPEG
    };

    // Validate against the supported values; if our target is not offered,
    // fall back to the first JPEG-only option the body advertises.
    let mut desc = EdsPropertyDesc { form: 0, access: 0, num_elements: 0, prop_desc: [0; 128] };
    let derr = unsafe { EdsGetPropertyDesc(camera_ref, PROP_IMAGE_QUALITY, &mut desc) };
    if derr == EDS_ERR_OK && desc.num_elements > 0 {
        let options = &desc.prop_desc[..desc.num_elements as usize];
        if !options.contains(&target) {
            match options.iter().copied().find(|&c| !image_quality_has_raw(c)) {
                Some(jpeg) => target = jpeg,
                None => {
                    eprintln!("canon: camera offers no JPEG-only ImageQuality; leaving as-is");
                    return;
                }
            }
        }
    }

    let err = unsafe {
        EdsSetPropertyData(
            camera_ref,
            PROP_IMAGE_QUALITY,
            0,
            std::mem::size_of::<i32>() as u32,
            &target as *const i32 as *const std::ffi::c_void,
        )
    };
    if err != EDS_ERR_OK {
        eprintln!("canon: could not force JPEG-only ImageQuality ({err:#010x})");
    }
}

fn connect_impl(
    device_id: &str,
    connected: &mut HashMap<String, EdsCameraRef>,
) -> Result<(), CameraError> {
    // Close any existing session before opening a new one. This ensures that a
    // reconnect after a physical disconnect (stale ref) or after the camera was
    // already logically connected always results in a clean, fresh session.
    if let Some(old_ref) = connected.remove(device_id) {
        unsafe {
            EdsCloseSession(old_ref); // may fail if camera already gone — ignore
            EdsRelease(old_ref);
        }
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

    // Force single-shot drive mode — stop motion captures one frame at a time.
    let drive_single: i32 = 0;
    unsafe {
        EdsSetPropertyData(
            camera_ref,
            PROP_DRIVE_MODE,
            0,
            std::mem::size_of::<i32>() as u32,
            &drive_single as *const i32 as *const std::ffi::c_void,
        )
    };

    // Direct captured photos to the host so that DirItemRequestTransfer fires.
    let save_to: u32 = SAVE_TO_HOST;
    unsafe {
        EdsSetPropertyData(
            camera_ref,
            PROP_SAVE_TO,
            0,
            std::mem::size_of::<u32>() as u32,
            &save_to as *const u32 as *const std::ffi::c_void,
        )
    };

    // Host-side capture only downloads JPEG. If the body is set to RAW or
    // RAW+JPEG, capture would fail — force it to a JPEG-only quality.
    force_jpeg_only_quality(camera_ref);

    // Inform the camera that the host has ample free space.
    unsafe {
        EdsSetCapacity(
            camera_ref,
            EdsCapacity {
                number_of_free_clusters: 0x7FFF_FFFF,
                bytes_per_sector: 512,
                reset: 1,
            },
        )
    };

    // Register the object event handler so DirItemRequestTransfer reaches us.
    unsafe {
        EdsSetObjectEventHandler(
            camera_ref,
            OBJ_EVENT_ALL,
            Some(object_event_callback),
            std::ptr::null_mut(),
        )
    };

    // Register the state event handler to detect physical disconnects.
    // Pass camera_ref as context so the callback can identify which camera disconnected.
    unsafe {
        EdsSetCameraStateEventHandler(
            camera_ref,
            STATE_EVENT_ALL,
            Some(state_event_callback),
            camera_ref,
        )
    };

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

    EVF_ZOOM_CACHE.with(|c| c.borrow_mut().remove(device_id));
    EVF_COORD_CACHE.with(|c| c.borrow_mut().remove(device_id));

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

    // RangeSelect: ordered numeric progression (aperture, ISO, …).
    // Select:      arbitrary discrete choices (WB, AF mode, …).
    // The filter predicate drops options that should not be exposed (e.g. RAW formats).
    type Spec = (ParameterType, u32, fn(i32) -> String, fn(i32) -> bool);

    let range_select_specs: &[Spec] = &[
        (ParameterType::Aperture,             PROP_AV,            decode_av,           |_| true),
        (ParameterType::ExposureCompensation, PROP_EXPOSURE_COMP, decode_ev,           |_| true),
    ];

    let select_specs: &[Spec] = &[
        // RAW-containing formats are excluded: capture returns only JPEG.
        // All RAW codes have byte 2 == 0x64 (e.g. 0x0064ff0f, 0x00640013, …).
        (ParameterType::ImageQuality,    PROP_IMAGE_QUALITY,     decode_image_quality, |c| !image_quality_has_raw(c)),
        (ParameterType::WhiteBalance,    PROP_WHITE_BALANCE,     decode_wb,            |_| true),
        (ParameterType::ColorTemperature,PROP_COLOR_TEMPERATURE, decode_color_temp,    |_| true),
    ];

    let mut result = Vec::new();

    for (specs, is_range_select) in [
        (range_select_specs as &[Spec], true),
        (select_specs        as &[Spec], false),
    ] {
        for &(param_type, prop_id, decode, keep) in specs {
            let mut desc = EdsPropertyDesc {
                form: 0, access: 0, num_elements: 0, prop_desc: [0; 128],
            };

            let err = unsafe { EdsGetPropertyDesc(camera_ref, prop_id, &mut desc) };
            if err != EDS_ERR_OK || desc.num_elements <= 0 || desc.access == 0 {
                continue;
            }

            let mut current_code: i32 = 0;
            let err = unsafe {
                EdsGetPropertyData(
                    camera_ref, prop_id, 0,
                    std::mem::size_of::<i32>() as u32,
                    &mut current_code as *mut i32 as *mut std::ffi::c_void,
                )
            };
            let current = if err == EDS_ERR_OK {
                current_code.to_string()
            } else {
                "0".to_string()
            };

            let options = desc.prop_desc[..desc.num_elements as usize]
                .iter()
                .copied()
                .filter(|&code| keep(code))
                .map(|code| ParameterOption {
                    label: decode(code),
                    value: code.to_string(),
                })
                .collect::<Vec<_>>();

            if options.is_empty() {
                continue;
            }

            result.push(if is_range_select {
                CameraParameter::RangeSelect { param_type, current, options, disabled: false }
            } else {
                CameraParameter::Select { param_type, current, options, disabled: false }
            });
        }
    }

    // ShutterSpeed: Bulb (0x0C) excluded — not usable for stop motion.
    {
        let mut tv_desc = EdsPropertyDesc { form: 0, access: 0, num_elements: 0, prop_desc: [0; 128] };
        let tv_err = unsafe { EdsGetPropertyDesc(camera_ref, PROP_TV, &mut tv_desc) };
        if tv_err == EDS_ERR_OK && tv_desc.num_elements > 0 && tv_desc.access != 0 {
            let mut current_code: i32 = 0;
            let read_err = unsafe {
                EdsGetPropertyData(
                    camera_ref, PROP_TV, 0,
                    std::mem::size_of::<i32>() as u32,
                    &mut current_code as *mut i32 as *mut std::ffi::c_void,
                )
            };
            if read_err != EDS_ERR_OK { current_code = 0; }

            let options: Vec<ParameterOption> = tv_desc.prop_desc[..tv_desc.num_elements as usize]
                .iter()
                .filter(|&&code| code != 0x0C)
                .map(|&code| ParameterOption {
                    label: decode_tv(code),
                    value: code.to_string(),
                })
                .collect();

            result.push(CameraParameter::RangeSelect {
                param_type: ParameterType::ShutterSpeed,
                current: current_code.to_string(),
                options,
                disabled: false,
            });
        }
    }

    // ISO: IsoAuto boolean + Iso RangeSelect (disabled when auto, 0x00 excluded from options).
    {
        let mut iso_desc = EdsPropertyDesc { form: 0, access: 0, num_elements: 0, prop_desc: [0; 128] };
        let iso_err = unsafe { EdsGetPropertyDesc(camera_ref, PROP_ISO, &mut iso_desc) };
        if iso_err == EDS_ERR_OK && iso_desc.num_elements > 0 && iso_desc.access != 0 {
            let mut current_code: i32 = 0;
            let read_err = unsafe {
                EdsGetPropertyData(
                    camera_ref, PROP_ISO, 0,
                    std::mem::size_of::<i32>() as u32,
                    &mut current_code as *mut i32 as *mut std::ffi::c_void,
                )
            };
            if read_err != EDS_ERR_OK { current_code = 0; }

            let iso_auto = current_code == 0x00;

            result.push(CameraParameter::Boolean {
                param_type: ParameterType::IsoAuto,
                current: iso_auto,
                disabled: false,
            });

            let options: Vec<ParameterOption> = iso_desc.prop_desc[..iso_desc.num_elements as usize]
                .iter()
                .filter(|&&code| code != 0x00)
                .map(|&code| ParameterOption {
                    label: decode_iso(code),
                    value: code.to_string(),
                })
                .collect();

            let iso_current = if iso_auto {
                options.first().map(|o| o.value.clone()).unwrap_or_default()
            } else {
                current_code.to_string()
            };

            result.push(CameraParameter::RangeSelect {
                param_type: ParameterType::Iso,
                current: iso_current,
                options,
                disabled: iso_auto,
            });
        }
    }

    // EVF zoom position — read coordinate system for max range, current point for value.
    // EVF zoom level + position must be read from an evfImageRef (not cameraRef).
    // On many bodies (e.g. 600D), reading them from cameraRef returns NOT_SUPPORTED.
    // Zoom Select is always added; position Range only when coordinate system is readable.
    //
    // evf_zoom is pre-seeded from the cache set by set_parameter_impl so that a
    // stale or unavailable EVF frame (OBJECT_NOTREADY) does not falsely report Fit.
    let mut evf_zoom: u32 = EVF_ZOOM_CACHE.with(|c| {
        c.borrow().get(device_id).copied().unwrap_or(1)
    });
    let mut coord_sys = EdsSize { width: 0, height: 0 };
    let mut pos = EdsPoint { x: 0, y: 0 };
    let mut coord_sys_ok = false;

    let mut stream: EdsStreamRef = std::ptr::null_mut();
    if unsafe { EdsCreateMemoryStream(0, &mut stream) } == EDS_ERR_OK {
        let mut evf_image: EdsEvfImageRef = std::ptr::null_mut();
        if unsafe { EdsCreateEvfImageRef(stream, &mut evf_image) } == EDS_ERR_OK {
            // Retry a few times on OBJECT_NOTREADY — the live view loop may have
            // just consumed the latest frame leaving none buffered yet.
            let mut dl_err = unsafe { EdsDownloadEvfImage(camera_ref, evf_image) };
            let mut retries = 0;
            while dl_err == 0x0000_A102 && retries < 4 {
                unsafe { EdsGetEvent() };
                std::thread::sleep(Duration::from_millis(16));
                dl_err = unsafe { EdsDownloadEvfImage(camera_ref, evf_image) };
                retries += 1;
            }
            if dl_err == EDS_ERR_OK {
                // Read zoom from EVF as the primary source.
                // Guard: if EVF reports Fit (1) but the cache holds a non-1 value,
                // the EVF frame is stale (mid-transition) — keep the cached value.
                let mut reported_zoom: u32 = 0;
                let zoom_err = unsafe {
                    EdsGetPropertyData(
                        evf_image, PROP_EVF_ZOOM, 0,
                        std::mem::size_of::<u32>() as u32,
                        &mut reported_zoom as *mut u32 as *mut std::ffi::c_void,
                    )
                };
                if zoom_err == EDS_ERR_OK {
                    let cached = EVF_ZOOM_CACHE.with(|c| c.borrow().get(device_id).copied());
                    // Trust EVF value unless it says Fit while cache says otherwise
                    if reported_zoom != 1 || cached.is_none() || cached == Some(1) {
                        evf_zoom = reported_zoom;
                    }
                }

                let cs_err = unsafe {
                    EdsGetPropertyData(
                        evf_image, PROP_EVF_COORDINATE_SYS, 0,
                        std::mem::size_of::<EdsSize>() as u32,
                        &mut coord_sys as *mut EdsSize as *mut std::ffi::c_void,
                    )
                };

                if cs_err == EDS_ERR_OK && coord_sys.width > 0 && coord_sys.height > 0 {
                    unsafe {
                        EdsGetPropertyData(
                            evf_image, PROP_EVF_ZOOM_POSITION, 0,
                            std::mem::size_of::<EdsPoint>() as u32,
                            &mut pos as *mut EdsPoint as *mut std::ffi::c_void,
                        )
                    };
                    coord_sys_ok = true;
                    EVF_COORD_CACHE.with(|c| {
                        c.borrow_mut().insert(
                            device_id.to_string(),
                            (coord_sys.width, coord_sys.height, pos.x, pos.y),
                        );
                    });
                }
            }
            unsafe { EdsRelease(evf_image) };
        }
        unsafe { EdsRelease(stream) };
    }

    // If the EVF download failed, fall back to the last known coord_sys so
    // pan/tilt controls don't disappear on a transient OBJECT_NOTREADY.
    if !coord_sys_ok {
        EVF_COORD_CACHE.with(|c| {
            if let Some(&(cw, ch, px, py)) = c.borrow().get(device_id) {
                coord_sys = EdsSize { width: cw, height: ch };
                pos = EdsPoint { x: px, y: py };
                coord_sys_ok = true;
            }
        });
    }

    result.push(CameraParameter::Select {
        param_type: ParameterType::LiveViewZoom,
        current: evf_zoom.to_string(),
        options: vec![
            ParameterOption { label: "Fit".to_string(), value: "1".to_string() },
            ParameterOption { label: "5x".to_string(),  value: "5".to_string() },
            ParameterOption { label: "6x".to_string(),  value: "6".to_string() },
            ParameterOption { label: "10x".to_string(), value: "10".to_string() },
            ParameterOption { label: "15x".to_string(), value: "15".to_string() },
        ],
        disabled: false,
    });

    if coord_sys_ok {
        let pan_tilt_disabled = evf_zoom == 1;
        result.push(CameraParameter::Range {
            param_type: ParameterType::LiveViewPan,
            current: pos.x, min: 0, max: coord_sys.width, step: 1, disabled: pan_tilt_disabled,
        });
        result.push(CameraParameter::Range {
            param_type: ParameterType::LiveViewTilt,
            current: pos.y, min: 0, max: coord_sys.height, step: 1, disabled: pan_tilt_disabled,
        });
    }

    Ok(result)
}

fn capture_photo_impl(
    device_id: &str,
    connected: &HashMap<String, EdsCameraRef>,
) -> Result<Vec<u8>, CameraError> {
    let camera_ref = connected
        .get(device_id)
        .copied()
        .ok_or(CameraError::NotConnected)?;

    // Discard any stale dir item from a previous capture.
    PENDING_DIR_ITEM.with(|p| {
        if let Some(stale) = p.borrow_mut().take() {
            unsafe { EdsRelease(stale) };
        }
    });

    // PressShutterButton Completely then OFF — more reliable than TakePicture
    // on recent EOS bodies.
    let err = unsafe { EdsSendCommand(camera_ref, CMD_PRESS_SHUTTER, SHUTTER_COMPLETELY) };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }
    let err = unsafe { EdsSendCommand(camera_ref, CMD_PRESS_SHUTTER, SHUTTER_OFF) };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }

    // Pump events until DirItemRequestTransfer arrives or timeout.
    let deadline = std::time::Instant::now() + CAPTURE_TIMEOUT;
    let dir_item = loop {
        unsafe { EdsGetEvent() };

        if let Some(item) = PENDING_DIR_ITEM.with(|p| p.borrow_mut().take()) {
            break item;
        }

        if std::time::Instant::now() >= deadline {
            return Err(CameraError::SdkError(0x000_0001)); // capture timeout
        }

        std::thread::sleep(Duration::from_millis(16));
    };

    download_dir_item(dir_item)
}

fn download_dir_item(dir_item: EdsDirectoryItemRef) -> Result<Vec<u8>, CameraError> {
    // Read file metadata to get the size.
    let mut info = EdsDirectoryItemInfo {
        size: 0,
        is_folder: 0,
        group_id: 0,
        option: 0,
        sz_file_name: [0; EDS_MAX_NAME],
        format: 0,
        date_time: 0,
    };
    let err = unsafe { EdsGetDirectoryItemInfo(dir_item, &mut info) };
    if err != EDS_ERR_OK {
        unsafe { EdsRelease(dir_item) };
        return Err(CameraError::SdkError(err));
    }

    // Create an in-memory stream.
    let mut stream: EdsStreamRef = std::ptr::null_mut();
    let err = unsafe { EdsCreateMemoryStream(0, &mut stream) };
    if err != EDS_ERR_OK {
        unsafe { EdsRelease(dir_item) };
        return Err(CameraError::SdkError(err));
    }

    // Download the file into the stream.
    let err = unsafe { EdsDownload(dir_item, info.size, stream) };
    if err != EDS_ERR_OK {
        unsafe {
            EdsDownloadCancel(dir_item);
            EdsRelease(stream);
            EdsRelease(dir_item);
        }
        return Err(CameraError::SdkError(err));
    }

    // Signal transfer complete to the camera.
    unsafe { EdsDownloadComplete(dir_item) };

    // Read bytes from the stream buffer.
    let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut length: u64 = 0;
    unsafe {
        EdsGetPointer(stream, &mut ptr);
        EdsGetLength(stream, &mut length);
    }

    // SAFETY: ptr is valid until EdsRelease(stream).
    let bytes = unsafe {
        std::slice::from_raw_parts(ptr as *const u8, length as usize).to_vec()
    };

    unsafe {
        EdsRelease(stream);
        EdsRelease(dir_item);
    }

    Ok(bytes)
}

fn type_to_prop_id(param_type: ParameterType) -> Option<u32> {
    match param_type {
        ParameterType::ImageQuality        => Some(PROP_IMAGE_QUALITY),
        ParameterType::Aperture            => Some(PROP_AV),
        ParameterType::ShutterSpeed        => Some(PROP_TV),
        ParameterType::Iso                 => Some(PROP_ISO),
        ParameterType::WhiteBalance        => Some(PROP_WHITE_BALANCE),
        ParameterType::ColorTemperature    => Some(PROP_COLOR_TEMPERATURE),
        ParameterType::ExposureCompensation=> Some(PROP_EXPOSURE_COMP),
        ParameterType::LiveViewZoom                => Some(PROP_EVF_ZOOM),
        _ => None,
    }
}

fn set_parameter_impl(
    device_id: &str,
    prop_id: u32,
    value: i32,
    connected: &HashMap<String, EdsCameraRef>,
) -> Result<(), CameraError> {
    let camera_ref = connected
        .get(device_id)
        .copied()
        .ok_or(CameraError::NotConnected)?;

    let err = unsafe {
        EdsSetPropertyData(
            camera_ref,
            prop_id,
            0,
            std::mem::size_of::<i32>() as u32,
            &value as *const i32 as *const std::ffi::c_void,
        )
    };

    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }

    if prop_id == PROP_EVF_ZOOM {
        let target_zoom = value as u32;

        // Cache the zoom immediately so get_parameters_impl can report the
        // correct value even if the EVF frame is not yet ready (OBJECT_NOTREADY
        // is common right after the live view loop has drained available frames).
        EVF_ZOOM_CACHE.with(|c| c.borrow_mut().insert(device_id.to_string(), target_zoom));

        // Poll the EVF until it confirms the new zoom level (up to 500 ms),
        // then center the position. Checking actual_zoom guards against using
        // a stale coord_sys (zoom=1 range) which would set an out-of-range
        // position and cause some bodies to silently reset the zoom to Fit.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            unsafe { EdsGetEvent() };
            std::thread::sleep(Duration::from_millis(32));

            let mut stream: EdsStreamRef = std::ptr::null_mut();
            if unsafe { EdsCreateMemoryStream(0, &mut stream) } != EDS_ERR_OK {
                break;
            }
            let mut evf_image: EdsEvfImageRef = std::ptr::null_mut();
            let evf_created = unsafe { EdsCreateEvfImageRef(stream, &mut evf_image) } == EDS_ERR_OK;
            let mut confirmed = false;
            if evf_created && unsafe { EdsDownloadEvfImage(camera_ref, evf_image) } == EDS_ERR_OK {
                let mut actual_zoom: u32 = 0;
                unsafe {
                    EdsGetPropertyData(
                        evf_image, PROP_EVF_ZOOM, 0,
                        std::mem::size_of::<u32>() as u32,
                        &mut actual_zoom as *mut u32 as *mut std::ffi::c_void,
                    )
                };
                if actual_zoom == target_zoom {
                    // Zoom confirmed — center position if zoomed in.
                    if target_zoom > 1 {
                        let mut coord_sys = EdsSize { width: 0, height: 0 };
                        let cs_err = unsafe {
                            EdsGetPropertyData(
                                evf_image, PROP_EVF_COORDINATE_SYS, 0,
                                std::mem::size_of::<EdsSize>() as u32,
                                &mut coord_sys as *mut EdsSize as *mut std::ffi::c_void,
                            )
                        };
                        if cs_err == EDS_ERR_OK && coord_sys.width > 0 && coord_sys.height > 0 {
                            let center = EdsPoint {
                                x: coord_sys.width / 2,
                                y: coord_sys.height / 2,
                            };
                            unsafe {
                                EdsSetPropertyData(
                                    camera_ref, PROP_EVF_ZOOM_POSITION, 0,
                                    std::mem::size_of::<EdsPoint>() as u32,
                                    &center as *const EdsPoint as *const std::ffi::c_void,
                                )
                            };
                        }
                    }
                    confirmed = true;
                }
            }
            if evf_created {
                unsafe { EdsRelease(evf_image) };
            }
            unsafe { EdsRelease(stream) };

            if confirmed || std::time::Instant::now() >= deadline {
                break;
            }
        }
    }

    Ok(())
}

fn set_iso_manual_impl(
    device_id: &str,
    connected: &HashMap<String, EdsCameraRef>,
) -> Result<(), CameraError> {
    let camera_ref = connected
        .get(device_id)
        .copied()
        .ok_or(CameraError::NotConnected)?;

    let mut desc = EdsPropertyDesc { form: 0, access: 0, num_elements: 0, prop_desc: [0; 128] };
    let err = unsafe { EdsGetPropertyDesc(camera_ref, PROP_ISO, &mut desc) };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }

    let first_manual = desc.prop_desc[..desc.num_elements as usize]
        .iter()
        .copied()
        .find(|&v| v != 0x00)
        .ok_or(CameraError::NotSupported)?;

    let err = unsafe {
        EdsSetPropertyData(
            camera_ref,
            PROP_ISO,
            0,
            std::mem::size_of::<i32>() as u32,
            &first_manual as *const i32 as *const std::ffi::c_void,
        )
    };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }
    Ok(())
}

fn set_evf_zoom_axis_impl(
    device_id: &str,
    axis_is_x: bool,
    value: i32,
    connected: &HashMap<String, EdsCameraRef>,
) -> Result<(), CameraError> {
    let camera_ref = connected
        .get(device_id)
        .copied()
        .ok_or(CameraError::NotConnected)?;

    // Read the current position so we only update one axis.
    let mut current = EdsPoint { x: 0, y: 0 };
    unsafe {
        EdsGetPropertyData(
            camera_ref,
            PROP_EVF_ZOOM_POSITION,
            0,
            std::mem::size_of::<EdsPoint>() as u32,
            &mut current as *mut EdsPoint as *mut std::ffi::c_void,
        )
    };

    let point = if axis_is_x {
        EdsPoint { x: value, y: current.y }
    } else {
        EdsPoint { x: current.x, y: value }
    };

    let err = unsafe {
        EdsSetPropertyData(
            camera_ref,
            PROP_EVF_ZOOM_POSITION,
            0,
            std::mem::size_of::<EdsPoint>() as u32,
            &point as *const EdsPoint as *const std::ffi::c_void,
        )
    };
    if err != EDS_ERR_OK {
        return Err(CameraError::SdkError(err));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Code tables
// ---------------------------------------------------------------------------

fn decode_av(code: i32) -> String {
    let label = match code {
        0x08 => "f/1",
        0x0B => "f/1.1",
        0x0C => "f/1.2",
        0x0D => "f/1.2 (1/3)",
        0x10 => "f/1.4",
        0x13 => "f/1.6",
        0x14 => "f/1.8",
        0x15 => "f/1.8 (1/3)",
        0x18 => "f/2",
        0x1B => "f/2.2",
        0x1C => "f/2.5",
        0x1D => "f/2.5 (1/3)",
        0x20 => "f/2.8",
        0x23 => "f/3.2",
        0x85 => "f/3.4",
        0x24 => "f/3.5",
        0x25 => "f/3.5 (1/3)",
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
        0x43 => "f/13 (1/3)",
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
        0x5B => "f/36",
        0x5C => "f/38",
        0x5D => "f/40",
        0x60 => "f/45",
        0x63 => "f/51",
        0x64 => "f/54",
        0x65 => "f/57",
        0x68 => "f/64",
        0x6B => "f/72",
        0x6C => "f/76",
        0x6D => "f/80",
        0x70 => "f/91",
        -1   => "Not valid", // 0xffffffff
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_tv(code: i32) -> String {
    let label = match code {
        0x0C => "Bulb",
        0x10 => "30s",
        0x13 => "25s",
        0x14 => "20s",
        0x15 => "20s (1/3)",
        0x18 => "15s",
        0x1B => "13s",
        0x1C => "10s",
        0x1D => "10s (1/3)",
        0x20 => "8s",
        0x23 => "6s",
        0x24 => "6s (1/3)",
        0x25 => "5s",
        0x28 => "4s",
        0x2B => "3.2s",
        0x2C => "3s",
        0x2D => "2.5s",
        0x30 => "2s",
        0x33 => "1.6s",
        0x34 => "1.5s",
        0x35 => "1.3s",
        0x38 => "1s",
        0x3B => "0.8s",
        0x3C => "0.7s",
        0x3D => "0.6s",
        0x40 => "0.5s",
        0x43 => "0.4s",
        0x44 => "0.3s (1/3)",
        0x45 => "0.3s",
        0x48 => "1/4",
        0x4B => "1/5 (1/3)",
        0x4C => "1/5",
        0x4D => "1/6 (1/3)",
        0x50 => "1/8",
        0x53 => "1/10 (1/3)",
        0x54 => "1/10",
        0x55 => "1/13",
        0x58 => "1/15",
        0x5B => "1/20 (1/3)",
        0x5C => "1/20",
        0x5D => "1/25",
        0x60 => "1/30",
        0x63 => "1/40",
        0x64 => "1/45",
        0x65 => "1/50",
        0x68 => "1/60",
        0x6B => "1/80",
        0x6C => "1/90",
        0x6D => "1/100",
        0x70 => "1/125",
        0x73 => "1/160",
        0x74 => "1/180",
        0x75 => "1/200",
        0x78 => "1/250",
        0x7B => "1/320",
        0x7C => "1/350",
        0x7D => "1/400",
        0x80 => "1/500",
        0x83 => "1/640",
        0x84 => "1/750",
        0x85 => "1/800",
        0x88 => "1/1000",
        0x8B => "1/1250",
        0x8C => "1/1500",
        0x8D => "1/1600",
        0x90 => "1/2000",
        0x93 => "1/2500",
        0x94 => "1/3000",
        0x95 => "1/3200",
        0x98 => "1/4000",
        0x9B => "1/5000",
        0x9C => "1/6000",
        0x9D => "1/6400",
        0xA0 => "1/8000",
        0xA5 => "1/12800",
        0xA8 => "1/16000",
        0xAB => "1/20000",
        0xAD => "1/25600",
        0xB0 => "1/32000",
        -1   => "Not valid", // 0xffffffff
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_iso(code: i32) -> String {
    match code {
        0x00 => "Auto".to_string(),
        0x28 => "6".to_string(),
        0x30 => "12".to_string(),
        0x38 => "25".to_string(),
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
        0x83 => "16000".to_string(),
        0x85 => "20000".to_string(),
        0x88 => "25600".to_string(),
        0x8B => "32000".to_string(),
        0x8D => "40000".to_string(),
        0x90 => "51200".to_string(),
        0x93 => "64000".to_string(),
        0x95 => "80000".to_string(),
        0x98 => "102400".to_string(),
        0xA0 => "204800".to_string(),
        0xA8 => "409600".to_string(),
        0xB0 => "819200".to_string(),
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
        15 => "White paper 2",
        16 => "White paper 3",
        18 => "White paper 4",
        19 => "White paper 5",
        20 => "Custom WB 4",
        21 => "Custom WB 5",
        23 => "Auto white priority", // kEdsWhiteBalance_AwbWhite
        -1 => "Click WB",             // kEdsWhiteBalance_Click
        -2 => "Pasted",               // kEdsWhiteBalance_Pasted
        _ => return format!("0x{code:02X}"),
    };
    label.to_string()
}

fn decode_color_temp(code: i32) -> String {
    format!("{code}K")
}


fn decode_image_quality(code: i32) -> String {
    let label = match code as u32 {
        0x0010ff0f => "L JPEG",
        0x0013ff0f => "L JPEG Fine",
        0x0012ff0f => "L JPEG Normal",
        0x0110ff0f => "M JPEG",
        0x0113ff0f => "M JPEG Fine",
        0x0112ff0f => "M JPEG Normal",
        0x0510ff0f => "M1 JPEG",
        0x0513ff0f => "M1 JPEG Fine",
        0x0512ff0f => "M1 JPEG Normal",
        0x0610ff0f => "M2 JPEG",
        0x0613ff0f => "M2 JPEG Fine",
        0x0612ff0f => "M2 JPEG Normal",
        0x0210ff0f => "S JPEG",
        0x0213ff0f => "S JPEG Fine",
        0x0212ff0f => "S JPEG Normal",
        0x0e10ff0f => "S1 JPEG",
        0x0e13ff0f => "S1 JPEG Fine",
        0x0e12ff0f => "S1 JPEG Normal",
        0x0f10ff0f => "S2 JPEG",
        0x0f13ff0f => "S2 JPEG Fine",
        0x1013ff0f => "S3 JPEG Fine",
        0x0064ff0f => "RAW",
        0x0164ff0f => "MRAW",
        0x0264ff0f => "SRAW",
        0x00640013 => "RAW + L Fine",
        0x00640012 => "RAW + L Normal",
        0x00640113 => "RAW + M Fine",
        0x00640112 => "RAW + M Normal",
        0x00640213 => "RAW + S Fine",
        0x00640212 => "RAW + S Normal",
        0x00640010 => "RAW + L JPEG",
        0x00640110 => "RAW + M JPEG",
        0x00640210 => "RAW + S JPEG",
        0x01640013 => "MRAW + L Fine",
        0x01640012 => "MRAW + L Normal",
        0x01640010 => "MRAW + L JPEG",
        0x02640013 => "SRAW + L Fine",
        0x02640012 => "SRAW + L Normal",
        0x02640010 => "SRAW + L JPEG",
        _ => return format!("{code:#010x}"),
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
