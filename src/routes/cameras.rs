use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use serde_json::json;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::camera::{CameraBackend, CameraError, DeviceId, DeviceInfo};

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

pub type BackendState = Arc<HashMap<String, Arc<dyn CameraBackend>>>;

/// One broadcast sender per active live-view device.
/// The key is the opaque (encoded) device ID.
type LiveViewSenders = Arc<Mutex<HashMap<String, broadcast::Sender<Arc<Bytes>>>>>;

#[derive(Clone)]
pub struct AppState {
    pub backends: BackendState,
    pub live_views: LiveViewSenders,
}

impl axum::extract::FromRef<AppState> for BackendState {
    fn from_ref(state: &AppState) -> Self {
        state.backends.clone()
    }
}

impl AppState {
    pub fn new(backends: BackendState) -> Self {
        Self {
            backends,
            live_views: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn list_cameras(State(backends): State<BackendState>) -> Json<Vec<DeviceInfo>> {
    let mut devices = Vec::new();
    for backend in backends.values() {
        match backend.list_devices() {
            Ok(mut found) => devices.append(&mut found),
            Err(e) => eprintln!("[error] failed to list devices from backend: {e}"),
        }
    }
    Json(devices)
}

pub async fn connect_camera(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    dispatch(&backends, &id, |b, native_id| b.connect(native_id))
}

pub async fn get_parameters(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
) -> Response {
    let dev_id = match DeviceId::decode(&id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    let native_id = dev_id.native_id.clone();
    let result =
        tokio::task::spawn_blocking(move || backend.get_parameters(&native_id)).await;

    match result {
        Ok(Ok(params)) => Json(params).into_response(),
        Ok(Err(CameraError::NotConnected)) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "device is not connected" })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error" })),
        )
            .into_response(),
    }
}

pub async fn disconnect_camera(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    dispatch(&backends, &id, |b, native_id| b.disconnect(native_id))
}

pub async fn live_view(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let dev_id = match DeviceId::decode(&id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match state.backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    // Pre-flight: reject before sending any headers if device is not connected.
    {
        let backend_clone = backend.clone();
        let native_id = dev_id.native_id.clone();
        let connected = tokio::task::spawn_blocking(move || backend_clone.is_connected(&native_id))
            .await
            .unwrap_or(false);

        if !connected {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "device is not connected" })),
            )
                .into_response();
        }
    }

    // Subscribe to the shared broadcast channel, starting a capture loop if needed.
    let rx = {
        let mut senders = state.live_views.lock().await;
        let sender = senders.entry(id.clone()).or_insert_with(|| {
            // Buffer a few frames to absorb jitter without adding latency.
            let (tx, _) = broadcast::channel::<Arc<Bytes>>(4);
            tx
        });

        let rx = sender.subscribe();

        // If we are the first subscriber, spawn the capture loop.
        // receiver_count includes the receiver we just created.
        if sender.receiver_count() == 1 {
            let tx = sender.clone();
            let backend_loop = backend.clone();
            let native_id = dev_id.native_id.clone();

            tokio::spawn(async move {
                loop {
                    // No subscribers left — stop.
                    if tx.receiver_count() == 0 {
                        break;
                    }

                    let b = backend_loop.clone();
                    let nid = native_id.clone();
                    let result =
                        tokio::task::spawn_blocking(move || b.get_live_view_frame(&nid)).await;

                    match result {
                        Ok(Ok(jpeg)) => {
                            let mut buf = format!(
                                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                jpeg.len()
                            )
                            .into_bytes();
                            buf.extend_from_slice(&jpeg);
                            buf.extend_from_slice(b"\r\n");

                            // send() only errors when there are no receivers.
                            if tx.send(Arc::new(Bytes::from(buf))).is_err() {
                                break;
                            }
                        }
                        Ok(Err(crate::camera::CameraError::SdkError(0x0000_A102))) => {
                            // EDS_ERR_OBJECT_NOTREADY: EVF not ready yet, skip frame.
                            continue;
                        }
                        Ok(Err(e)) => {
                            eprintln!("[error] live view frame error for {native_id}: {e}");
                            break;
                        }
                        Err(_) => break, // spawn_blocking panicked
                    }
                }
            });
        }

        rx
    };

    // Convert the broadcast receiver into an HTTP body stream.
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(frame) => Some(Ok::<Bytes, std::io::Error>((*frame).clone())),
        Err(_) => None, // lagged frames — just skip
    });

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        )
        .body(Body::from_stream(stream))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decodes an opaque device ID, routes to the correct backend, and runs `op`.
fn dispatch(
    backends: &HashMap<String, Arc<dyn CameraBackend>>,
    opaque_id: &str,
    op: impl Fn(&Arc<dyn CameraBackend>, &str) -> Result<(), CameraError>,
) -> impl IntoResponse {
    let dev_id = match DeviceId::decode(opaque_id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match backends.get(&dev_id.backend) {
        Some(b) => b,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    match op(backend, &dev_id.native_id) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(CameraError::DeviceNotFound(id)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("device not found: {id}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
