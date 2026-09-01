//! ratewall-gateway — process bootstrap for the reverse proxy.
//!
//! Phase 0 scope: an axum server exposing `GET /healthz` and nothing else.
//! Routing, auth, rate limiting and circuit breakers arrive in later phases
//! (see BUILD_PLAN.md).

use std::net::SocketAddr;

use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ratewall=info,tower_http=info".into()),
        )
        .init();

    let port: u16 = std::env::var("RATEWALL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let app = Router::new().route("/healthz", get(healthz));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "ratewall gateway listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind gateway port");
    axum::serve(listener, app)
        .await
        .expect("gateway server error");
}

/// Liveness probe: process is up. Deliberately checks nothing else —
/// `/readyz` (real backend + Redis health) arrives in Phase 4.
async fn healthz() -> StatusCode {
    StatusCode::OK
}
