use axum::{
    extract::{Json, Path},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, put},
    Router,
};
mod camera;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[derive(Debug, Deserialize)]
struct PutPayload {
    message: String,
}

#[derive(Debug, Serialize)]
struct RootResponse {
    service: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct PutResponse {
    ok: bool,
    received: String,
}

#[derive(Debug, Serialize)]
struct CamerasResponse {
    backend: &'static str,
    count: usize,
    cameras: Vec<camera::CameraInfo>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(get_root).put(put_root))
        .route("/update", put(put_update))
        .route("/cameras", get(get_cameras))
        .route("/cameras/{camera_id}/photo", get(get_camera_photo));

    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .expect("failed to bind an available port");

    let bound_addr: SocketAddr = listener
        .local_addr()
        .expect("failed to read bound socket address");

    println!("bird-camera-server listening on http://{bound_addr}");

    axum::serve(listener, app)
        .await
        .expect("server error");
}

async fn get_root() -> Json<RootResponse> {
    Json(RootResponse {
        service: "bird-camera-server",
        status: "ok",
    })
}

async fn put_root(Json(payload): Json<PutPayload>) -> Json<PutResponse> {
    Json(PutResponse {
        ok: true,
        received: payload.message,
    })
}

async fn put_update(Json(payload): Json<PutPayload>) -> Json<PutResponse> {
    Json(PutResponse {
        ok: true,
        received: payload.message,
    })
}

async fn get_cameras() -> (StatusCode, Json<serde_json::Value>) {
    match camera::list_cameras() {
        Ok(cameras) => (
            StatusCode::OK,
            Json(serde_json::json!(CamerasResponse {
                backend: "windows-native-media-foundation",
                count: cameras.len(),
                cameras,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!(ErrorResponse { error })),
        ),
    }
}

async fn get_camera_photo(Path(camera_id): Path<String>) -> impl IntoResponse {
    match camera::capture_photo_jpeg(&camera_id) {
        Ok(jpeg_bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg")],
            jpeg_bytes,
        )
            .into_response(),
        Err(error) => {
            let status = if error.contains("camera not found") || error.contains("invalid camera id") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };

            (status, Json(serde_json::json!(ErrorResponse { error }))).into_response()
        }
    }
}
