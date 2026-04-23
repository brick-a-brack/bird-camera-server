use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Serialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Opaque device ID
// ---------------------------------------------------------------------------

/// Opaque, URL-safe device identifier exposed by the API.
///
/// Encodes the backend name and the backend-native device ID as
/// `base64url(backend:native_id)`, e.g. `base64url("canon:USB:0,1,0")`.
/// This avoids URL-encoding issues and hides internal identifiers from clients.
pub struct DeviceId {
    pub backend: String,
    pub native_id: String,
}

impl DeviceId {
    pub fn new(backend: impl Into<String>, native_id: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            native_id: native_id.into(),
        }
    }

    /// Encodes to the opaque string sent to clients.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}:{}", self.backend, self.native_id))
    }

    /// Decodes an opaque string received from a client.
    pub fn decode(encoded: &str) -> Result<Self, CameraError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| CameraError::InvalidDeviceId)?;
        let s = String::from_utf8(bytes).map_err(|_| CameraError::InvalidDeviceId)?;
        let (backend, native_id) = s.split_once(':').ok_or(CameraError::InvalidDeviceId)?;
        Ok(Self {
            backend: backend.to_string(),
            native_id: native_id.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Device information returned by `list_devices`.
/// The `id` field is the opaque encoded ID suitable for use in subsequent API calls.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    /// Opaque, URL-safe device identifier (base64url encoded).
    pub id: String,
    /// Human-readable device name (e.g. "Canon EOS R5").
    pub name: String,
    /// Whether a session is currently open with this device.
    pub connected: bool,
}

/// One allowed value for a camera parameter.
#[derive(Debug, Clone, Serialize)]
pub struct ParameterOption {
    /// Human-readable label (e.g. "f/5.6", "1/500", "ISO 400").
    pub label: String,
    /// Raw SDK code to pass back when setting the parameter.
    pub value: i32,
}

/// A single settable camera parameter with its current value and allowed options.
///
/// Discrete params (e.g. focus mode) set `options` and leave `min`/`max`/`step` as `None`.
/// Range params (e.g. brightness, zoom) set `min`/`max`/`step` and leave `options` empty.
#[derive(Debug, Clone, Serialize)]
pub struct CameraParameter {
    /// Parameter identifier (e.g. "brightness", "zoom_absolute").
    #[serde(rename = "type")]
    pub kind: String,
    /// Current value: label for discrete params, stringified integer for range params.
    pub current: String,
    /// Allowed values for discrete params; empty for range params.
    pub options: Vec<ParameterOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<i32>,
}

#[derive(Debug, Error)]
pub enum CameraError {
    #[error("SDK error: {0:#010x}")]
    SdkError(u32),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("invalid device id")]
    InvalidDeviceId,
    #[error("no session open for this device")]
    NotConnected,
    #[error("operation not supported by this backend")]
    NotSupported,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Common interface every camera backend must implement.
///
/// Backends work exclusively with native IDs (e.g. Canon port names).
/// Opaque ID encoding/decoding is handled by the route layer.
pub trait CameraBackend: Send + Sync {
    /// Unique name of this backend (e.g. `"canon"`). Used to build opaque device IDs.
    fn backend_id(&self) -> &str;

    /// Returns all devices currently visible to this backend.
    /// The `DeviceInfo.id` field contains the already-encoded opaque ID.
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError>;

    /// Opens a session with the device identified by `native_id`.
    /// Connecting an already-connected device is a no-op.
    fn connect(&self, native_id: &str) -> Result<(), CameraError>;

    /// Closes the session with the device identified by `native_id`.
    fn disconnect(&self, native_id: &str) -> Result<(), CameraError>;

    /// Returns true if a session is currently open for `native_id`.
    fn is_connected(&self, native_id: &str) -> bool;

    /// Returns all currently settable parameters with their allowed values.
    /// The device must be connected before calling this.
    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError>;

    /// Captures a single live view frame and returns it as raw JPEG bytes.
    /// The device must be connected before calling this.
    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError>;

    /// Sets a camera parameter by its type name and raw SDK value.
    /// The device must be connected before calling this.
    fn set_parameter(&self, native_id: &str, kind: &str, value: i32) -> Result<(), CameraError>;
}
