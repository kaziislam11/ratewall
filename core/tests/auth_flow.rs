//! End-to-end integration test for the Phase 2 auth flow, driven through
//! the real router exactly as the gateway binary wires it:
//!
//! login (no token required) → mint JWT → proxy with Bearer → 200,
//! and every failure variant → 401. This pins the flow proven by the live
//! smoke checks as a regression test.
//!
//! The app under test is assembled the same way `gateway/src/main.rs`
//! assembles it: `build_router(state.with_auth(...))` nested with
//! `auth_login::router`, wrapped in `request_id_and_trace`. If main.rs
//! drifts from this assembly, the live checks will catch it — but the
//! *behavior* pinned here (fail-closed gate, login reachable, token
//! accepted) is the contract.

use std::sync::Arc;

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

#[tokio::test]
async fn login_mint_proxy_flow() {
    let app = build_app().await;

    // 1. Proxied route without token → 401 (fail-closed).
    let response = app
        .clone()
        .oneshot(
            Request::get("/crm/customers/42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 2. Login is reachable without a token → 200 + token.
    let response = login(&app, r#"{"username":"demo","password":"demo-password"}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    let token = extract_token(response).await;

    // 3. Same proxied route with the minted Bearer → 200, backend reached.
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
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["service"], "crm");
    assert_eq!(json["path"], "/customers/42");

    // 4. Tampered token (signature flipped) → 401.
    let mut tampered = token.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    let response = app
        .clone()
        .oneshot(
            Request::get("/crm/customers/42")
                .header("authorization", format!("Bearer {tampered}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 5. Wrong password → 401 from login itself.
    let response = login(&app, r#"{"username":"demo","password":"nope"}"#).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
