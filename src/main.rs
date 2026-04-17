use axum::{
    extract::Path,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
    routing::get,
    Router,
};
use nokhwa::Camera;
use bytes::Bytes;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType};

#[derive(Clone, serde::Serialize)]
struct CameraSummary {
    index: u32,
    name: String,
}

#[derive(Clone, serde::Serialize)]
struct ResolutionCompatibility {
    width: u32,
    height: u32,
    fps: Vec<u32>,
}

#[derive(Clone, serde::Serialize)]
struct FrameFormatCompatibility {
    frame_format: String,
    resolutions: Vec<ResolutionCompatibility>,
}

#[derive(Clone, serde::Serialize)]
struct CameraCompatibility {
    index: u32,
    name: String,
    compatible_by_resolution: Vec<FrameFormatCompatibility>,
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(get_index))
        .route("/cameras", get(list_cameras))
        .route(
            "/cameras/{camera_id}/compatible-list-by-resolution",
            get(list_camera_compatible_resolutions),
        )
        .route("/snapshot/{camera_id}", get(snapshot_jpeg))
        .route("/stream/{camera_id}", get(stream_mjpeg));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .unwrap();

    println!("Server running on http://0.0.0.0:8080");
    println!("  Interface: http://localhost:8080");
    println!("  API cameras: http://localhost:8080/cameras");
    println!("  API camera compatibilities: http://localhost:8080/cameras/compatible-list-by-resolution");
    println!("  Snapshot: http://localhost:8080/snapshot/0");
    println!("  Stream camera 0: http://localhost:8080/stream/0");
    
    axum::serve(listener, app).await.unwrap();
}

async fn get_index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn list_cameras() -> impl IntoResponse {
    let cameras = enumerate_cameras();
    Json(cameras)
}

async fn list_camera_compatible_resolutions(Path(camera_id): Path<u32>) -> impl IntoResponse {
    match tokio::task::spawn_blocking(move || enumerate_camera_compatibility(camera_id)).await {
        Ok(Some(compatibility)) => (StatusCode::OK, Json(compatibility)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(join_err) => {
            eprintln!("Compatibility task failed (camera {}): {}", camera_id, join_err);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn snapshot_jpeg(Path(camera_id): Path<u32>) -> Response {
    match tokio::task::spawn_blocking(move || {
        capture_photo_jpeg(camera_id)
    }).await {
        Ok(Ok(data)) => {
            (
                StatusCode::OK,
                [("Content-Type", "image/jpeg")],
                data,
            )
                .into_response()
        }
        Ok(Err(err)) => {
            eprintln!("Snapshot error (camera {}): {}", camera_id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to capture photo: {}", err),
            )
                .into_response()
        }
        Err(join_err) => {
            eprintln!("Snapshot task failed (camera {}): {}", camera_id, join_err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Task failed: {}", join_err),
            )
                .into_response()
        }
    }
}

fn capture_photo_jpeg(camera_id: u32) -> Result<Vec<u8>, String> {
    // Use the dedicated photo path instead of streaming frame capture.
    let mut camera = Camera::new(
        CameraIndex::Index(camera_id),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::None),
    )
    .map_err(|e| format!("Failed to create camera: {}", e))?;
    let photo = camera.photo().map_err(|e| format!("Failed to capture photo: {}", e))?;
        if photo.source_frame_format() == nokhwa::utils::FrameFormat::MJPEG {
            return Ok(photo.buffer().to_vec());
        }

    let rgb_img = photo
        .decode_image::<RgbFormat>()
        .map_err(|e| format!("Failed to decode photo: {}", e))?;
    let (width, height) = (photo.resolution().width_x, photo.resolution().height_y);
    
    println!(
        "Captured photo at resolution: {}x{} ({:?})",
        width,
        height,
        photo.source_frame_format()
    );

    let mut jpeg_data = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_data, 95);
    encoder.encode(
        rgb_img.as_raw(),
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
        RequestedFormat::with_formats(
            RequestedFormatType::AbsoluteHighestResolution,
            &[FrameFormat::MJPEG],
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

fn enumerate_cameras() -> Vec<CameraSummary> {
    let mut cameras = Vec::new();

    for i in 0..10 {
        match Camera::new(
            CameraIndex::Index(i),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution),
        ) {
            Ok(camera) => {
                cameras.push(CameraSummary {
                    index: i,
                    name: camera.info().human_name(),
                });
            }
            Err(_) => {}
        }
    }

    cameras
}

fn enumerate_camera_compatibility(camera_id: u32) -> Option<CameraCompatibility> {
    let mut camera = Camera::new(
        CameraIndex::Index(camera_id),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution),
    )
    .ok()?;

    let frame_formats = camera.compatible_fourcc().ok()?;

    let mut compatible_by_resolution = Vec::new();
    for frame_format in frame_formats {
        let mut resolutions = camera
            .compatible_list_by_resolution(frame_format)
            .ok()?
            .into_iter()
            .map(|(resolution, mut fps)| {
                fps.sort_unstable();
                fps.dedup();
                ResolutionCompatibility {
                    width: resolution.width_x,
                    height: resolution.height_y,
                    fps,
                }
            })
            .collect::<Vec<_>>();

        resolutions.sort_unstable_by(|a, b| a.width.cmp(&b.width).then(a.height.cmp(&b.height)));

        compatible_by_resolution.push(FrameFormatCompatibility {
            frame_format: frame_format.to_string(),
            resolutions,
        });
    }

    compatible_by_resolution.sort_unstable_by(|a, b| {
        let a_score = quality_score_for_format(a);
        let b_score = quality_score_for_format(b);
        b_score.cmp(&a_score)
    });

    Some(CameraCompatibility {
        index: camera_id,
        name: camera.info().human_name(),
        compatible_by_resolution,
    })
}

fn quality_score_for_format(entry: &FrameFormatCompatibility) -> (u32, u8, u32, u32) {
    let codec_score = match entry.frame_format.as_str() {
        "MJPEG" => 3,
        "RAWRGB" | "RAWBGR" => 2,
        "YUYV" | "NV12" => 1,
        "GRAY" => 0,
        _ => 0,
    };

    let mut max_area = 0_u32;
    let mut max_width = 0_u32;
    let mut max_fps = 0_u32;
    for resolution in &entry.resolutions {
        let area = resolution.width.saturating_mul(resolution.height);
        if area > max_area {
            max_area = area;
            max_width = resolution.width;
        }
        if let Some(local_max_fps) = resolution.fps.iter().copied().max() {
            if local_max_fps > max_fps {
                max_fps = local_max_fps;
            }
        }
    }

    // Resolution first so photo modes like 2304x1536 rank above 1080p preview modes.
    (max_area, codec_score, max_width, max_fps)
}
