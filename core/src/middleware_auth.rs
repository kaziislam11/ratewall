//! Phase 2 auth middleware: bearer-JWT gate in front of proxied routes.
//!
//! **Fail-closed by construction** (ADR-0001): the middleware only proceeds
//! on a fully verified token. Missing header, malformed header, verification
//! error of any kind — all produce 401 before the request ever reaches the
//! proxy. There is no "degrade gracefully" path here; the fail-open rule
//! belongs to rate limiting alone.

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::{self, bearer_token};

/// Shared auth state consumed by the middleware.
#[derive(Clone)]
pub struct AuthState {
    inner: std::sync::Arc<AuthInner>,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("trusted_issuer", &self.inner.trusted_issuer)
            .finish_non_exhaustive()
    }
}

struct AuthInner {
    /// Trusted Ed25519 public key for verifying incoming tokens.
    verifying_key: ed25519_dalek::VerifyingKey,
    /// Trusted issuer. Own-keys mode issues with the gateway's own issuer
    /// string and enforces it here; external-issuer mode enforces the
    /// configured one.
    trusted_issuer: Option<String>,
}

impl AuthState {
    /// Build the middleware state from a verifying key.
    pub fn new(verifying_key: ed25519_dalek::VerifyingKey, trusted_issuer: Option<String>) -> Self {
        Self {
            inner: std::sync::Arc::new(AuthInner {
                verifying_key,
                trusted_issuer,
            }),
        }
    }
}

/// Axum middleware fn: verify the bearer JWT or reject with 401.
///
/// Rejection responses intentionally do not echo the failure detail (e.g.
/// "expired" vs "bad signature") to callers; the specifics go to the log
/// where operators can see them. Distinguishing failure modes for
/// unauthenticated clients is free information for attackers.
pub async fn require_bearer_jwt(
    State(auth): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| bearer_token(Some(v)));

    let Some(token) = token else {
        tracing::warn!("request rejected: missing or malformed Authorization header");
        return missing_token_response();
    };

    match auth::verify_token(
        &auth.inner.verifying_key,
        token,
        auth.inner.trusted_issuer.as_deref(),
        auth::unix_now(),
    ) {
        Ok(subject) => {
            tracing::info!(subject = %subject.subject, "request authenticated");
            next.run(request).await
        }
        Err(err) => {
            tracing::warn!(%err, "request rejected by auth");
            unauthorized_response()
        }
    }
}

/// Non-HTTP variant of the gate used by the proxy fallback (Phase 2):
/// returns `Ok(())` when the request carries a verified token, otherwise the
/// 401 response to send. Same fail-closed rules as the middleware form.
pub fn check_bearer(
    auth: &AuthState,
    request: &axum::extract::Request,
) -> Result<(), Box<Response>> {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| bearer_token(Some(v)));
    let Some(token) = token else {
        tracing::warn!("request rejected: missing or malformed Authorization header");
        return Err(Box::new(missing_token_response()));
    };
    match auth::verify_token(
        &auth.inner.verifying_key,
        token,
        auth.inner.trusted_issuer.as_deref(),
        auth::unix_now(),
    ) {
        Ok(subject) => {
            tracing::info!(subject = %subject.subject, "request authenticated");
            Ok(())
        }
        Err(err) => {
            tracing::warn!(%err, "request rejected by auth");
            Err(Box::new(unauthorized_response()))
        }
    }
}

fn missing_token_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("www-authenticate", "Bearer")],
        "missing or malformed bearer token",
    )
        .into_response()
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("www-authenticate", "Bearer")],
        "invalid token",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{issue_token, DEFAULT_TOKEN_TTL};
    use axum::body::Body as AxumBody;
    use axum::http::Request as AxumRequest;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "reached backend"
    }

    fn test_app(key: &ed25519_dalek::SigningKey, issuer: Option<&str>) -> Router {
        let auth = AuthState::new(key.verifying_key(), issuer.map(str::to_string));
        Router::new().route("/protected", get(ok_handler)).layer(
            axum::middleware::from_fn_with_state(auth, require_bearer_jwt),
        )
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let response = test_app(&key, Some("ratewall"))
            .oneshot(
                AxumRequest::get("/protected")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let www = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(www.contains("Bearer"));
    }

    #[tokio::test]
    async fn valid_token_passes_through() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let token = issue_token(
            &key,
            "ratewall",
            "demo-user",
            auth::unix_now(),
            DEFAULT_TOKEN_TTL,
        )
        .unwrap();
        let response = test_app(&key, Some("ratewall"))
            .oneshot(
                AxumRequest::get("/protected")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"reached backend");
    }

    #[tokio::test]
    async fn expired_token_is_401() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        // Issued "long ago" with the default TTL — now well past expiry.
        let token = issue_token(&key, "ratewall", "demo-user", 1_000, DEFAULT_TOKEN_TTL).unwrap();
        let response = test_app(&key, Some("ratewall"))
            .oneshot(
                AxumRequest::get("/protected")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn signed_by_other_key_is_401() {
        let signer = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let verifier = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let token = issue_token(
            &signer,
            "ratewall",
            "demo-user",
            auth::unix_now(),
            DEFAULT_TOKEN_TTL,
        )
        .unwrap();
        let response = test_app(&verifier, Some("ratewall"))
            .oneshot(
                AxumRequest::get("/protected")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_scheme_header_is_401() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let response = test_app(&key, Some("ratewall"))
            .oneshot(
                AxumRequest::get("/protected")
                    .header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
