use axum::{
    extract::{Json, Path, Query},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post, put},
    Router,
};
mod camera;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;
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
    count: usize,
    cameras: Vec<camera::CameraInfo>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    ok: bool,
    camera_id: String,
}

#[derive(Debug, Deserialize)]
struct ResolutionQuery {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SetResolutionPayload {
    width: u32,
    height: u32,
}

#[derive(Debug, Serialize)]
struct CameraResolutionsResponse {
    camera_id: String,
    available: Vec<camera::CameraResolution>,
    preview_selected: Option<camera::CameraResolution>,
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(get_root).put(put_root))
        .route("/ui", get(get_ui))
        .route("/update", put(put_update))
        .route("/cameras", get(get_cameras))
        .route("/cameras/{camera_id}/connect", post(post_camera_connect))
        .route("/cameras/{camera_id}/disconnect", post(post_camera_disconnect))
        .route("/cameras/{camera_id}/resolutions", get(get_camera_resolutions))
        .route(
            "/cameras/{camera_id}/preview-resolution",
            put(put_camera_preview_resolution),
        )
        .route("/cameras/{camera_id}/photo", get(get_camera_photo))
        .route("/cameras/{camera_id}/stream", get(get_camera_stream));

    let listener = TcpListener::bind("127.0.0.1:6969")
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

async fn get_ui() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
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

fn resolution_from_query(query: &ResolutionQuery) -> Result<Option<camera::CameraResolution>, String> {
    match (query.width, query.height) {
        (Some(width), Some(height)) => {
            if width == 0 || height == 0 {
                return Err("width and height must be greater than 0".to_string());
            }
            Ok(Some(camera::CameraResolution { width, height }))
        }
        (None, None) => Ok(None),
        _ => Err("both width and height must be provided".to_string()),
    }
}

async fn get_camera_resolutions(Path(camera_id): Path<String>) -> impl IntoResponse {
    match (
        camera::list_camera_resolutions(&camera_id),
        camera::get_camera_preview_resolution(&camera_id),
    ) {
        (Ok(available), Ok(preview_selected)) => (
            StatusCode::OK,
            Json(serde_json::json!(CameraResolutionsResponse {
                camera_id,
                available,
                preview_selected,
            })),
        )
            .into_response(),
        (Err(error), _) | (_, Err(error)) => {
            let status = if error.contains("camera not found") || error.contains("invalid camera id") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };

            (status, Json(serde_json::json!(ErrorResponse { error }))).into_response()
        }
    }
}

async fn put_camera_preview_resolution(
    Path(camera_id): Path<String>,
    Json(payload): Json<SetResolutionPayload>,
) -> impl IntoResponse {
    let resolution = camera::CameraResolution {
        width: payload.width,
        height: payload.height,
    };

    match camera::set_camera_preview_resolution(&camera_id, resolution) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "camera_id": camera_id,
                "preview_resolution": resolution,
            })),
        )
            .into_response(),
        Err(error) => {
            let status = if error.contains("camera not found") || error.contains("invalid camera id") {
                StatusCode::NOT_FOUND
            } else if error.contains("not connected") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };

            (status, Json(serde_json::json!(ErrorResponse { error }))).into_response()
        }
    }
}

async fn get_camera_photo(
    Path(camera_id): Path<String>,
    Query(query): Query<ResolutionQuery>,
) -> impl IntoResponse {
    let preferred_resolution = match resolution_from_query(&query) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!(ErrorResponse { error })),
            )
                .into_response();
        }
    };

    match camera::capture_photo_jpeg_with_resolution(&camera_id, preferred_resolution) {
        Ok(jpeg_bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg")],
            jpeg_bytes,
        )
            .into_response(),
        Err(error) => {
            let status = if error.contains("camera not found") || error.contains("invalid camera id") {
                StatusCode::NOT_FOUND
            } else if error.contains("not connected") {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };

            (status, Json(serde_json::json!(ErrorResponse { error }))).into_response()
        }
    }
}

async fn post_camera_connect(Path(camera_id): Path<String>) -> impl IntoResponse {
    match camera::connect_camera(&camera_id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(SessionResponse {
                ok: true,
                camera_id,
            })),
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

async fn post_camera_disconnect(Path(camera_id): Path<String>) -> impl IntoResponse {
    match camera::disconnect_camera(&camera_id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(SessionResponse {
                ok: true,
                camera_id,
            })),
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

async fn get_camera_stream(
    Path(camera_id): Path<String>,
    Query(query): Query<ResolutionQuery>,
) -> impl IntoResponse {
    let query_resolution = match resolution_from_query(&query) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!(ErrorResponse { error })),
            )
                .into_response();
        }
    };

    let preview_resolution = if query_resolution.is_some() {
        query_resolution
    } else {
        match camera::get_camera_preview_resolution(&camera_id) {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!(ErrorResponse { error })),
                )
                    .into_response();
            }
        }
    };

    // Check if camera is connected
    if !camera::is_camera_connected(&camera_id) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!(ErrorResponse {
                error: "camera not connected".to_string()
            })),
        )
            .into_response();
    }

    // Create a channel to send frame data
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(10);
    let camera_id_clone = camera_id.clone();

    // Spawn a task that reads frames and sends them to the channel
    tokio::spawn(async move {
        loop {
            match camera::capture_photo_jpeg_with_resolution(&camera_id_clone, preview_resolution) {
                Ok(jpeg_bytes) => {
                    // Send boundary
                    if tx
                        .send(Ok(axum::body::Bytes::from_static(b"--frame\r\n")))
                        .await
                        .is_err()
                    {
                        break;
                    }

                    // Send content-type and content-length
                    let mut header = Vec::new();
                    header.extend(b"Content-Type: image/jpeg\r\nContent-Length: ");
                    header.extend(jpeg_bytes.len().to_string().as_bytes());
                    header.extend(b"\r\n\r\n");

                    if tx.send(Ok(axum::body::Bytes::from(header))).await.is_err() {
                        break;
                    }

                    // Send JPEG data
                    if tx.send(Ok(axum::body::Bytes::from(jpeg_bytes))).await.is_err() {
                        break;
                    }

                    // Send final CRLF
                    if tx.send(Ok(axum::body::Bytes::from_static(b"\r\n"))).await.is_err() {
                        break;
                    }
                }
                Err(_) => {
                    // Camera disconnected or error
                    break;
                }
            }

            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "multipart/x-mixed-replace; boundary=frame"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
    )
        .into_response()
}
