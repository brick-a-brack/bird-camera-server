#[cfg(feature = "backend-canon")]
pub mod canon;

#[cfg(all(feature = "backend-webcam-macos", target_os = "macos"))]
pub mod webcam_macos;

#[cfg(all(feature = "backend-webcam-windows", target_os = "windows"))]
pub mod webcam_windows;
