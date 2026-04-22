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
- The trait covers: `backend_id`, `list_devices`, `connect`, `disconnect`. Future methods: `capture_photo`, `live_view`, `get_capabilities`, `get_settings`, `set_settings`.
- `backend_id()` returns the backend's unique name (e.g. `"canon"`). It is used to build opaque device IDs and to key the backend registry.
- Route handlers must only interact with the `CameraBackend` trait — never with a concrete backend type.

### Backend registry
- `BackendState` is `Arc<HashMap<String, Arc<dyn CameraBackend>>>`, keyed by `backend_id()`.
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

### Camera capabilities
- Parameters (aperture, ISO, exposure, focus, white balance, etc.) are discovered dynamically at connection time via `get_capabilities()` (to be implemented).
- Capabilities depend on both the backend and the connected device — no hardcoded schema.
- The API response for capabilities must include: current value + list of available values for each supported parameter.

### Live view & streaming
- Live view is served as MJPEG over HTTP (`multipart/x-mixed-replace; boundary=frame`).
- Target framerate: 30–60 fps.
- Each camera has one capture loop (tokio task) that pushes frames into a `tokio::sync::broadcast` channel.
- Multiple HTTP clients can subscribe to the same camera stream simultaneously.
- The broadcast buffer must drop old frames when a client is slow — never block the capture loop.
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
GET  /                               — healthcheck
GET  /cameras                        — list all devices across all active backends
PUT  /cameras/{id}/connect           — open a session with a device
PUT  /cameras/{id}/disconnect        — close a session with a device
```

## Canon SDK
- SDK files live in `external/EDSDK/` (git-ignored).
- Windows 64-bit library: `external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library/EDSDK.lib`
- Windows 64-bit DLL: `external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Dll/EDSDK.dll`
- `build.rs` links the SDK library and copies the DLLs to the build output directory automatically.

## File structure
```
src/
  main.rs             — server startup, backend registry, route registration
  camera/
    mod.rs            — CameraBackend trait, DeviceId, DeviceInfo, CameraError
  backends/
    mod.rs            — feature-gated module declarations
    canon.rs          — FFI bindings + impl CameraBackend for CanonBackend
  routes/
    mod.rs
    cameras.rs        — route handlers (list, connect, disconnect, ...)
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
