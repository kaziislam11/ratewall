//! Demo login endpoint: `POST /auth/login` issues a short-lived JWT.
//!
//! This exists so the stack is demoable with zero setup (BUILD_PLAN.md
//! decision 2): a stranger runs `docker compose up`, logs in with the
//! documented demo credentials, and gets a real token. It is disabled
//! entirely when the gateway is configured with an external issuer.
//!
//! Scope discipline: a hardcoded demo user, one credential, short-lived
//! tokens. This is not an identity provider; the real deployment swaps in
//! an external issuer via `[auth]` config.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::json;

/// The one demo credential. Documented in the README; exists so the demo
/// needs no setup, not to protect anything real.
pub const DEMO_USERNAME: &str = "demo";
pub const DEMO_PASSWORD: &str = "demo-password";

/// State for the login router.
#[derive(Clone)]
pub struct LoginState {
    pub signing_key: Arc<SigningKey>,
    pub issuer: Arc<str>,
    pub ttl: std::time::Duration,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// Router handling `POST /login` (mounted at `/auth` by the binary).
pub fn router(state: LoginState) -> Router {
    Router::new().route("/login", post(login)).with_state(state)
}

async fn login(State(state): State<LoginState>, Json(payload): Json<LoginRequest>) -> Response {
    // Constant-time-ish comparison is unnecessary for a demo credential
    // that is published in the README, but the shape of this handler is
    // what a real credential check would look like.
    if payload.username != DEMO_USERNAME || payload.password != DEMO_PASSWORD {
        tracing::warn!(username = %payload.username, "login failed: unknown credentials");
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }

    let now = crate::auth::unix_now();
    match crate::auth::issue_token(
        &state.signing_key,
        &state.issuer,
        &payload.username,
        now,
        state.ttl,
    ) {
        Ok(token) => {
            tracing::info!(subject = %payload.username, "token issued");
            (
                StatusCode::OK,
                Json(json!({
                    "token": token,
                    "token_type": "Bearer",
                    "expires_in": state.ttl.as_secs(),
                })),
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(%err, "failed to issue token");
            (StatusCode::INTERNAL_SERVER_ERROR, "token issuance failed").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{verify_token, DEFAULT_TOKEN_TTL};
    use axum::body::Body;
    use tower::ServiceExt;

    fn test_state() -> LoginState {
        LoginState {
            signing_key: Arc::new(SigningKey::generate(&mut rand_core::OsRng)),
            issuer: Arc::from("ratewall"),
            ttl: DEFAULT_TOKEN_TTL,
        }
    }

    #[tokio::test]
    async fn valid_credentials_yield_verifiable_token() {
        let state = test_state();
        let key_bytes = state.signing_key.verifying_key().to_bytes();
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::post("/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"username":"demo","password":"demo-password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let token = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        // The issued token must verify against the same key the middleware
        // would use, with the gateway's issuer.
        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes).unwrap();
        let subject = verify_token(
            &verifying_key,
            &token,
            Some("ratewall"),
            crate::auth::unix_now(),
        )
        .expect("issued token must verify");
        assert_eq!(subject.subject, "demo");
        let _ = state; // keep alive for Arc comparison clarity
    }

    #[tokio::test]
    async fn wrong_password_is_401() {
        let response = router(test_state())
            .oneshot(
                axum::http::Request::post("/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"demo","password":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_body_is_rejected() {
        let response = router(test_state())
            .oneshot(
                axum::http::Request::post("/login")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum's Json extractor rejects malformed JSON with 400.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
