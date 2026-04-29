use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
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
// Parameter type — exhaustive list of all known parameter identifiers.
// Adding a new parameter requires updating this enum and any backend that
// exposes it. This prevents spelling mismatches between backends.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterType {
    // --- Canon: image quality & capture ---
    ImageQuality,
    Aperture,
    ShutterSpeed,
    Iso,
    ExposureCompensation,
    MeteringMode,
    AfMode,
    DriveMode,

    // --- Shared: white balance ---
    WhiteBalance,
    WhiteBalanceMode,  // auto / manual toggle
    ColorTemperature,

    // --- Shared: exposure ---
    Exposure,
    ExposureMode,      // auto / manual toggle

    // --- Shared: focus & zoom ---
    Focus,
    FocusMode,         // auto / manual toggle
    Zoom,

    // --- Webcam: format ---
    VideoFormat,

    // --- Webcam: image adjustments ---
    Brightness,
    BrightnessMode,
    Contrast,
    ContrastMode,
    Hue,
    HueMode,
    Saturation,
    SaturationMode,
    Sharpness,
    Gamma,
    BacklightCompensation,
    Gain,
    GainMode,
    PowerLineFrequency,

    // --- Webcam: camera geometry ---
    Pan,
    PanMode,
    Tilt,
    TiltMode,
    Roll,
    RollMode,
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

/// One option in a Select or RangeSelect parameter.
/// `value` is the opaque string key passed back to `set_parameter`.
#[derive(Debug, Clone, Serialize)]
pub struct ParameterOption {
    /// Human-readable label (e.g. "f/5.6", "1/500", "ISO 400").
    pub label: String,
    /// Opaque string key for identifying the option.
    pub value: String,
}

/// A camera parameter, discriminated by its representation kind.
///
/// - `range`        — continuous numeric value (slider); `current`, `min`, `max`, `step` are integers.
/// - `select`       — arbitrary discrete choices; `current` matches one `option.value`.
/// - `range_select` — ordered discrete values with numeric progression (ISO, aperture);
///                    rendered as a select but values are semantically ordered.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CameraParameter {
    Range {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: i32,
        min: i32,
        max: i32,
        step: i32,
    },
    Select {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: String,
        options: Vec<ParameterOption>,
    },
    RangeSelect {
        #[serde(rename = "type")]
        param_type: ParameterType,
        current: String,
        options: Vec<ParameterOption>,
    },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

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

    /// Sets a parameter by its type and value.
    ///
    /// `value` is always a string:
    /// - Range params:              stringified integer (e.g. `"42"`)
    /// - Select / RangeSelect:      opaque key from `ParameterOption.value` (e.g. `"77"`)
    /// - Mode (auto/manual) params: `"1"` = auto, `"0"` = manual
    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError>;

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let _ = native_id;
        Err(CameraError::NotSupported)
    }
}
