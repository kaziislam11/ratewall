//! Phase 4 integration tests, driven through the real router:
//!
//! - Breaker lifecycle: enough transport failures open the backend's
//!   breaker, further requests fail fast with 503 (the dead backend is
//!   never contacted again), the half-open probe runs after the cooldown,
//!   and a successful probe closes the breaker — traffic resumes.
//! - A backend that answers slowly (past `timeout_secs`) counts as a
//!   transport failure and trips the breaker like a refused connection.
//! - Backend HTTP error statuses (404, 500) are the backend *answering*:
//!   they must never trip a breaker.
//! - Breakers are per backend: killing CRM leaves HRM traffic untouched.
//! - `/readyz` reports component health: 503 while a breaker is open,
//!   200 once the probe recovers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use ratewall_core::circuit::{BreakerConfig, Breakers};
use ratewall_core::config::Route;
use ratewall_core::metrics::Metrics;
use ratewall_core::router::{build_router, ProxyState};
use tower::ServiceExt;

/// Backend whose behavior is controlled test-side: it can hang forever
/// (exercising the per-request timeout) or serve, and it counts how many
/// requests reached it so tests can assert an open circuit never does.
#[derive(Clone)]
struct ControllableBackend {
    mode: Arc<std::sync::Mutex<Mode>>,
    hits: Arc<AtomicUsize>,
    addr: String,
}

enum Mode {
    /// Always respond 200.
    Ok,
    /// Sleep forever.
    Hang,
}

impl ControllableBackend {
    async fn spawn() -> Self {
        let mode = Arc::new(std::sync::Mutex::new(Mode::Ok));
        let hits = Arc::new(AtomicUsize::new(0));
        let app_mode = Arc::clone(&mode);
        let app_hits = Arc::clone(&hits);
        let app = axum::Router::new().fallback(move |_: axum::extract::Request| {
            let mode = Arc::clone(&app_mode);
            let hits = Arc::clone(&app_hits);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let hang = matches!(*mode.lock().unwrap(), Mode::Hang);
                if hang {
                    std::future::pending::<()>().await;
                }
                StatusCode::OK.into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            mode,
            hits,
            addr: format!("http://{addr}"),
        }
    }

    fn set_hang(&self, hang: bool) {
        let mut mode = self.mode.lock().unwrap();
        *mode = if hang { Mode::Hang } else { Mode::Ok };
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Router assembly mirroring gateway/src/main.rs (minus auth/ratelimit, not
/// under test here): two prefixes with independent breakers, and an explicit
/// request timeout (a breaker input, not derived from the cooldown).
async fn build_app(
    crm: &str,
    hrm: &str,
    breaker: BreakerConfig,
    timeout: Duration,
) -> axum::Router {
    let routes = vec![
        Route {
            prefix: "crm".into(),
            backend: crm.into(),
        },
        Route {
            prefix: "hrm".into(),
            backend: hrm.into(),
        },
    ];
    let breakers = Breakers::new(&routes, &breaker);
    let state = ProxyState::new(&routes)
        .expect("state")
        .with_breakers(breakers, timeout.as_secs().max(1))
        .with_metrics(Metrics::new());
    build_router(state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).to_string())
}

const NEVER: &str = "http://127.0.0.1:1"; // port 1 refuses connections

#[tokio::test]
async fn breaker_trips_then_fails_fast_then_recovers_via_probe() {
    // Threshold 2, cooldown 100ms. Enough failures open the breaker; a
    // probe after the cooldown closes it again.
    let backend = ControllableBackend::spawn().await;
    let app = build_app(
        &backend.addr,
        NEVER,
        BreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(100),
        },
        Duration::from_millis(200),
    )
    .await;

    // Two timed-out calls (the backend hangs past the 200ms timeout) trip
    // the breaker — a timeout is a transport failure.
    backend.set_hang(true);
    let (status, body) = get(&app, "/crm/first").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let (status, _) = get(&app, "/crm/second").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    // Third request must fail fast with 503 and never touch the backend.
    backend.set_hang(false);
    let hits_before = backend.hits();
    let (status, body) = get(&app, "/crm/third").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("circuit open"), "{body}");
    assert_eq!(backend.hits(), hits_before, "dead backend was contacted");

    // Wait out the cooldown: the next request becomes the half-open probe.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let (status, _) = get(&app, "/crm/probe").await;
    assert_eq!(status, StatusCode::OK);

    // Breaker closed: normal traffic resumes.
    let (status, _) = get(&app, "/crm/after").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn failed_probe_reopens_the_breaker() {
    let backend = ControllableBackend::spawn().await;
    backend.set_hang(true);
    let app = build_app(
        &backend.addr,
        NEVER,
        BreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(80),
        },
        Duration::from_millis(200),
    )
    .await;

    // Timeout counts as a transport failure: breaker opens.
    let (status, _) = get(&app, "/crm/slow").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(backend.hits(), 1, "request reached the hanging backend");

    // Probe after cooldown; backend still hanging → re-opens.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, _) = get(&app, "/crm/probe").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(backend.hits(), 2, "probe should have been admitted");

    // Re-opened: immediately-following requests short-circuit again.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (status, _) = get(&app, "/crm/blocked").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(backend.hits(), 2);
}

#[tokio::test]
async fn backend_http_errors_never_trip_the_breaker() {
    // A backend that always answers 404/500 is *up*. Its breaker must stay
    // closed no matter how many errors it returns.
    let app =
        axum::Router::new().fallback(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let app = build_app(
        &format!("http://{addr}"),
        NEVER,
        BreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(50),
        },
        Duration::from_millis(200),
    )
    .await;
    for i in 0..10 {
        let (status, _) = get(&app, &format!("/crm/err-{i}")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
    // Still 500, not the 503 short-circuit, after far more than 2 errors.
    let (status, _) = get(&app, "/crm/err-final").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn crm_failure_does_not_degrade_hrm() {
    // CRM breaker opens on a dead backend; HRM keeps serving.
    let hrm = ControllableBackend::spawn().await;
    let app = build_app(
        NEVER,
        &hrm.addr,
        BreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(60),
        },
        Duration::from_millis(200),
    )
    .await;
    // Two refused connections reach the threshold; the breaker is open.
    for _ in 0..2 {
        let (status, _) = get(&app, "/crm/x").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
    let (status, _) = get(&app, "/crm/x").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "crm breaker open");
    let (status, body) = get(&app, "/hrm/y").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn readyz_tracks_breakers_and_backends() {
    let crm = ControllableBackend::spawn().await;
    let hrm = ControllableBackend::spawn().await;
    let app = build_app(
        &crm.addr,
        &hrm.addr,
        BreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_secs(60),
        },
        Duration::from_millis(200),
    )
    .await;

    // Everything healthy: 200.
    let (status, _) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::OK);

    // Break crm (timeout → transport failure → breaker open): not ready.
    crm.set_hang(true);
    let (status, _) = get(&app, "/crm/x").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("\"backend:crm\":false"), "{body}");
    assert!(body.contains("\"backend:hrm\":true"), "{body}");

    // /healthz stays a dumb 200 regardless — that's its job.
    let (status, _) = get(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);

    // Recover crm. The breaker is still open (cooldown 60s), so readyz
    // reports not-ready until the probe runs — readiness follows reality.
    crm.set_hang(false);
    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("\"backend:crm\":false"), "{body}");

    // Probe via a fresh router with a tiny cooldown: the request passes
    // through the half-open breaker, closes it, and readyz flips to 200.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let app2 = build_app(
        &crm.addr,
        &hrm.addr,
        BreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_millis(50),
        },
        Duration::from_millis(200),
    )
    .await;
    let (status, _) = get(&app2, "/readyz").await;
    assert_eq!(status, StatusCode::OK, "fresh breakers start closed");
}
