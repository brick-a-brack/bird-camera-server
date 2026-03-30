use serde::Serialize;
use std::sync::OnceLock;

// mod directshow;
mod media_foundation;

#[derive(Debug, Clone, Serialize)]
pub struct CameraInfo {
    pub id: String,
    pub name: String,
    pub backend: &'static str,
    pub connected: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct CameraResolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum CameraError {
    InvalidCameraId(String),
    CameraNotFound(String),
    CameraNotConnected(String),
    BackendUnavailable(String),
    BackendFailure(String),
}

impl std::fmt::Display for CameraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCameraId(message) => write!(f, "{message}"),
            Self::CameraNotFound(message) => write!(f, "{message}"),
            Self::CameraNotConnected(message) => write!(f, "{message}"),
            Self::BackendUnavailable(message) => write!(f, "{message}"),
            Self::BackendFailure(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CameraError {}

#[derive(Debug, Clone)]
pub struct CameraDevice {
    pub raw_id: String,
    pub name: String,
}

pub trait CameraBackend: Send + Sync {
    fn backend_id(&self) -> &'static str;
    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError>;
    fn list_resolutions(&self, raw_id: &str) -> Result<Vec<CameraResolution>, CameraError>;
    fn connect(&self, raw_id: &str) -> Result<(), CameraError>;
    fn disconnect(&self, raw_id: &str) -> Result<(), CameraError>;
    fn set_preview_resolution(
        &self,
        raw_id: &str,
        resolution: CameraResolution,
    ) -> Result<(), CameraError>;
    fn get_preview_resolution(&self, raw_id: &str) -> Option<CameraResolution>;
    fn is_connected(&self, raw_id: &str) -> bool;
    fn capture_photo_jpeg(
        &self,
        raw_id: &str,
        preferred_resolution: Option<CameraResolution>,
    ) -> Result<Vec<u8>, CameraError>;
}

pub struct CameraService {
    backends: Vec<Box<dyn CameraBackend>>,
}

impl CameraService {
    pub fn new(backends: Vec<Box<dyn CameraBackend>>) -> Self {
        Self { backends }
    }

    pub fn list_cameras(&self) -> Result<Vec<CameraInfo>, CameraError> {
        let mut cameras = Vec::new();

        for backend in &self.backends {
            let backend_id = backend.backend_id();
            let mut backend_cameras = backend.list_cameras()?;
            cameras.extend(backend_cameras.drain(..).map(|camera| CameraInfo {
                id: format!("{backend_id}:{}", camera.raw_id),
                name: camera.name,
                backend: backend_id,
                connected: backend.is_connected(&camera.raw_id),
            }));
        }

        Ok(cameras)
    }

    pub fn capture_photo_jpeg_with_resolution(
        &self,
        camera_id: &str,
        preferred_resolution: Option<CameraResolution>,
    ) -> Result<Vec<u8>, CameraError> {
        let (backend_id, raw_id) = split_camera_id(camera_id)?;

        let backend = self
            .backends
            .iter()
            .find(|backend| backend.backend_id() == backend_id)
            .ok_or_else(|| {
                CameraError::BackendUnavailable(format!("backend not registered: {backend_id}"))
            })?;

        if !backend.is_connected(raw_id) {
            return Err(CameraError::CameraNotConnected(format!(
                "camera is not connected: {camera_id}"
            )));
        }

        backend.capture_photo_jpeg(raw_id, preferred_resolution)
    }

    pub fn list_resolutions(&self, camera_id: &str) -> Result<Vec<CameraResolution>, CameraError> {
        let (backend_id, raw_id) = split_camera_id(camera_id)?;
        let backend = self
            .backends
            .iter()
            .find(|backend| backend.backend_id() == backend_id)
            .ok_or_else(|| {
                CameraError::BackendUnavailable(format!("backend not registered: {backend_id}"))
            })?;

        backend.list_resolutions(raw_id)
    }

    pub fn set_preview_resolution(
        &self,
        camera_id: &str,
        resolution: CameraResolution,
    ) -> Result<(), CameraError> {
        let (backend_id, raw_id) = split_camera_id(camera_id)?;
        let backend = self
            .backends
            .iter()
            .find(|backend| backend.backend_id() == backend_id)
            .ok_or_else(|| {
                CameraError::BackendUnavailable(format!("backend not registered: {backend_id}"))
            })?;

        backend.set_preview_resolution(raw_id, resolution)
    }

    pub fn get_preview_resolution(
        &self,
        camera_id: &str,
    ) -> Result<Option<CameraResolution>, CameraError> {
        let (backend_id, raw_id) = split_camera_id(camera_id)?;
        let backend = self
            .backends
            .iter()
            .find(|backend| backend.backend_id() == backend_id)
            .ok_or_else(|| {
                CameraError::BackendUnavailable(format!("backend not registered: {backend_id}"))
            })?;

        Ok(backend.get_preview_resolution(raw_id))
    }

    pub fn connect(&self, camera_id: &str) -> Result<(), CameraError> {
        let (backend_id, raw_id) = split_camera_id(camera_id)?;
        let backend = self
            .backends
            .iter()
            .find(|backend| backend.backend_id() == backend_id)
            .ok_or_else(|| {
                CameraError::BackendUnavailable(format!("backend not registered: {backend_id}"))
            })?;

        backend.connect(raw_id)
    }

    pub fn disconnect(&self, camera_id: &str) -> Result<(), CameraError> {
        let (backend_id, raw_id) = split_camera_id(camera_id)?;
        let backend = self
            .backends
            .iter()
            .find(|backend| backend.backend_id() == backend_id)
            .ok_or_else(|| {
                CameraError::BackendUnavailable(format!("backend not registered: {backend_id}"))
            })?;

        backend.disconnect(raw_id)
    }

    pub fn is_connected(&self, camera_id: &str) -> bool {
        if let Ok((backend_id, raw_id)) = split_camera_id(camera_id) {
            if let Some(backend) = self
                .backends
                .iter()
                .find(|backend| backend.backend_id() == backend_id)
            {
                return backend.is_connected(raw_id);
            }
        }
        false
    }
}

fn split_camera_id(camera_id: &str) -> Result<(&str, &str), CameraError> {
    if let Some((backend_id, raw_id)) = camera_id.split_once(':') {
        if raw_id.is_empty() {
            return Err(CameraError::InvalidCameraId(format!(
                "invalid camera id: {camera_id}"
            )));
        }

        return Ok((backend_id, raw_id));
    }

    Ok(("mf", camera_id))
}

fn default_service() -> &'static CameraService {
    static SERVICE: OnceLock<CameraService> = OnceLock::new();

    SERVICE.get_or_init(|| {
        let backends: Vec<Box<dyn CameraBackend>> = vec![
            Box::new(media_foundation::MediaFoundationBackend),
        ];

        CameraService::new(backends)
    })
}

pub fn list_cameras() -> Result<Vec<CameraInfo>, String> {
    default_service()
        .list_cameras()
        .map_err(|error| error.to_string())
}

pub fn capture_photo_jpeg_with_resolution(
    camera_id: &str,
    preferred_resolution: Option<CameraResolution>,
) -> Result<Vec<u8>, String> {
    default_service()
        .capture_photo_jpeg_with_resolution(camera_id, preferred_resolution)
        .map_err(|error| error.to_string())
}

pub fn list_camera_resolutions(camera_id: &str) -> Result<Vec<CameraResolution>, String> {
    default_service()
        .list_resolutions(camera_id)
        .map_err(|error| error.to_string())
}

pub fn set_camera_preview_resolution(
    camera_id: &str,
    resolution: CameraResolution,
) -> Result<(), String> {
    default_service()
        .set_preview_resolution(camera_id, resolution)
        .map_err(|error| error.to_string())
}

pub fn get_camera_preview_resolution(camera_id: &str) -> Result<Option<CameraResolution>, String> {
    default_service()
        .get_preview_resolution(camera_id)
        .map_err(|error| error.to_string())
}

pub fn connect_camera(camera_id: &str) -> Result<(), String> {
    default_service()
        .connect(camera_id)
        .map_err(|error| error.to_string())
}

pub fn disconnect_camera(camera_id: &str) -> Result<(), String> {
    default_service()
        .disconnect(camera_id)
        .map_err(|error| error.to_string())
}

pub fn is_camera_connected(camera_id: &str) -> bool {
    default_service().is_connected(camera_id)
}
