//! Shared bootstrap for the mock CRM/HRM stubs.
//!
//! Each backend serves a single catch-all route that echoes which service
//! it represents and the path that was requested — enough for the gateway to
//! prove routing round-trips without real services.

use std::net::SocketAddr;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::any;
use axum::{Json, Router};
use serde_json::json;

/// Serve a catch-all echo endpoint as `service` on `port`.
pub async fn serve(service: &'static str, port: u16) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = Router::new()
        .route("/*path", any(echo_path))
        .fallback(echo_root);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%service, %addr, "mock backend listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("{service} failed to bind {addr}: {e}"));
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("{service} server error: {e}"));
}

async fn echo_path(Path(path): Path<String>) -> axum::response::Response {
    echo_body(&path).into_response()
}

async fn echo_root() -> axum::response::Response {
    echo_body("").into_response()
}

fn echo_body(path: &str) -> (StatusCode, Json<serde_json::Value>) {
    tracing::info!(%path, "mock backend request");
    (
        StatusCode::OK,
        Json(json!({
            "service": std::env::var("MOCK_SERVICE").unwrap_or_default(),
            "path": format!("/{path}"),
        })),
    )
}
