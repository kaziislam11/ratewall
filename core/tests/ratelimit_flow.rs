//! Phase 3 integration tests, driven through the real router:
//!
//! - Rate limiting enforces the configured cap: over-limit requests get
//!   429 with a `retry-after` header.
//! - **Fail-open holds**: with an unreachable Redis, the same traffic
//!   passes — enforcement degrades, availability does not (ADR-0001).
//!
//! The enforcement test needs a real Redis. `docker compose up redis`
//! provides one; set `RATEWALL_TEST_REDIS` to its URL (default
//! `redis://127.0.0.1:6379`). The fail-open test needs nothing — it points
//! at a port that refuses connections, which is the whole point.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn;
use ratewall_core::auth::DEFAULT_TOKEN_TTL;
use ratewall_core::auth_login::LoginState;
use ratewall_core::config::{RateLimitConfig, Route};
use ratewall_core::middleware::request_id_and_trace;
use ratewall_core::middleware_auth::AuthState;
use ratewall_core::ratelimit::RateLimiter;
use ratewall_core::router::{build_router, ProxyState};
use tower::ServiceExt;

const OWN_ISSUER: &str = "ratewall";

async fn spawn_mock_backend() -> String {
    use axum::response::IntoResponse;
    use axum::Router as AppRouter;

    let app = AppRouter::new().fallback(|request: axum::extract::Request| async move {
        let path = request.uri().path().to_string();
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "path": path, "service": "crm" })),
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Mirror of the assembly in gateway/src/main.rs, parameterized over the
/// limiter so each scenario can point it at its own Redis. `key_prefix`
/// namespaces the counters (used by the enforcement test to get a fresh
/// window per run).
async fn build_app(rl: RateLimitConfig, key_prefix: Option<&str>) -> axum::Router {
    let backend_url = spawn_mock_backend().await;
    let routes = vec![Route {
        prefix: "crm".into(),
        backend: backend_url,
    }];
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
    let auth_state = AuthState::new(signing_key.verifying_key(), Some(OWN_ISSUER.into()));
    let login_state = LoginState {
        signing_key: Arc::new(signing_key),
        issuer: Arc::from(OWN_ISSUER),
        ttl: DEFAULT_TOKEN_TTL,
    };
    let limiter = RateLimiter::connect(
        &rl.redis_url,
        rl.limit,
        std::time::Duration::from_secs(rl.window_secs),
        key_prefix,
    )
    .expect("limiter");

    let state = ProxyState::new(&routes)
        .expect("proxy state")
        .with_auth(auth_state)
        .with_limiter(limiter);
    build_router(state)
        .nest("/auth", ratewall_core::auth_login::router(login_state))
        .layer(from_fn(request_id_and_trace))
}

fn test_rl(url: &str, limit: u32) -> RateLimitConfig {
    RateLimitConfig {
        redis_url: url.into(),
        limit,
        window_secs: 60,
    }
}

async fn login(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::post("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"username":"demo","password":"demo-password"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The enforcement test needs a reachable Redis; skip (print, not fail)
/// when none is provided. The fail-open test below always runs.
fn redis_url() -> Option<String> {
    match std::env::var("RATEWALL_TEST_REDIS") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => None,
    }
}

#[tokio::test]
async fn over_limit_gets_429_with_retry_after() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: set RATEWALL_TEST_REDIS (e.g. docker compose up redis)");
        return;
    };
    // Unique prefix per run so re-execution doesn't inherit a full window.
    // (The marker goes into the limiter's key, NOT the URL: a URL path is
    // parsed by redis-rs as a database number, and a nanosecond-sized one
    // makes SELECT fail — every counter op would silently fail open.)
    let marker = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let rl = test_rl(&url, 5);
    let app = build_app(rl, Some(&format!("{marker}"))).await;
    let token = login(&app).await;

    // First 5 requests pass (limit = 5) and reach the backend.
    for i in 0..5 {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/crm/req-{i}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "request {i} of 5");
    }

    // The 6th is over the cap: 429 with retry-after.
    let response = app
        .clone()
        .oneshot(
            Request::get("/crm/req-6")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .expect("retry-after header")
        .to_string();
    assert_eq!(retry_after, "60");
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&body[..], b"rate limit exceeded");
}

#[tokio::test]
async fn dead_redis_fails_open() {
    // Port 1 refuses connections: the mandatory ADR-0001 scenario. Under
    // load, every request must pass — the limiter degrades to no-op, it
    // must never 429 or 5xx.
    let app = build_app(test_rl("redis://127.0.0.1:1", 1), None).await;
    let token = login(&app).await;

    for i in 0..20 {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/crm/failopen-{i}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "request {i} must pass when redis is dead"
        );
    }
}
