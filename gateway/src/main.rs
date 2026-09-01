//! ratewall-gateway — process bootstrap for the reverse proxy.
//!
//! Phase 1 scope: config-driven routing (`/crm/*`, `/hrm/*`), request-id
//! middleware, structured tracing. Auth, rate limiting and circuit breakers
//! arrive in later phases (see BUILD_PLAN.md).

use std::net::SocketAddr;

use axum::middleware::from_fn;
use axum::Router;

use ratewall_core::config::GatewayConfig;
use ratewall_core::middleware::request_id_and_trace;
use ratewall_core::router::{build_router, ProxyState};

const DEFAULT_CONFIG_PATH: &str = "/etc/ratewall/config.toml";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ratewall=info,tower_http=info".into()),
        )
        .init();

    // ── Config: load, validate, refuse to start on bad config ──────────
    let config_path =
        std::env::var("RATEWALL_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    let config = match std::fs::read_to_string(&config_path) {
        Ok(text) => match GatewayConfig::from_toml(&text) {
            Ok(config) => {
                tracing::info!(path = %config_path, "config loaded and validated");
                config
            }
            Err(err) => {
                eprintln!("ratewall: refusing to start — invalid config: {err}");
                std::process::exit(1);
            }
        },
        Err(err) => {
            eprintln!(
                "ratewall: refusing to start — cannot read config at {config_path}: {err}\n\
                 Set RATEWALL_CONFIG or mount a config.toml with at least one [routes] entry."
            );
            std::process::exit(1);
        }
    };

    let routes = match config.validate() {
        Ok(routes) => routes,
        Err(err) => {
            eprintln!("ratewall: refusing to start — config validation failed: {err}");
            std::process::exit(1);
        }
    };
    for route in &routes {
        tracing::info!(prefix = %route.prefix, backend = %route.backend, "route configured");
    }

    // ── Router + middleware ─────────────────────────────────────────────
    let state = ProxyState::new(&routes).expect("failed to build proxy state");
    let app: Router = build_router(state).layer(from_fn(request_id_and_trace));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!(%addr, "ratewall gateway listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind gateway port");
    axum::serve(listener, app)
        .await
        .expect("gateway server error");
}
