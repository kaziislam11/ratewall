//! The per-request audit-log contract (ADR-0001 / build-plan decision 5):
//! every request's log line carries request id, route, status, latency —
//! and, once a bearer token is verified, the authenticated subject.
//!
//! This file must contain exactly one test and must stay a file of its own.
//! Each integration-test file is a separate binary, and `set_default`'s
//! thread-local subscriber is only deterministic when it is the *only*
//! subscriber being installed in the process: parallel test binaries racing
//! `tracing`'s global interest cache make captured events drop at random
//! (observed as flaky, thread-dependent losses when this test shared a
//! file with the other auth-flow tests).
//!
//! Note the subject is asserted on the *span* in the JSON line, which is
//! exactly what a log consumer sees — the same shape the audit ledger
//! will ingest.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn;
use ratewall_core::auth::DEFAULT_TOKEN_TTL;
use ratewall_core::auth_login::{self, LoginState};
use ratewall_core::config::Route;
use ratewall_core::middleware::request_id_and_trace;
use ratewall_core::middleware_auth::AuthState;
use ratewall_core::router::{build_router, ProxyState};
use tower::ServiceExt;

const OWN_ISSUER: &str = "ratewall";

/// An in-memory JSON-lines capture wired in as the subscriber's writer.
/// Synchronized on purpose: events are recorded from several tokio worker
/// threads (the runtime migrates tasks freely), so whole-line writes must
/// serialize via `MakeWriter` rather than `fmt`'s thread-local buffering.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("ratewall=info"))
            .json()
            .with_writer(self.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn lines(&self) -> Vec<String> {
        let text = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_string).collect()
    }
}

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// In-process mock backend standing in for CRM (no fixed ports).
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

/// Mirror of the assembly in gateway/src/main.rs (own-keys mode).
async fn build_app() -> axum::Router {
    let backend_url = spawn_mock_backend().await;
    let routes = vec![Route {
        prefix: "crm".into(),
        backend: backend_url,
    }];
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
    let auth_state = AuthState::new(signing_key.verifying_key(), Some(OWN_ISSUER.into()));
    let login_state = LoginState {
        signing_key: Arc::new(signing_key.clone()),
        issuer: Arc::from(OWN_ISSUER),
        ttl: DEFAULT_TOKEN_TTL,
    };

    let state = ProxyState::new(&routes)
        .expect("proxy state")
        .with_auth(auth_state);
    build_router(state)
        .nest("/auth", auth_login::router(login_state))
        .layer(from_fn(request_id_and_trace))
}

async fn login(app: &axum::Router, body: &str) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(
            Request::post("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn extract_token(response: axum::http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("login JSON");
    json["token"].as_str().expect("token field").to_string()
}

/// One captured JSON log line for a given message string.
fn line_with<'a>(lines: &'a [String], message: &str) -> &'a str {
    lines
        .iter()
        .map(String::as_str)
        .find(|line| line.contains(&format!(r#""message":"{message}""#)))
        .unwrap_or_else(|| panic!("expected a {message:?} line in {lines:?}"))
}

#[tokio::test]
async fn audit_log_lines_carry_the_verified_subject() {
    let capture = LogCapture::default();
    let _guard = capture.subscribe();
    let app = build_app().await;

    // Login: the request line for a successful login names the subject.
    let response = login(&app, r#"{"username":"demo","password":"demo-password"}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    let token = extract_token(response).await;

    // Proxied request with the minted bearer: the line's span carries the
    // verified subject next to request id, route, status and latency.
    let response = app
        .clone()
        .oneshot(
            Request::get("/crm/customers/42")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Rejected request (no token): present in the log with its request id,
    // and — the fail-closed half of the contract — with no subject at all.
    let response = app
        .clone()
        .oneshot(Request::get("/crm/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let lines = capture.lines();

    let proxied = line_with(&lines, "proxied request");
    assert!(
        proxied.contains(r#""subject":"demo""#),
        "span subject missing: {proxied}"
    );
    assert!(
        proxied.contains(r#""request_id":"#),
        "request id missing: {proxied}"
    );
    assert!(
        proxied.contains(r#""status":"200 OK""#),
        "status missing: {proxied}"
    );
    assert!(proxied.contains("latency_ms"), "latency missing: {proxied}");
    assert!(
        proxied.contains(r#""path":"/crm/customers/42""#),
        "route missing: {proxied}"
    );

    let login_line = line_with(&lines, "request complete");
    let login_is_auth_login = login_line.contains(r#""path":"/auth/login""#);
    assert!(
        login_is_auth_login && login_line.contains(r#""subject":"demo""#),
        "login's own request line should carry subject=demo: {login_line}"
    );

    let rejected = lines
        .iter()
        .map(String::as_str)
        .find(|line| line.contains(r#""status":"401 Unauthorized""#))
        .expect("the rejected request should be logged");
    assert!(
        !rejected.contains(r#""subject""#),
        "a 401 must not claim a subject: {rejected}"
    );
}
