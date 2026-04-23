# Claude Rules for bird-camera-server

## Language
- All code, comments, commit messages, variable names, and documentation must be in English.

## Project overview
REST API to control cameras (DSLR and webcams) from multiple vendors and operating systems.
The API is consumed locally — it binds exclusively to `127.0.0.1`, no authentication required.

## Code style
- Follow standard Rust conventions (`rustfmt`, `clippy`).
- No `unwrap()` in production paths — use proper error handling (`?`, `Result`, `thiserror`).
- Keep handlers thin: business logic belongs in dedicated modules, not in route handlers.
- Prefer explicit types over inference when it aids readability.

## Architecture

### Device IDs
- All device IDs exposed by the API are **opaque, URL-safe base64url strings**.
- Format: `base64url("<backend_id>:<native_id>")` — e.g. `base64url("canon:USB:0,1,0")`.
- Encoding/decoding is handled by `DeviceId` in `src/camera/mod.rs`.
- Backends work exclusively with **native IDs** (e.g. Canon port names). They never see or produce opaque IDs directly, except in `list_devices` where they call `DeviceId::new(...).encode()` to build the `DeviceInfo.id` field.
- Opaque IDs allow direct backend routing without trying all backends: decode → read `backend` field → look up in `BackendState` HashMap.

### CameraBackend trait
- Every backend must implement the `CameraBackend` trait defined in `src/camera/mod.rs`.
- Current trait methods: `backend_id`, `list_devices`, `connect`, `disconnect`, `is_connected`, `get_parameters`, `get_live_view_frame`.
- Future methods: `capture_photo`, `set_parameter`.
- `backend_id()` returns the backend's unique name (e.g. `"canon"`). It is used to build opaque device IDs and to key the backend registry.
- Route handlers must only interact with the `CameraBackend` trait — never with a concrete backend type.

### Backend registry & app state
- `BackendState` is `Arc<HashMap<String, Arc<dyn CameraBackend>>>`, keyed by `backend_id()`.
- The axum app state is `AppState` (in `src/routes/cameras.rs`), which wraps both `BackendState` and `LiveViewSenders`.
- `FromRef<AppState> for BackendState` is implemented so handlers that only need backends can extract `State<BackendState>` directly.
- Backends are registered at startup in `build_backends()` in `main.rs`.
- If a backend fails to initialize (e.g. SDK DLL not found), it is skipped with an error log — the server starts anyway.
- Each backend is gated behind a Cargo feature flag: `backend-canon`, `backend-nikon`, `backend-webcam-linux`, `backend-webcam-windows`, `backend-webcam-macos`.
- Backend code lives in `src/backends/<name>.rs` (`#[cfg(feature = "backend-<name>")]`).
- Currently in scope: `backend-canon` only. Others will be added later.

### Canon SDK thread
- The EDSDK relies on Windows messages internally and does not work on tokio worker threads.
- All Canon SDK calls run on a single dedicated OS thread (`"canon-sdk"`) that pumps `EdsGetEvent()` every 16 ms.
- Communication between the backend and its SDK thread uses `std::sync::mpsc` channels (actor pattern).
- The SDK thread holds all Canon-internal state (open session refs, etc.) — raw pointers never leave the thread.
- `EdsInitializeSDK` / `EdsTerminateSDK` are called on the SDK thread, not on the main thread.
- EVF (live view) output is enabled once at `connect` time via `EdsSetPropertyData(kEdsPropID_Evf_OutputDevice, kEdsEvfOutputDevice_PC)` — not on every frame.

### Camera parameters
- `get_parameters(native_id)` returns only the **currently settable** parameters for the connected device.
- Uses `EdsGetPropertyDesc` to get allowed values and access level per property. Properties with `access == 0` (read-only) or `num_elements == 0` are excluded.
- The allowed values depend on the camera's current exposure mode (e.g. Tv is read-only in Av mode).
- Response format: `[{ type, current, options: [{ label, value }] }]` where `value` is the raw SDK i32 code.
- Canon property IDs (from `EDSDKTypes.h`):
  - `kEdsPropID_DriveMode`           = `0x00000401`
  - `kEdsPropID_ISOSpeed`            = `0x00000402`
  - `kEdsPropID_MeteringMode`        = `0x00000403`
  - `kEdsPropID_AFMode`              = `0x00000404`
  - `kEdsPropID_Av`                  = `0x00000405`
  - `kEdsPropID_Tv`                  = `0x00000406`
  - `kEdsPropID_ExposureCompensation`= `0x00000407`
  - `kEdsPropID_WhiteBalance`        = `0x00000106`
  - `kEdsPropID_ColorTemperature`    = `0x00000107`
  - `kEdsPropID_Evf_OutputDevice`    = `0x00000500`
- **Do not guess property IDs** — always verify in `external/EDSDK/EDSDKv132010W/Windows/EDSDK/Header/EDSDKTypes.h`.

### Live view & streaming
- Live view is served as MJPEG over HTTP (`multipart/x-mixed-replace; boundary=frame`).
- `LiveViewSenders` = `Arc<Mutex<HashMap<String, broadcast::Sender<Arc<Bytes>>>>>` — one sender per active device (keyed by opaque device ID).
- Only one capture loop runs per device regardless of how many clients are connected. The loop starts when the first client subscribes and stops when the last one disconnects.
- `EDS_ERR_OBJECT_NOTREADY` (0x0000A102) during frame capture is skipped (continue), not fatal.
- The route checks `is_connected` via `spawn_blocking` **before** sending any HTTP headers — returns 409 if not connected.
- Broadcast buffer capacity: 4 frames (drops old frames if clients are slow).
- No frames are ever written to disk — everything is in-memory and streamed directly.

### Photo capture
- `POST /cameras/{id}/capture` returns the raw JPEG bytes directly in the HTTP response body.
- Response headers: `Content-Type: image/jpeg`, `Content-Length: <size>`.
- No base64, no JSON wrapper — raw binary only.
- Only JPEG output is supported for now (no RAW/CR3).

### HTTP layer
- Framework: `axum` (not actix-web).
- The server binds exclusively to `127.0.0.1` — never `0.0.0.0`.
- All routes must be registered explicitly; no catch-all wildcards unless intentional.
- JSON is the response format for all non-binary endpoints.
- State changes (connect, disconnect) use `PUT` — they are idempotent.
- The device ID is always in the URL path, never in the request body.

### Current routes
```
GET  /                               — web UI (embedded HTML, served from binary via include_str!)
GET  /health                         — healthcheck JSON
GET  /cameras                        — list all devices across all active backends (includes connected: bool)
PUT  /cameras/{id}/connect           — open a session with a device
PUT  /cameras/{id}/disconnect        — close a session with a device
GET  /cameras/{id}/parameters        — list settable parameters with current value and allowed options (requires connected)
GET  /cameras/{id}/liveview          — MJPEG stream (requires connected, returns 409 if not)
```

### Web UI
- Single-page HTML embedded in the binary via `include_str!("../static/index.html")`.
- Source file: `static/index.html` — no external assets, everything inline.
- Features: camera list with connected badge, connect/disconnect buttons, collapsible parameters panel, live view toggle.
- Auto-refreshes camera list every 5 seconds.

### AVFoundation backend (macOS webcams)

- Source: `src/backends/avfoundation/bridge.m` (Objective-C) + `src/backends/avfoundation/mod.rs` (Rust actor).
- Build: `build.rs` compiles `bridge.m` with `cc` and links `AVFoundation`, `CoreMedia`, `CoreVideo`, `CoreImage`, `Foundation`, `IOKit` frameworks.

#### Parameter reading
- Parameters are enumerated via **CoreMediaIO (CMIO)**: `wc_get_parameters` walks CMIO feature-control objects owned by the device.
- Ranges are read with `kCMIOFeatureControlPropertyNativeRange` / `kCMIOFeatureControlPropertyNativeValue` — these are read-only on most UVC cameras' CMIO drivers (that is fine, we only read).
- `exposure_time_absolute` values from CMIO are in seconds (Float32); they are scaled ×10000 to match UVC 100µs units before being returned to the Rust layer.
- Auto/manual toggles (exposure, white balance) are read via `kCMIOFeatureControlPropertyAutomaticManual`.

#### Parameter writing — IOKit direct UVC
- **Do NOT write via CMIO** (`kCMIOFeatureControlPropertyNativeValue`). On typical UVC webcams macOS marks this property as non-settable (`CMIOObjectIsPropertySettable` → false). Any write returns `kCMIOHardwareIllegalOperationError` (0x6E6F7065 = `'nope'`).
- **Do NOT call `[AVCaptureDevice lockForConfiguration]` + set `exposureMode`/`whiteBalanceMode`** inside `wc_set_parameter`. This permanently locks the AVFoundation mode and breaks subsequent CMIO reads.
- Writes go through **IOKit direct UVC `SET_CUR` requests** (`IOUSBDevRequest`, `bmRequestType=0x21`, `bRequest=0x01`).
- At `wc_open_session`: parse the uniqueID to extract the USB locationID, find the `IOUSBDevice` in the IOKit registry, parse the config descriptor to locate the VideoControl interface number + Processing Unit ID + Camera Terminal ID, then call `USBDeviceOpen`.
- At `wc_close_session` / dealloc: call `USBDeviceClose` + `Release`.

#### AVFoundation uniqueID format (USB cameras)
- Format: `"0x"` + up to 16 hex chars (leading zeros omitted) encoding `locationID(32) | vendorID(16) | productID(16)`.
- Example: `"0x130000046d082d"` → locationID=`0x00130000`, vendorID=`0x046d` (Logitech), productID=`0x082d`.
- Parse with `NSScanner scanHexLongLong`, then `locationID = (uint32_t)(combined >> 32)`.
- Minimum valid length is 10 chars (`"0x"` + 8 hex). Built-in cameras have a different format and will skip UVC silently.

#### UVC control table
| kind | unit | selector | size |
|---|---|---|---|
| backlight_compensation | PU | 0x01 | 2 |
| brightness | PU | 0x02 | 2 |
| contrast | PU | 0x03 | 2 |
| gain | PU | 0x04 | 2 |
| hue | PU | 0x06 | 2 |
| saturation | PU | 0x07 | 2 |
| sharpness | PU | 0x08 | 2 |
| white_balance_temperature | PU | 0x0A | 2 |
| white_balance_auto | PU | 0x0B | 1 |
| exposure_auto | CT | 0x02 | 1 (0→1=manual, 1→8=aperture priority) |
| exposure_time_absolute | CT | 0x04 | 4 (100µs units) |
| zoom_absolute | CT | 0x0B | 2 |

## Canon SDK
- SDK files live in `external/EDSDK/` (git-ignored).
- Windows 64-bit library: `external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library/EDSDK.lib`
- Windows 64-bit DLL: `external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Dll/EDSDK.dll`
- **Official API documentation (PDF)**: `external/EDSDK/EDSDKv132010W/Document/EDSDK_API_EN.pdf`
- **Header files** (property IDs, enums, structs): `external/EDSDK/EDSDKv132010W/Windows/EDSDK/Header/`
- `build.rs` links the SDK library and copies the DLLs to the build output directory automatically.
- Always verify property IDs, error codes, and struct layouts against the header files or PDF before implementing.

## File structure
```
src/
  main.rs             — server startup, backend registry, route registration
  camera/
    mod.rs            — CameraBackend trait, DeviceId, DeviceInfo, CameraError,
                        CameraParameter, ParameterOption
  backends/
    mod.rs            — feature-gated module declarations
    canon.rs          — FFI bindings + impl CameraBackend for CanonBackend
                        (actor pattern, SDK thread, code→label decode tables)
  routes/
    mod.rs
    cameras.rs        — AppState, BackendState, LiveViewSenders, route handlers
static/
  index.html          — web UI source (embedded in binary at compile time)
build.rs              — SDK linking + DLL copy based on active features and target OS
```

## Dependencies
- Prefer well-maintained crates from the axum / tokio ecosystem.
- Do not add a dependency that can be replaced by a few lines of standard library code.
- Pin minor versions in `Cargo.toml` (e.g. `"1"` not `"*"`).

## Testing
- Unit tests live in the same file as the code under test (`#[cfg(test)]` module).
- Integration tests live under `tests/`.
- Every new route must have at least one integration test covering the happy path.
- Backend-specific tests must be gated behind the same feature flag as the backend.

## Git
- Commit messages use the imperative mood: "Add Canon live view route", not "Added" or "Adding".
- Never commit secrets, credentials, SDK license files, or local `.env` files.
