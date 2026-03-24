use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct CameraInfo {
    pub id: String,
    pub name: String,
}

#[cfg(target_os = "windows")]
struct MfGuard;

#[cfg(target_os = "windows")]
impl MfGuard {
    fn new() -> Result<Self, String> {
        use windows::{
            Win32::{
                Media::MediaFoundation::{MF_VERSION, MFStartup},
                System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
            },
        };

        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("CoInitializeEx failed: {e}"))?;

            MFStartup(MF_VERSION, 0).map_err(|e| format!("MFStartup failed: {e}"))?;
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
fn enumerate_devices() -> Result<Vec<(String, windows::Win32::Media::MediaFoundation::IMFActivate)>, String> {
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
            .map_err(|e| format!("MFCreateAttributes failed: {e}"))?;
        let attributes = attributes.ok_or_else(|| "MFCreateAttributes returned null".to_string())?;

        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| format!("SetGUID failed: {e}"))?;

        let mut activate_array_ptr: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attributes, &mut activate_array_ptr, &mut count)
            .map_err(|e| format!("MFEnumDeviceSources failed: {e}"))?;

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
                    .map_err(|e| format!("GetAllocatedString failed: {e}"))?;

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
pub fn list_cameras() -> Result<Vec<CameraInfo>, String> {
    let _guard = MfGuard::new()?;
    let devices = enumerate_devices()?;

    Ok(devices
        .into_iter()
        .enumerate()
        .map(|(index, (name, _))| CameraInfo {
            id: index.to_string(),
            name,
        })
        .collect())
}

#[cfg(target_os = "windows")]
pub fn capture_photo_jpeg(camera_id: &str) -> Result<Vec<u8>, String> {
    use windows::{
        Win32::Media::MediaFoundation::{
            IMFMediaSource, IMFSample, MF_MT_MAJOR_TYPE,
            MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MFCreateMediaType,
            MFCreateSourceReaderFromMediaSource, MFMediaType_Video, MFVideoFormat_MJPG,
        },
    };

    let _guard = MfGuard::new()?;
    let devices = enumerate_devices()?;
    let camera_index = camera_id
        .parse::<usize>()
        .map_err(|_| format!("invalid camera id: {camera_id}"))?;

    let (_name, activate) = devices
        .into_iter()
        .nth(camera_index)
        .ok_or_else(|| format!("camera not found: {camera_id}"))?;

    unsafe {
        let media_source: IMFMediaSource = activate
            .ActivateObject()
            .map_err(|e| format!("ActivateObject failed: {e}"))?;

        let reader = MFCreateSourceReaderFromMediaSource(&media_source, None)
            .map_err(|e| format!("MFCreateSourceReaderFromMediaSource failed: {e}"))?;

        let media_type = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType failed: {e}"))?;

        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("SetGUID major type failed: {e}"))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_MJPG)
            .map_err(|e| format!("SetGUID subtype MJPG failed: {e}"))?;

        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &media_type)
            .map_err(|e| format!("SetCurrentMediaType(MJPG) failed: {e}"))?;

        for _ in 0..60 {
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
                .map_err(|e| format!("ReadSample failed: {e}"))?;

            if let Some(sample) = sample_opt {
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|e| format!("ConvertToContiguousBuffer failed: {e}"))?;

                let mut data_ptr = std::ptr::null_mut();
                let mut _max_len = 0u32;
                let mut current_len = 0u32;

                buffer
                    .Lock(&mut data_ptr, Some(&mut _max_len), Some(&mut current_len))
                    .map_err(|e| format!("buffer lock failed: {e}"))?;

                let jpeg = std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec();

                let _ = buffer.Unlock();

                if jpeg.len() > 4 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 {
                    return Ok(jpeg);
                }

                return Err("captured frame is not JPEG (camera may not support MJPG)".to_string());
            }
        }

        return Err("no video frame available from camera".to_string());
    }
}

#[cfg(not(target_os = "windows"))]
pub fn list_cameras() -> Result<Vec<CameraInfo>, String> {
    Err("native camera bindings are currently only implemented on Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn capture_photo_jpeg(_camera_id: &str) -> Result<Vec<u8>, String> {
    Err("native camera bindings are currently only implemented on Windows".to_string())
}
