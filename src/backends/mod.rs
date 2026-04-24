#[cfg(feature = "backend-canon")]
pub mod canon;

#[cfg(all(feature = "backend-avfoundation", target_os = "macos"))]
pub mod avfoundation;

#[cfg(all(feature = "backend-webcam-windows", target_os = "windows"))]
pub mod webcam_windows;
