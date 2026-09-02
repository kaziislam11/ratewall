//! ratewall-gateway — process bootstrap for the reverse proxy.
//!
//! Phase 2 scope, on top of Phase 1: Ed25519 key management on the keys
//! volume, the demo `/auth/login` issuer, and the fail-closed bearer
//! middleware protecting proxied routes. `/healthz` and `/auth/*` stay
//! unauthenticated by design — the health check must work for orchestrators
//! and clients need a way to obtain a token. Rate limiting and circuit
//! breakers arrive in later phases (see BUILD_PLAN.md).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware::from_fn;

use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::VerifyingKey;
use ratewall_core::auth;
use ratewall_core::auth_login::{self, LoginState};
use ratewall_core::circuit::Breakers;
use ratewall_core::config::GatewayConfig;
use ratewall_core::middleware::request_id_and_trace;
use ratewall_core::middleware_auth::AuthState;
use ratewall_core::ratelimit::RateLimiter;
use ratewall_core::router::{build_router, ProxyState};

const DEFAULT_CONFIG_PATH: &str = "/etc/ratewall/config.toml";
const DEFAULT_KEYS_DIR: &str = "/var/lib/ratewall/keys";
/// Issuer string stamped into locally issued tokens and enforced on
/// verification in own-keys mode.
const OWN_ISSUER: &str = "ratewall";

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

    // ── Auth: load key material (fail-closed on any error) ──────────────
    //
    // The default keys dir lives on a Docker volume so issued tokens stay
    // valid across restarts and container rebuilds. If the key directory is
    // unreadable or the key material is corrupt, the gateway exits — auth
    // must never fall back to "trust everything".
    let (auth_state, login_state) = if config.auth.issues_own_tokens() {
        let keys_dir = config
            .auth
            .keys_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_KEYS_DIR));
        let (signing_key, created) = match auth::load_or_create_signing_key(&keys_dir) {
            Ok(result) => result,
            Err(err) => {
                eprintln!(
                    "ratewall: refusing to start — cannot load signing keys from {}: {err}\n\
                     Auth is fail-closed: there is no safe fallback to unauthenticated operation.",
                    keys_dir.display()
                );
                std::process::exit(1);
            }
        };
        if created {
            tracing::info!(
                dir = %keys_dir.display(),
                "generated new Ed25519 signing keypair (first boot)"
            );
        } else {
            tracing::info!(dir = %keys_dir.display(), "loaded existing Ed25519 signing keypair");
        }
        let state = AuthState::new(signing_key.verifying_key(), Some(OWN_ISSUER.into()));
        let login = LoginState {
            signing_key: Arc::new(signing_key),
            issuer: Arc::from(OWN_ISSUER),
            ttl: std::time::Duration::from_secs(config.auth.token_ttl_secs),
        };
        (state, Some(login))
    } else {
        // External-issuer mode: trust the configured public key, do not issue.
        let pem_path = config
            .auth
            .issuer_public_key_pem
            .clone()
            .expect("validated: issuer mode has a key path");
        let verifying_key = match std::fs::read_to_string(&pem_path) {
            Ok(pem) => match VerifyingKey::from_public_key_pem(&pem) {
                Ok(key) => key,
                Err(err) => {
                    eprintln!(
                        "ratewall: refusing to start — {} is not a valid Ed25519 public key: {err}",
                        pem_path.display()
                    );
                    std::process::exit(1);
                }
            },
            Err(err) => {
                eprintln!(
                    "ratewall: refusing to start — cannot read issuer key {}: {err}",
                    pem_path.display()
                );
                std::process::exit(1);
            }
        };
        tracing::info!(pem = %pem_path.display(), "loaded external issuer public key");
        let issuer = config.auth.issuer.clone().expect("validated: issuer set");
        (AuthState::new(verifying_key, Some(issuer)), None)
    };

    // ── Rate limiter: fail-open, so an unreachable Redis at startup is ──
    // not an error. The pool connects lazily; every failed counter op
    // passes the request uncounted and logs a warning.
    let limiter = match RateLimiter::connect(
        &config.ratelimit.redis_url,
        config.ratelimit.limit,
        std::time::Duration::from_secs(config.ratelimit.window_secs),
        None,
    ) {
        Ok(limiter) => {
            tracing::info!(
                url = %config.ratelimit.redis_url,
                limit = config.ratelimit.limit,
                window_secs = config.ratelimit.window_secs,
                "rate limiter enabled (fail-open)"
            );
            Some(limiter)
        }
        Err(err) => {
            eprintln!("ratewall: refusing to start — invalid rate limit config: {err}");
            std::process::exit(1);
        }
    };

    // ── Router + middleware ─────────────────────────────────────────────
    //
    // The bearer gate lives inside the proxy fallback (ProxyState::with_auth)
    // so /healthz and /auth/* are never gated. request-id/trace wraps
    // everything, including auth rejections and the login endpoint.
    // One breaker per backend: CRM being slow must not degrade HRM.
    let breakers = Breakers::new(
        &routes,
        &ratewall_core::circuit::BreakerConfig {
            failure_threshold: config.breaker.failure_threshold,
            cooldown: std::time::Duration::from_secs(config.breaker.cooldown_secs),
        },
    );
    tracing::info!(
        failure_threshold = config.breaker.failure_threshold,
        cooldown_secs = config.breaker.cooldown_secs,
        timeout_secs = config.breaker.timeout_secs,
        "circuit breakers enabled (one per backend)"
    );
    let state = ProxyState::new(&routes)
        .expect("failed to build proxy state")
        .with_auth(auth_state)
        .with_limiter(limiter.expect("limiter built above"))
        .with_breakers(breakers, config.breaker.timeout_secs);

    let mut app = build_router(state);
    if let Some(login) = login_state {
        app = app.nest("/auth", auth_login::router(login));
        tracing::info!("demo login enabled at POST /auth/login (own-keys mode)");
    } else {
        tracing::info!("demo login disabled (external issuer configured)");
    }
    let app = app.layer(from_fn(request_id_and_trace));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!(%addr, "ratewall gateway listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind gateway port");
    // into_make_service_with_connect_info supplies the client IP the rate
    // limiter keys on when no token was verified.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("gateway server error");
}
