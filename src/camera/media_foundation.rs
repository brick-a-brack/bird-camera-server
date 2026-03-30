use super::{CameraBackend, CameraDevice, CameraError, CameraResolution};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[cfg(target_os = "windows")]
const DEFAULT_PREVIEW_WIDTH: u32 = 1920;
#[cfg(target_os = "windows")]
const DEFAULT_PREVIEW_HEIGHT: u32 = 1080;

#[cfg(target_os = "windows")]
#[derive(Clone)]
struct NativeMode {
    index: u32,
    width: u32,
    height: u32,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
enum SelectionPolicy {
    Prefer1080p,
    PreferMaximum,
}

pub struct MediaFoundationBackend;

struct SessionHandle {
    stop: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<Vec<u8>>>>,
    preview_resolution: Arc<Mutex<Option<CameraResolution>>>,
    worker: Option<JoinHandle<()>>,
}

fn sessions() -> &'static Mutex<HashMap<String, SessionHandle>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, SessionHandle>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "windows")]
struct MfGuard;

#[cfg(target_os = "windows")]
impl MfGuard {
    fn new() -> Result<Self, CameraError> {
        use windows::Win32::{
            Media::MediaFoundation::{MF_VERSION, MFStartup},
            System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
        };

        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| CameraError::BackendFailure(format!("CoInitializeEx failed: {e}")))?;

            MFStartup(MF_VERSION, 0)
                .map_err(|e| CameraError::BackendFailure(format!("MFStartup failed: {e}")))?;
        }

        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
impl Drop for MfGuard {
    fn drop(&mut self) {
        use windows::Win32::{
            Media::MediaFoundation::MFShutdown,
            System::Com::CoUninitialize,
        };

        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

#[cfg(target_os = "windows")]
fn enumerate_devices(
) -> Result<Vec<(String, windows::Win32::Media::MediaFoundation::IMFActivate)>, CameraError> {
    use std::ptr;
    use windows::{
        core::PWSTR,
        Win32::{
            Media::MediaFoundation::{
                IMFActivate, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
                MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
                MFCreateAttributes, MFEnumDeviceSources,
            },
            System::Com::CoTaskMemFree,
        },
    };

    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)
            .map_err(|e| CameraError::BackendFailure(format!("MFCreateAttributes failed: {e}")))?;
        let attributes = attributes.ok_or_else(|| {
            CameraError::BackendFailure("MFCreateAttributes returned null".to_string())
        })?;

        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| CameraError::BackendFailure(format!("SetGUID failed: {e}")))?;

        let mut activate_array_ptr: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attributes, &mut activate_array_ptr, &mut count).map_err(|e| {
            CameraError::BackendFailure(format!("MFEnumDeviceSources failed: {e}"))
        })?;

        if activate_array_ptr.is_null() || count == 0 {
            return Ok(vec![]);
        }

        let mut devices = Vec::with_capacity(count as usize);
        let activates = std::slice::from_raw_parts_mut(activate_array_ptr, count as usize);

        for activate_slot in activates.iter_mut() {
            if let Some(activate) = activate_slot.take() {
                let mut name_ptr = PWSTR::null();
                let mut _length = 0u32;

                activate
                    .GetAllocatedString(
                        &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
                        &mut name_ptr,
                        &mut _length,
                    )
                    .map_err(|e| {
                        CameraError::BackendFailure(format!("GetAllocatedString failed: {e}"))
                    })?;

                let name = name_ptr
                    .to_string()
                    .unwrap_or_else(|_| "Unknown Camera".to_string());
                CoTaskMemFree(Some(name_ptr.0 as _));

                devices.push((name, activate));
            }
        }

        CoTaskMemFree(Some(activate_array_ptr as _));
        Ok(devices)
    }
}

#[cfg(target_os = "windows")]
fn activate_for_raw_id(
    raw_id: &str,
) -> Result<windows::Win32::Media::MediaFoundation::IMFActivate, CameraError> {
    let camera_index = raw_id
        .parse::<usize>()
        .map_err(|_| CameraError::InvalidCameraId(format!("invalid camera id: mf:{raw_id}")))?;

    let devices = enumerate_devices()?;
    let (_name, activate) = devices
        .into_iter()
        .nth(camera_index)
        .ok_or_else(|| CameraError::CameraNotFound(format!("camera not found: mf:{raw_id}")))?;

    Ok(activate)
}

#[cfg(target_os = "windows")]
fn enumerate_mjpg_native_modes(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> Vec<NativeMode> {
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaType, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        MFVideoFormat_MJPG,
    };

    let mut modes = Vec::new();
    let mut index = 0u32;

    loop {
        let media_type: IMFMediaType = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, index)
        } {
            Ok(value) => value,
            Err(_) => break,
        };

        let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) };
        if subtype.ok() != Some(MFVideoFormat_MJPG) {
            index += 1;
            continue;
        }

        let packed_size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) };
        let Ok(packed_size) = packed_size else {
            index += 1;
            continue;
        };

        let width = (packed_size >> 32) as u32;
        let height = (packed_size & 0xFFFF_FFFF) as u32;

        modes.push(NativeMode {
            index,
            width,
            height,
        });

        index += 1;
    }

    modes
}

#[cfg(target_os = "windows")]
fn modes_to_resolutions(modes: &[NativeMode]) -> Vec<CameraResolution> {
    let mut values: Vec<CameraResolution> = Vec::new();

    for mode in modes {
        let candidate = CameraResolution {
            width: mode.width,
            height: mode.height,
        };

        if !values
            .iter()
            .any(|r| r.width == candidate.width && r.height == candidate.height)
        {
            values.push(candidate);
        }
    }

    values.sort_by(|a, b| {
        let area_a = a.width as u64 * a.height as u64;
        let area_b = b.width as u64 * b.height as u64;
        area_b.cmp(&area_a)
    });

    values
}

#[cfg(target_os = "windows")]
fn select_mode_index(
    modes: &[NativeMode],
    preferred_resolution: Option<CameraResolution>,
    policy: SelectionPolicy,
) -> Option<u32> {
    if let Some(preferred) = preferred_resolution {
        if let Some(mode) = modes
            .iter()
            .find(|m| m.width == preferred.width && m.height == preferred.height)
        {
            return Some(mode.index);
        }
    }

    if let SelectionPolicy::Prefer1080p = policy {
        if let Some(mode) = modes
            .iter()
            .find(|m| m.width == DEFAULT_PREVIEW_WIDTH && m.height == DEFAULT_PREVIEW_HEIGHT)
        {
            return Some(mode.index);
        }
    }

    modes
        .iter()
        .max_by_key(|m| (m.width as u64) * (m.height as u64))
        .map(|m| m.index)
}

#[cfg(target_os = "windows")]
fn create_reader_for_resolution(
    activate: windows::Win32::Media::MediaFoundation::IMFActivate,
    preferred_resolution: Option<CameraResolution>,
    policy: SelectionPolicy,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSourceReader, CameraError> {
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaSource, IMFMediaType, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        MFCreateSourceReaderFromMediaSource,
    };

    unsafe {
        let media_source: IMFMediaSource = activate
            .ActivateObject()
            .map_err(|e| CameraError::BackendFailure(format!("ActivateObject failed: {e}")))?;

        let reader = MFCreateSourceReaderFromMediaSource(&media_source, None).map_err(|e| {
            CameraError::BackendFailure(format!(
                "MFCreateSourceReaderFromMediaSource failed: {e}"
            ))
        })?;

        let modes = enumerate_mjpg_native_modes(&reader);
        let selected_index = select_mode_index(&modes, preferred_resolution, policy)
            .ok_or_else(|| CameraError::BackendFailure("no native MJPG mode found".to_string()))?;

        let selected_type: IMFMediaType = reader
            .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, selected_index)
            .map_err(|e| {
                CameraError::BackendFailure(format!(
                    "GetNativeMediaType(selected mode) failed: {e}"
                ))
            })?;

        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &selected_type,
            )
            .map_err(|e| {
                CameraError::BackendFailure(format!("SetCurrentMediaType(native mode) failed: {e}"))
            })?;

        Ok(reader)
    }
}

#[cfg(target_os = "windows")]
fn capture_mf_photo_once(
    raw_id: &str,
    preferred_resolution: Option<CameraResolution>,
) -> Result<Vec<u8>, CameraError> {
    let _guard = MfGuard::new()?;
    let activate = activate_for_raw_id(raw_id)?;
    let reader = create_reader_for_resolution(
        activate,
        preferred_resolution,
        SelectionPolicy::PreferMaximum,
    )?;

    read_mjpg_frame(&reader)
}

impl CameraBackend for MediaFoundationBackend {
    fn backend_id(&self) -> &'static str {
        "mf"
    }

    #[cfg(target_os = "windows")]
    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError> {
        let _guard = MfGuard::new()?;
        let devices = enumerate_devices()?;

        Ok(devices
            .into_iter()
            .enumerate()
            .map(|(index, (name, _))| CameraDevice {
                raw_id: index.to_string(),
                name,
            })
            .collect())
    }

    #[cfg(not(target_os = "windows"))]
    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError> {
        Ok(vec![])
    }

    #[cfg(target_os = "windows")]
    fn list_resolutions(&self, raw_id: &str) -> Result<Vec<CameraResolution>, CameraError> {
        use windows::Win32::Media::MediaFoundation::{
            IMFMediaSource, MFCreateSourceReaderFromMediaSource,
        };

        let _guard = MfGuard::new()?;
        let activate = activate_for_raw_id(raw_id)?;

        let media_source: IMFMediaSource = unsafe {
            activate
                .ActivateObject()
                .map_err(|e| CameraError::BackendFailure(format!("ActivateObject failed: {e}")))?
        };

        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&media_source, None) }
            .map_err(|e| {
                CameraError::BackendFailure(format!(
                    "MFCreateSourceReaderFromMediaSource failed: {e}"
                ))
            })?;

        let resolutions = modes_to_resolutions(&enumerate_mjpg_native_modes(&reader));
        if resolutions.is_empty() {
            return Err(CameraError::BackendFailure(
                "camera exposes no native MJPG resolution".to_string(),
            ));
        }

        Ok(resolutions)
    }

    #[cfg(not(target_os = "windows"))]
    fn list_resolutions(&self, _raw_id: &str) -> Result<Vec<CameraResolution>, CameraError> {
        Err(CameraError::BackendUnavailable(
            "media foundation backend is only available on Windows".to_string(),
        ))
    }

    #[cfg(target_os = "windows")]
    fn connect(&self, raw_id: &str) -> Result<(), CameraError> {
        let _guard = MfGuard::new()?;
        let _activate = activate_for_raw_id(raw_id)?;

        let mut guard = sessions()
            .lock()
            .map_err(|_| CameraError::BackendFailure("session lock poisoned".to_string()))?;

        if guard.contains_key(raw_id) {
            return Ok(());
        }

        let raw_id_owned = raw_id.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let latest_frame = Arc::new(Mutex::new(None));
        let preview_resolution = Arc::new(Mutex::new(Some(CameraResolution {
            width: DEFAULT_PREVIEW_WIDTH,
            height: DEFAULT_PREVIEW_HEIGHT,
        })));
        let stop_for_worker = Arc::clone(&stop);
        let latest_for_worker = Arc::clone(&latest_frame);
        let preview_for_worker = Arc::clone(&preview_resolution);

        let worker = thread::spawn(move || {
            let _guard = match MfGuard::new() {
                Ok(guard) => guard,
                Err(_) => return,
            };

            let activate = match activate_for_raw_id(&raw_id_owned) {
                Ok(value) => value,
                Err(_) => return,
            };

            let mut current_resolution = None;
            let mut reader = None;

            while !stop_for_worker.load(Ordering::Relaxed) {
                let desired_resolution = preview_for_worker.lock().ok().and_then(|slot| *slot);

                if desired_resolution != current_resolution || reader.is_none() {
                    reader = create_reader_for_resolution(
                        activate.clone(),
                        desired_resolution,
                        SelectionPolicy::Prefer1080p,
                    )
                    .ok();
                    current_resolution = desired_resolution;
                }

                if let Some(active_reader) = reader.as_ref() {
                    if let Ok(frame) = read_mjpg_frame(active_reader) {
                        if let Ok(mut slot) = latest_for_worker.lock() {
                            *slot = Some(frame);
                        }
                    }
                }

                thread::sleep(Duration::from_millis(20));
            }
        });

        guard.insert(
            raw_id.to_string(),
            SessionHandle {
                stop,
                latest_frame,
                preview_resolution,
                worker: Some(worker),
            },
        );

        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn connect(&self, _raw_id: &str) -> Result<(), CameraError> {
        Err(CameraError::BackendUnavailable(
            "media foundation backend is only available on Windows".to_string(),
        ))
    }

    #[cfg(target_os = "windows")]
    fn disconnect(&self, raw_id: &str) -> Result<(), CameraError> {
        let mut guard = sessions()
            .lock()
            .map_err(|_| CameraError::BackendFailure("session lock poisoned".to_string()))?;

        if let Some(mut session) = guard.remove(raw_id) {
            session.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = session.worker.take() {
                let _ = worker.join();
            }
        }

        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn disconnect(&self, _raw_id: &str) -> Result<(), CameraError> {
        Err(CameraError::BackendUnavailable(
            "media foundation backend is only available on Windows".to_string(),
        ))
    }

    fn is_connected(&self, raw_id: &str) -> bool {
        sessions()
            .lock()
            .map(|guard| guard.contains_key(raw_id))
            .unwrap_or(false)
    }

    #[cfg(target_os = "windows")]
    fn set_preview_resolution(
        &self,
        raw_id: &str,
        resolution: CameraResolution,
    ) -> Result<(), CameraError> {
        let available = self.list_resolutions(raw_id)?;
        let exact_exists = available
            .iter()
            .any(|r| r.width == resolution.width && r.height == resolution.height);

        if !exact_exists {
            return Err(CameraError::BackendFailure(format!(
                "unsupported preview resolution: {}x{}",
                resolution.width, resolution.height
            )));
        }

        let guard = sessions()
            .lock()
            .map_err(|_| CameraError::BackendFailure("session lock poisoned".to_string()))?;
        let session = guard.get(raw_id).ok_or_else(|| {
            CameraError::CameraNotConnected(format!("camera is not connected: mf:{raw_id}"))
        })?;

        let mut slot = session
            .preview_resolution
            .lock()
            .map_err(|_| CameraError::BackendFailure("preview lock poisoned".to_string()))?;
        *slot = Some(resolution);

        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn set_preview_resolution(
        &self,
        _raw_id: &str,
        _resolution: CameraResolution,
    ) -> Result<(), CameraError> {
        Err(CameraError::BackendUnavailable(
            "media foundation backend is only available on Windows".to_string(),
        ))
    }

    #[cfg(target_os = "windows")]
    fn get_preview_resolution(&self, raw_id: &str) -> Option<CameraResolution> {
        sessions()
            .lock()
            .ok()
            .and_then(|guard| guard.get(raw_id).map(|session| Arc::clone(&session.preview_resolution)))
            .and_then(|slot| slot.lock().ok().and_then(|value| *value))
    }

    #[cfg(not(target_os = "windows"))]
    fn get_preview_resolution(&self, _raw_id: &str) -> Option<CameraResolution> {
        None
    }

    #[cfg(target_os = "windows")]
    fn capture_photo_jpeg(
        &self,
        raw_id: &str,
        preferred_resolution: Option<CameraResolution>,
    ) -> Result<Vec<u8>, CameraError> {
        let (latest_frame, active_preview_resolution) = sessions()
            .lock()
            .ok()
            .and_then(|guard| {
                guard.get(raw_id).map(|session| {
                    (
                        Arc::clone(&session.latest_frame),
                        Arc::clone(&session.preview_resolution),
                    )
                })
            })
            .map(|(frame_slot, preview_slot)| {
                let preview = preview_slot.lock().ok().and_then(|v| *v);
                (Some(frame_slot), preview)
            })
            .unwrap_or((None, None));

        if let (Some(frame_slot), Some(requested), Some(active_preview)) = (
            latest_frame,
            preferred_resolution,
            active_preview_resolution,
        ) {
            if requested == active_preview {
                for _ in 0..20 {
                    if let Ok(frame_guard) = frame_slot.lock() {
                        if let Some(frame) = frame_guard.as_ref() {
                            return Ok(frame.clone());
                        }
                    }
                    thread::sleep(Duration::from_millis(30));
                }
            }
        }

        capture_mf_photo_once(raw_id, preferred_resolution)
    }

    #[cfg(not(target_os = "windows"))]
    fn capture_photo_jpeg(
        &self,
        _raw_id: &str,
        _preferred_resolution: Option<CameraResolution>,
    ) -> Result<Vec<u8>, CameraError> {
        Err(CameraError::BackendUnavailable(
            "media foundation backend is only available on Windows".to_string(),
        ))
    }
}

#[cfg(target_os = "windows")]
fn read_mjpg_frame(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> Result<Vec<u8>, CameraError> {
    use windows::Win32::Media::MediaFoundation::{IMFSample, MF_SOURCE_READER_FIRST_VIDEO_STREAM};

    unsafe {
        for _ in 0..90 {
            let mut _stream_index = 0u32;
            let mut _flags = 0u32;
            let mut _timestamp = 0i64;
            let mut sample_opt: Option<IMFSample> = None;

            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut _stream_index),
                    Some(&mut _flags),
                    Some(&mut _timestamp),
                    Some(&mut sample_opt),
                )
                .map_err(|e| CameraError::BackendFailure(format!("ReadSample failed: {e}")))?;

            if let Some(sample) = sample_opt {
                let buffer = sample.ConvertToContiguousBuffer().map_err(|e| {
                    CameraError::BackendFailure(format!("ConvertToContiguousBuffer failed: {e}"))
                })?;

                let mut data_ptr = std::ptr::null_mut();
                let mut _max_len = 0u32;
                let mut current_len = 0u32;

                buffer
                    .Lock(&mut data_ptr, Some(&mut _max_len), Some(&mut current_len))
                    .map_err(|e| CameraError::BackendFailure(format!("buffer lock failed: {e}")))?;

                let jpeg = std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec();
                let _ = buffer.Unlock();

                if jpeg.len() > 4 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 {
                    return Ok(jpeg);
                }
            }
        }

        Err(CameraError::BackendFailure(
            "captured frame is not JPEG (camera may not support MJPG)".to_string(),
        ))
    }
}

#[cfg(not(target_os = "windows"))]
fn read_mjpg_frame(
    _reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> Result<Vec<u8>, CameraError> {
    Err(CameraError::BackendUnavailable(
        "media foundation backend is only available on Windows".to_string(),
    ))
}
