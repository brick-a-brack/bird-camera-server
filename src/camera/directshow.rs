use super::{CameraBackend, CameraDevice, CameraError};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct DirectShowBackend;

struct SessionHandle {
    stop: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<Vec<u8>>>>,
    worker: Option<JoinHandle<()>>,
}

fn sessions() -> &'static Mutex<HashMap<String, SessionHandle>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, SessionHandle>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "windows")]
fn with_com<R>(f: impl FnOnce() -> Result<R, CameraError>) -> Result<R, CameraError> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    struct ComGuard;

    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| CameraError::BackendFailure(format!("CoInitializeEx failed: {e}")))?;
    }

    let _guard = ComGuard;
    f()
}

#[cfg(target_os = "windows")]
fn enumerate_monikers(
) -> Result<Vec<(CameraDevice, windows::Win32::System::Com::IMoniker)>, CameraError> {
    use windows::{
        core::{w, GUID},
        Win32::{
            Foundation::S_FALSE,
            Media::DirectShow::ICreateDevEnum,
            System::{
                Com::{
                    StructuredStorage::IPropertyBag, CoCreateInstance, CoTaskMemFree,
                    CLSCTX_INPROC_SERVER,
                },
                Variant::{VariantClear, VariantToStringAlloc, VARIANT},
            },
        },
    };

    const CLSID_SYSTEM_DEVICE_ENUM: GUID =
        GUID::from_u128(0x62be5d10_60eb_11d0_bd3b_00a0c911ce86);
    const CLSID_VIDEO_INPUT_DEVICE_CATEGORY: GUID =
        GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);

    unsafe {
        let dev_enum: ICreateDevEnum = CoCreateInstance(&CLSID_SYSTEM_DEVICE_ENUM, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| CameraError::BackendFailure(format!("CoCreateInstance(SystemDeviceEnum) failed: {e}")))?;

        let mut enum_moniker = None;
        dev_enum
            .CreateClassEnumerator(&CLSID_VIDEO_INPUT_DEVICE_CATEGORY, &mut enum_moniker, 0)
            .map_err(|e| CameraError::BackendFailure(format!("CreateClassEnumerator failed: {e}")))?;

        let Some(enum_moniker) = enum_moniker else {
            return Ok(vec![]);
        };

        let mut devices = Vec::new();
        let mut index = 0usize;

        loop {
            let mut fetched = 0u32;
            let mut monikers = [None];
            let hr = enum_moniker.Next(&mut monikers, Some(&mut fetched as *mut u32));

            if let Err(e) = hr.ok() {
                return Err(CameraError::BackendFailure(format!("IEnumMoniker::Next failed: {e}")));
            }

            if fetched == 0 || hr == S_FALSE {
                break;
            }

            let moniker = match monikers[0].take() {
                Some(value) => value,
                None => break,
            };

            let display_name = moniker.GetDisplayName(None, None).map_err(|e| {
                CameraError::BackendFailure(format!("IMoniker::GetDisplayName failed: {e}"))
            })?;

            let raw_display = display_name
                .to_string()
                .unwrap_or_else(|_| format!("dshow-device-{index}"));
            CoTaskMemFree(Some(display_name.0 as _));

            let mut friendly_name = String::new();
            if let Ok(property_bag) = moniker.BindToStorage::<_, _, IPropertyBag>(None, None) {
                let mut value = VARIANT::default();
                if property_bag.Read(w!("FriendlyName"), &mut value, None).is_ok() {
                    if let Ok(name_ptr) = VariantToStringAlloc(&value) {
                        friendly_name = name_ptr.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(name_ptr.0 as _));
                    }
                }
                let _ = VariantClear(&mut value);
            }

            let cleaned_name = raw_display
                .split('!')
                .next_back()
                .unwrap_or(&raw_display)
                .trim_matches('\0')
                .to_string();

            let final_name = if friendly_name.trim().is_empty() {
                cleaned_name
            } else {
                friendly_name
            };

            devices.push((
                CameraDevice {
                    raw_id: index.to_string(),
                    name: if final_name.is_empty() {
                        format!("DirectShow Camera {index}")
                    } else {
                        final_name
                    },
                },
                moniker,
            ));

            index += 1;
        }

        Ok(devices)
    }
}

#[cfg(not(target_os = "windows"))]
fn enumerate_monikers(
) -> Result<Vec<(CameraDevice, windows::Win32::System::Com::IMoniker)>, CameraError> {
    Ok(vec![])
}

#[cfg(target_os = "windows")]
fn dib_to_jpeg(dib: &[u8]) -> Result<Vec<u8>, CameraError> {
    use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageBuffer, Rgb};

    if dib.len() < 40 {
        return Err(CameraError::BackendFailure(
            "directshow frame too small".to_string(),
        ));
    }

    let header_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let width = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let height = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);

    if header_size < 40 || dib.len() <= header_size {
        return Err(CameraError::BackendFailure(
            "invalid DIB header in directshow frame".to_string(),
        ));
    }

    if compression != 0 {
        return Err(CameraError::BackendFailure(format!(
            "unsupported DIB compression type: {compression}"
        )));
    }

    if bit_count != 24 && bit_count != 32 {
        return Err(CameraError::BackendFailure(format!(
            "unsupported DIB pixel format: {bit_count} bits"
        )));
    }

    let width_u32 = width.unsigned_abs();
    let height_u32 = height.unsigned_abs();
    let width_usize = width_u32 as usize;
    let height_usize = height_u32 as usize;
    let bytes_per_pixel = (bit_count / 8) as usize;
    let row_stride = (width_usize * bytes_per_pixel).div_ceil(4) * 4;
    let pixels_offset = header_size;

    if dib.len() < pixels_offset + row_stride * height_usize {
        return Err(CameraError::BackendFailure(
            "truncated DIB buffer returned by directshow".to_string(),
        ));
    }

    let is_bottom_up = height > 0;
    let mut rgb = Vec::with_capacity(width_usize * height_usize * 3);

    for y in 0..height_usize {
        let src_row = if is_bottom_up { height_usize - 1 - y } else { y };
        let row_start = pixels_offset + src_row * row_stride;

        for x in 0..width_usize {
            let i = row_start + x * bytes_per_pixel;
            let b = dib[i];
            let g = dib[i + 1];
            let r = dib[i + 2];
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }
    }

    let image = ImageBuffer::<Rgb<u8>, _>::from_raw(width_u32, height_u32, rgb).ok_or_else(|| {
        CameraError::BackendFailure("failed to decode directshow frame buffer".to_string())
    })?;

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 90);
    encoder
        .encode_image(&DynamicImage::ImageRgb8(image))
        .map_err(|e| CameraError::BackendFailure(format!("jpeg encoding failed: {e}")))?;

    Ok(jpeg)
}

#[cfg(not(target_os = "windows"))]
fn list_dshow_cameras() -> Result<Vec<CameraDevice>, CameraError> {
    Ok(vec![])
}

#[cfg(target_os = "windows")]
fn list_dshow_cameras() -> Result<Vec<CameraDevice>, CameraError> {
    with_com(|| {
        let entries = enumerate_monikers()?;
        Ok(entries.into_iter().map(|(device, _)| device).collect())
    })
}

#[cfg(target_os = "windows")]
fn capture_dshow_photo(raw_id: &str) -> Result<Vec<u8>, CameraError> {
    use windows::{
        core::{w, GUID, Interface},
        Win32::{
            Foundation::S_FALSE,
            Media::DirectShow::{IBasicVideo, IFilterGraph2, IMediaControl, PINDIR_OUTPUT},
            System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        },
    };

    const CLSID_FILTER_GRAPH: GUID = GUID::from_u128(0xe436ebb3_524f_11ce_9f53_0020af0ba770);

    let camera_index = raw_id
        .parse::<usize>()
        .map_err(|_| CameraError::InvalidCameraId(format!("invalid camera id: dshow:{raw_id}")))?;

    with_com(|| unsafe {
        let entries = enumerate_monikers()?;
        let (_device, moniker) = entries
            .into_iter()
            .nth(camera_index)
            .ok_or_else(|| CameraError::CameraNotFound(format!("camera not found: dshow:{raw_id}")))?;

        let graph: IFilterGraph2 = CoCreateInstance(&CLSID_FILTER_GRAPH, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| CameraError::BackendFailure(format!("CoCreateInstance(FilterGraph) failed: {e}")))?;

        let source_filter = graph
            .AddSourceFilterForMoniker(&moniker, None, w!("DirectShowSource"))
            .map_err(|e| CameraError::BackendFailure(format!("AddSourceFilterForMoniker failed: {e}")))?;

        let enum_pins = source_filter
            .EnumPins()
            .map_err(|e| CameraError::BackendFailure(format!("EnumPins failed: {e}")))?;

        let mut output_pin = None;
        loop {
            let mut fetched = 0u32;
            let mut pins = [None];
            let hr = enum_pins.Next(&mut pins, Some(&mut fetched as *mut u32));
            if hr == S_FALSE || fetched == 0 {
                break;
            }
            hr.ok().map_err(|e| CameraError::BackendFailure(format!("IEnumPins::Next failed: {e}")))?;

            if let Some(pin) = pins[0].take() {
                let direction = pin
                    .QueryDirection()
                    .map_err(|e| CameraError::BackendFailure(format!("QueryDirection failed: {e}")))?;
                if direction == PINDIR_OUTPUT {
                    output_pin = Some(pin);
                    break;
                }
            }
        }

        let output_pin = output_pin.ok_or_else(|| {
            CameraError::BackendFailure("no output pin found for directshow device".to_string())
        })?;

        graph
            .Render(&output_pin)
            .map_err(|e| CameraError::BackendFailure(format!("graph render failed: {e}")))?;

        hide_active_movie_window();

        let media_control: IMediaControl = graph
            .cast()
            .map_err(|e| CameraError::BackendFailure(format!("IMediaControl cast failed: {e}")))?;

        media_control
            .Run()
            .map_err(|e| CameraError::BackendFailure(format!("graph run failed: {e}")))?;

        hide_active_movie_window();

        std::thread::sleep(std::time::Duration::from_millis(350));

        let basic_video: IBasicVideo = graph
            .cast()
            .map_err(|e| CameraError::BackendFailure(format!("IBasicVideo cast failed: {e}")))?;

        let mut dib_size = 0i32;
        basic_video
            .GetCurrentImage(&mut dib_size, std::ptr::null_mut())
            .map_err(|e| CameraError::BackendFailure(format!("GetCurrentImage(size) failed: {e}")))?;

        if dib_size <= 0 {
            let _ = media_control.Stop();
            return Err(CameraError::BackendFailure(
                "directshow returned empty frame".to_string(),
            ));
        }

        let words = (dib_size as usize).div_ceil(4);
        let mut dib_words = vec![0i32; words];
        basic_video
            .GetCurrentImage(&mut dib_size, dib_words.as_mut_ptr())
            .map_err(|e| CameraError::BackendFailure(format!("GetCurrentImage(data) failed: {e}")))?;

        let _ = media_control.Stop();

        let dib = std::slice::from_raw_parts(dib_words.as_ptr() as *const u8, dib_size as usize);
        dib_to_jpeg(dib)
    })
}

impl CameraBackend for DirectShowBackend {
    fn backend_id(&self) -> &'static str {
        "dshow"
    }

    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError> {
        list_dshow_cameras()
    }

    fn connect(&self, raw_id: &str) -> Result<(), CameraError> {
        let devices = self.list_cameras()?;
        let exists = devices.iter().any(|d| d.raw_id == raw_id);
        if !exists {
            return Err(CameraError::CameraNotFound(format!(
                "camera not found: dshow:{raw_id}"
            )));
        }

        let mut guard = sessions()
            .lock()
            .map_err(|_| CameraError::BackendFailure("session lock poisoned".to_string()))?;

        if guard.contains_key(raw_id) {
            return Ok(());
        }

        let raw_id_owned = raw_id.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let latest_frame = Arc::new(Mutex::new(None));
        let stop_for_worker = Arc::clone(&stop);
        let latest_for_worker = Arc::clone(&latest_frame);

        let worker = thread::spawn(move || {
            capture_dshow_stream(&raw_id_owned, &stop_for_worker, &latest_for_worker);
        });

        guard.insert(
            raw_id.to_string(),
            SessionHandle {
                stop,
                latest_frame,
                worker: Some(worker),
            },
        );
        Ok(())
    }

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

    fn is_connected(&self, raw_id: &str) -> bool {
        sessions()
            .lock()
            .map(|guard| guard.contains_key(raw_id))
            .unwrap_or(false)
    }

    #[cfg(target_os = "windows")]
    fn capture_photo_jpeg(&self, raw_id: &str) -> Result<Vec<u8>, CameraError> {
        let latest_frame = sessions()
            .lock()
            .ok()
            .and_then(|guard| guard.get(raw_id).map(|session| Arc::clone(&session.latest_frame)));

        if let Some(frame_slot) = latest_frame {
            for _ in 0..20 {
                if let Ok(frame_guard) = frame_slot.lock() {
                    if let Some(frame) = frame_guard.as_ref() {
                        return Ok(frame.clone());
                    }
                }

                thread::sleep(Duration::from_millis(50));
            }
        }

        capture_dshow_photo(raw_id)
    }

    #[cfg(not(target_os = "windows"))]
    fn capture_photo_jpeg(&self, _raw_id: &str) -> Result<Vec<u8>, CameraError> {
        Err(CameraError::BackendUnavailable(
            "directshow backend is only available on Windows".to_string(),
        ))
    }
}

#[cfg(target_os = "windows")]
fn capture_dshow_stream(
    raw_id: &str,
    stop: &Arc<AtomicBool>,
    latest_frame: &Arc<Mutex<Option<Vec<u8>>>>,
) {
    use windows::{
        core::{w, GUID, Interface},
        Win32::{
            Foundation::S_FALSE,
            Media::DirectShow::{IBasicVideo, IFilterGraph2, IMediaControl, PINDIR_OUTPUT},
            System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
        },
    };

    unsafe {
        if let Err(_) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            return;
        }

        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe { CoUninitialize(); }
            }
        }

        let _guard = ComGuard;
        const CLSID_FILTER_GRAPH: GUID = GUID::from_u128(0xe436ebb3_524f_11ce_9f53_0020af0ba770);

        let camera_index = match raw_id.parse::<usize>() {
            Ok(idx) => idx,
            Err(_) => return,
        };

        let entries = match enumerate_monikers() {
            Ok(e) => e,
            Err(_) => return,
        };

        let (_device, moniker) = match entries.into_iter().nth(camera_index) {
            Some(e) => e,
            None => return,
        };

        let graph: IFilterGraph2 = match CoCreateInstance(&CLSID_FILTER_GRAPH, None, CLSCTX_INPROC_SERVER) {
            Ok(g) => g,
            Err(_) => return,
        };

        let source_filter = match graph.AddSourceFilterForMoniker(&moniker, None, w!("DirectShowSource")) {
            Ok(f) => f,
            Err(_) => return,
        };

        let enum_pins = match source_filter.EnumPins() {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut output_pin = None;
        loop {
            let mut fetched = 0u32;
            let mut pins = [None];
            let hr = enum_pins.Next(&mut pins, Some(&mut fetched as *mut u32));
            if hr == S_FALSE || fetched == 0 {
                break;
            }
            if hr.is_err() {
                return;
            }

            if let Some(pin) = pins[0].take() {
                let direction = match pin.QueryDirection() {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if direction == PINDIR_OUTPUT {
                    output_pin = Some(pin);
                    break;
                }
            }
        }

        let output_pin = match output_pin {
            Some(p) => p,
            None => return,
        };

        if graph.Render(&output_pin).is_err() {
            return;
        }

        hide_active_movie_window();

        let media_control: IMediaControl = match graph.cast() {
            Ok(mc) => mc,
            Err(_) => return,
        };

        if media_control.Run().is_err() {
            return;
        }

        hide_active_movie_window();

        while !stop.load(Ordering::Relaxed) {
            hide_active_movie_window();

            let basic_video: IBasicVideo = match graph.cast() {
                Ok(bv) => bv,
                Err(_) => break,
            };

            let mut dib_size = 0i32;
            if basic_video.GetCurrentImage(&mut dib_size, std::ptr::null_mut()).is_ok() && dib_size > 0 {
                let words = (dib_size as usize).div_ceil(4);
                let mut dib_words = vec![0i32; words];
                if basic_video.GetCurrentImage(&mut dib_size, dib_words.as_mut_ptr()).is_ok() {
                    let dib = std::slice::from_raw_parts(dib_words.as_ptr() as *const u8, dib_size as usize);
                    if let Ok(jpeg) = dib_to_jpeg(dib) {
                        if let Ok(mut slot) = latest_frame.lock() {
                            *slot = Some(jpeg);
                        }
                    }
                }
            }

            thread::sleep(Duration::from_millis(150));
        }

        let _ = media_control.Stop();
    }
}

#[cfg(target_os = "windows")]
fn hide_active_movie_window() {
    use windows::{
        core::w,
        Win32::UI::WindowsAndMessaging::{FindWindowW, ShowWindow, SW_HIDE},
    };

    unsafe {
        if let Ok(hwnd) = FindWindowW(w!("ActiveMovie Window"), None) {
            if !hwnd.0.is_null() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}
