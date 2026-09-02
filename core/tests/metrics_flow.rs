//! Phase 5 integration tests: the `/metrics` scrape reflects what actually
//! happened on the proxy path — request counts, per-class responses,
//! latency observations, rejection counters, breaker gauges — and stays
//! unauthenticated while proxied routes stay gated.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ratewall_core::auth::DEFAULT_TOKEN_TTL;
use ratewall_core::auth_login::LoginState;
use ratewall_core::circuit::{BreakerConfig, Breakers};
use ratewall_core::config::{RateLimitConfig, Route};
use ratewall_core::metrics::Metrics;
use ratewall_core::middleware::request_id_and_trace;
use ratewall_core::middleware_auth::AuthState;
use ratewall_core::ratelimit::RateLimiter;
use ratewall_core::router::{build_router, ProxyState};
use tower::ServiceExt;

const OWN_ISSUER: &str = "ratewall";

/// Mock CRM that can be flipped to always-500 to exercise status classes.
struct TestBackend {
    addr: String,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl TestBackend {
    async fn spawn() -> Self {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail_handler = Arc::clone(&fail);
        let app = axum::Router::new().fallback(move || {
            let fail = Arc::clone(&fail_handler);
            async move {
                if fail.load(Ordering::SeqCst) {
                    (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response()
                } else {
                    (StatusCode::OK, "fine").into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr: format!("http://{addr}"),
            fail,
        }
    }
}

use axum::response::IntoResponse;

async fn build_app(
    backend: &str,
    rl: RateLimitConfig,
    key_prefix: &str,
) -> (axum::Router, Metrics) {
    let routes = vec![Route {
        prefix: "crm".into(),
        backend: backend.into(),
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
        Duration::from_secs(rl.window_secs),
        Some(key_prefix),
    )
    .expect("limiter");
    let breakers = Breakers::new(
        &routes,
        &BreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::from_millis(50),
        },
    );
    let metrics = Metrics::new();
    let state = ProxyState::new(&routes)
        .expect("state")
        .with_auth(auth_state)
        .with_limiter(limiter)
        .with_breakers(breakers, 5)
        .with_metrics(metrics.clone());
    let app = build_router(state)
        .nest("/auth", ratewall_core::auth_login::router(login_state))
        .layer(axum::middleware::from_fn(request_id_and_trace));
    (app, metrics)
}

/// Redis URL for the limiter. Points at a dead local port by default
/// (fail-open means the flow tests still behave deterministically); when
/// RATEWALL_TEST_REDIS is set, a real Redis is used so the 429 path is
/// exercised against the actual counter. A per-test prefix key namespace
/// keeps runs isolated.
fn dead_or_real_redis() -> String {
    std::env::var("RATEWALL_TEST_REDIS").unwrap_or_else(|_| "redis://127.0.0.1:1".into())
}

/// Unique per-run limiter key namespace so re-execution never inherits a
/// full window from a previous run (and never collides with the live
/// compose gateway's counters, which use the same subject/prefix keys).
fn run_marker() -> String {
    format!(
        "metrics-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn rl_config(url: &str, limit: u32) -> RateLimitConfig {
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

async fn get(app: &axum::Router, path: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::get(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn metrics_scrape_reflects_the_proxy_path() {
    let backend = TestBackend::spawn().await;
    let (app, _metrics) = build_app(
        &backend.addr,
        rl_config(&dead_or_real_redis(), 100),
        &run_marker(),
    )
    .await;
    let token = login(&app).await;

    // 2 OK requests, 1 backend 500, 1 unauthenticated 401 (auth happens
    // before the proxy path, so it does not count as a proxied request).
    for i in 0..2 {
        let (status, _) = get(&app, &format!("/crm/ok-{i}"), Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
    }
    backend.fail.store(true, Ordering::SeqCst);
    let (status, _) = get(&app, "/crm/failing", Some(&token)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = get(&app, "/crm/noauth", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Scrape.
    let (status, body) = get(&app, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("ratewall_requests_total{prefix=\"crm\"} 3\n"),
        "{body}"
    );
    assert!(
        body.contains("ratewall_responses_total{prefix=\"crm\",class=\"2xx\"} 2\n"),
        "{body}"
    );
    assert!(
        body.contains("ratewall_responses_total{prefix=\"crm\",class=\"5xx\"} 1\n"),
        "{body}"
    );
    // Histogram observed all three proxied requests.
    assert!(
        body.contains("ratewall_request_duration_seconds_count{prefix=\"crm\"} 3\n"),
        "{body}"
    );
    assert!(
        body.contains("ratewall_request_duration_seconds_sum"),
        "{body}"
    );
    // Breaker published at scrape time; backend 500s never trip it.
    assert!(
        body.contains("ratewall_breaker_state{prefix=\"crm\"} 0\n"),
        "{body}"
    );
    // The 401 (auth gate, pre-proxy) must not appear in proxy metrics.
    assert!(!body.contains("4xx"), "{body}");
}

#[tokio::test]
async fn rate_limit_rejections_are_counted() {
    let has_redis = std::env::var("RATEWALL_TEST_REDIS")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let backend = TestBackend::spawn().await;
    // Limit 2: the third proxied request must be 429 and counted.
    let (app, _metrics) = build_app(
        &backend.addr,
        rl_config(&dead_or_real_redis(), 2),
        &run_marker(),
    )
    .await;
    let token = login(&app).await;

    for i in 0..2 {
        let (status, _) = get(&app, &format!("/crm/limited-{i}"), Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = get(&app, "/crm/over", Some(&token)).await;
    if has_redis {
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    } else {
        // Fail-open (no Redis in CI): the over-limit request passes
        // uncounted, exactly as ADR-0001 demands, and no rejection shows.
        assert_eq!(status, StatusCode::OK);
    }

    let (_, body) = get(&app, "/metrics", None).await;
    let expected = format!(
        "ratewall_rate_limited_total{{prefix=\"crm\"}} {}",
        if has_redis { 1 } else { 0 }
    );
    assert!(
        body.lines().any(|l| l == expected),
        "expected '{expected}' in scrape:
{body}"
    );
    // The 429 is a gateway rejection before the proxy path, so with Redis
    // it never appears in requests_total (2 proxied attempts, 2 2xx
    // responses). Fail-open: the over-limit request DID proxy, so 3/3.
    let (req_expected, class_expected) = if has_redis { (2, 2) } else { (3, 3) };
    assert!(
        body.contains(&format!(
            "ratewall_requests_total{{prefix=\"crm\"}} {req_expected}
"
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "ratewall_responses_total{{prefix=\"crm\",class=\"2xx\"}} {class_expected}
"
        )),
        "{body}"
    );
}

#[tokio::test]
async fn circuit_open_short_circuits_are_counted_and_gauged() {
    // Dead backend (connection refused): threshold 3 → 502s, then 503s.
    let (app, _metrics) = build_app(
        "http://127.0.0.1:1",
        rl_config(&dead_or_real_redis(), 100),
        &run_marker(),
    )
    .await;
    let token = login(&app).await;

    for _ in 0..3 {
        let (status, _) = get(&app, "/crm/dead", Some(&token)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
    let (status, _) = get(&app, "/crm/dead-again", Some(&token)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (_, body) = get(&app, "/metrics", None).await;
    assert!(
        body.contains("ratewall_circuit_open_total{prefix=\"crm\"} 1\n"),
        "{body}"
    );
    // Scrape publishes the open state (2), set on this very scrape.
    assert!(
        body.contains("ratewall_breaker_state{prefix=\"crm\"} 2\n"),
        "{body}"
    );
    // Transport failures land in the 5xx response class.
    assert!(
        body.contains("ratewall_responses_total{prefix=\"crm\",class=\"5xx\"} 3\n"),
        "{body}"
    );
}

#[tokio::test]
async fn metrics_endpoint_is_unauthenticated() {
    let backend = TestBackend::spawn().await;
    let (app, _metrics) = build_app(
        &backend.addr,
        rl_config(&dead_or_real_redis(), 100),
        &run_marker(),
    )
    .await;
    // No token, wrong method both behave like the other observability routes.
    let (status, _) = get(&app, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&app, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&app, "/readyz", None).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
        "readyz reports truth without a token; got {status}"
    );
}
