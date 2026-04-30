use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use crate::routes::cameras::AppState;

pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let header_ok = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v: &str| v.strip_prefix("Bearer "))
        .map(|t: &str| t == state.token.as_str())
        .unwrap_or(false);

    let query_ok = request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .any(|part: &str| {
            let mut kv = part.splitn(2, '=');
            kv.next() == Some("token") && kv.next() == Some(state.token.as_str())
        });

    if header_ok || query_ok {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "invalid or missing token" })),
        )
            .into_response()
    }
}
