use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response, Html},
    routing::get,
    Router,
};
use nokhwa::Camera;
use nokhwa::utils::{CameraIndex, RequestedFormatType};
use nokhwa::pixel_format::RgbFormat;
use bytes::Bytes;

#[derive(Clone, serde::Serialize)]
struct CameraInfo {
    index: u32,
    name: String,
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(get_index))
        .route("/cameras", get(list_cameras))
        .route("/snapshot/{camera_id}", get(snapshot_jpeg))
        .route("/stream/{camera_id}", get(stream_mjpeg));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .unwrap();

    println!("Server running on http://0.0.0.0:8080");
    println!("  Interface: http://localhost:8080");
    println!("  API cameras: http://localhost:8080/cameras");
    println!("  Snapshot: http://localhost:8080/snapshot/0");
    println!("  Stream camera 0: http://localhost:8080/stream/0");
    
    axum::serve(listener, app).await.unwrap();
}

async fn get_index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn list_cameras() -> impl IntoResponse {
    // Try to enumerate cameras by attempting to open them
    let mut cameras = Vec::new();
    
    for i in 0..10 {
        match Camera::new(
            CameraIndex::Index(i),
            nokhwa::utils::RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
        ) {
            Ok(_) => {
                cameras.push(CameraInfo {
                    index: i,
                    name: format!("Camera {}", i),
                });
            }
            Err(_) => {}
        }
    }

    let json = serde_json::to_string(&cameras).unwrap_or_else(|_| "[]".to_string());
    (StatusCode::OK, [("Content-Type", "application/json")], json)
}

async fn snapshot_jpeg(Path(camera_id): Path<u32>) -> Response {
    match tokio::task::spawn_blocking(move || {
        capture_frame_jpeg(camera_id)
    }).await {
        Ok(Ok(data)) => {
            (
                StatusCode::OK,
                [("Content-Type", "image/jpeg")],
                data,
            )
                .into_response()
        }
        Ok(Err(_)) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to capture frame").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Task failed").into_response(),
    }
}

fn capture_frame_jpeg(camera_id: u32) -> Result<Vec<u8>, String> {
    // Build the camera first, then query all formats exposed by the driver.
    // This avoids hardcoding guesses that may not match the active backend.
    let mut camera = Camera::new(
        CameraIndex::Index(camera_id),
        nokhwa::utils::RequestedFormat::new::<RgbFormat>(RequestedFormatType::None),
    )
    .map_err(|e| format!("Failed to create camera: {}", e))?;

    let formats = camera
        .compatible_camera_formats()
        .map_err(|e| format!("Failed to list camera formats: {}", e))?;

    let best = formats
        .iter()
        .copied()
        .max_by_key(|fmt| {
            let res = fmt.resolution();
            let pixels = (res.width_x as u64) * (res.height_y as u64);
            let codec_rank = match fmt.format() {
                nokhwa::utils::FrameFormat::MJPEG => 2u8,
                nokhwa::utils::FrameFormat::YUYV
                | nokhwa::utils::FrameFormat::NV12
                | nokhwa::utils::FrameFormat::RAWRGB
                | nokhwa::utils::FrameFormat::RAWBGR => 1u8,
                _ => 0u8,
            };
            (pixels, codec_rank, fmt.frame_rate())
        })
        .ok_or("No compatible format found")?;

    camera
        .set_camera_requset(nokhwa::utils::RequestedFormat::with_formats(
            RequestedFormatType::Exact(best),
            &[best.format()],
        ))
        .map_err(|e| format!("Failed to set best photo format: {}", e))?;
    
    camera.open_stream().map_err(|e| format!("Failed to open stream: {}", e))?;
    let frame = camera.frame().map_err(|e| format!("Failed to read frame: {}", e))?;
    let rgb_data = frame.decode_image::<RgbFormat>().map_err(|e| format!("Failed to decode: {}", e))?;
    let (width, height) = (frame.resolution().width_x, frame.resolution().height_y);
    
    println!(
        "Captured frame at resolution: {}x{} ({:?} @ {} fps)",
        width,
        height,
        best.format(),
        best.frame_rate()
    );
    
    let img = image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(
        width,
        height,
        rgb_data.to_vec(),
    )
    .ok_or("Failed to create image buffer")?;
    
    let mut jpeg_data = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut jpeg_data);
    encoder.encode(
        &img,
        width,
        height,
        image::ExtendedColorType::Rgb8,
    ).map_err(|e| format!("Failed to encode: {}", e))?;
    
    Ok(jpeg_data)
}

async fn stream_mjpeg(Path(camera_id): Path<u32>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(2);

    // One dedicated OS thread per stream: camera access is blocking and not Send across tokio tasks.
    std::thread::spawn(move || {
        stream_frames_blocking(camera_id, tx);
    });
    
    let body = axum::body::Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx)
    );
    
    (
        StatusCode::OK,
        [("Content-Type", "multipart/x-mixed-replace; boundary=frame")],
        body,
    )
        .into_response()
}

fn stream_frames_blocking(camera_id: u32, tx: tokio::sync::mpsc::Sender<Result<Bytes, String>>) {
    let mut camera = match Camera::new(
        CameraIndex::Index(camera_id),
        nokhwa::utils::RequestedFormat::with_formats(
            RequestedFormatType::AbsoluteHighestFrameRate,
            &[nokhwa::utils::FrameFormat::MJPEG],
        ),
    ) {
        Ok(c) => c,
        Err(_) => return,
    };
    
    if camera.open_stream().is_err() {
        return;
    }
    
    loop {
        match camera.frame() {
            Ok(frame) => {
                let jpeg_data = if frame.source_frame_format() == nokhwa::utils::FrameFormat::MJPEG {
                    Some(frame.buffer().to_vec())
                } else {
                    match frame.decode_image::<RgbFormat>() {
                        Ok(rgb_data) => {
                            let (width, height) = (frame.resolution().width_x, frame.resolution().height_y);
                            if let Some(img) = image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(
                                width,
                                height,
                                rgb_data.to_vec(),
                            ) {
                                let mut encoded = Vec::new();
                                let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 55);
                                if encoder
                                    .encode(&img, width, height, image::ExtendedColorType::Rgb8)
                                    .is_ok()
                                {
                                    Some(encoded)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        Err(_) => None,
                    }
                };

                if let Some(jpeg_data) = jpeg_data {
                    let frame_header = format!(
                        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        jpeg_data.len()
                    );

                    let mut response = Vec::with_capacity(frame_header.len() + jpeg_data.len() + 2);
                    response.extend_from_slice(frame_header.as_bytes());
                    response.extend_from_slice(&jpeg_data);
                    response.extend_from_slice(b"\r\n");

                    match tx.try_send(Ok(Bytes::from(response))) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // Drop stale frame when client/network is slower than camera.
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }
            Err(_) => break,
        }
    }
}
