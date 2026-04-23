mod backends;
mod camera;
mod routes;

use std::collections::HashMap;
use std::sync::Arc;

use axum::{routing::{get, put}, Json, Router};
use axum::response::Html;
use serde::Serialize;

use routes::cameras::{self, AppState, BackendState};

#[derive(Serialize)]
struct HealthCheck {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn health() -> Json<HealthCheck> {
    Json(HealthCheck {
        status: "ok",
        service: "bird-camera-server",
        version: env!("CARGO_PKG_VERSION"),
    })
}

fn build_backends() -> BackendState {
    #[allow(unused_mut)]
    let mut map: HashMap<String, Arc<dyn camera::CameraBackend>> = HashMap::new();

    #[cfg(feature = "backend-canon")]
    match backends::canon::CanonBackend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] Canon backend failed to initialize: {e}"),
    }

    Arc::new(map)
}

async fn run_server() {
    let backends = build_backends();
    let state = AppState::new(backends);

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/cameras", get(cameras::list_cameras))
        .route("/cameras/{id}/connect", put(cameras::connect_camera))
        .route("/cameras/{id}/disconnect", put(cameras::disconnect_camera))
        .route("/cameras/{id}/parameters", get(cameras::get_parameters))
        .route("/cameras/{id}/liveview", get(cameras::live_view))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080")
        .await
        .expect("failed to bind to 127.0.0.1:8080");

    println!("Listening on http://{}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

// On macOS the EDSDK registers its IOKit USB-detection sources on the main CF
// run loop (CFRunLoopGetMain). We must keep the main thread free to pump it,
// so tokio runs on a background thread instead.
#[cfg(target_os = "macos")]
fn main() {
    std::thread::spawn(|| {
        tokio::runtime::Runtime::new()
            .expect("failed to build tokio runtime")
            .block_on(run_server());
    });

    // Pump the main CF run loop forever. The EDSDK's IOKit USB notifications
    // fire here, making cameras visible to EdsGetCameraList.
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" { fn CFRunLoopRun(); }
    unsafe { CFRunLoopRun() };
}

#[cfg(not(target_os = "macos"))]
#[tokio::main]
async fn main() {
    run_server().await;
}
