//! Phase 2 auth gate: bearer-JWT verification in front of proxied routes.
//!
//! **Fail-closed by construction** (ADR-0001): the gate only proceeds on a
//! fully verified token. Missing header, malformed header, verification
//! error of any kind — all produce 401 before the request ever reaches the
//! proxy. There is no "degrade gracefully" path here; the fail-open rule
//! belongs to rate limiting alone.
//!
//! The gate is invoked from the proxy fallback (`ProxyState::with_auth`)
//! rather than as an axum layer, because a layer applied to the router
//! would also gate `/healthz` (used by orchestrator probes) and `/auth`
//! (where clients obtain tokens). This shape keeps the unauthenticated
//! surface explicit and minimal.
//!
//! Rejection responses intentionally do not echo the failure detail (e.g.
//! "expired" vs "bad signature") to callers; the specifics go to the log
//! where operators can see them. Distinguishing failure modes for
//! unauthenticated clients is free information for attackers.

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth::{self, bearer_token};

/// Shared auth state consumed by the gate.
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
    /// Build the gate state from a verifying key.
    pub fn new(verifying_key: ed25519_dalek::VerifyingKey, trusted_issuer: Option<String>) -> Self {
        Self {
            inner: std::sync::Arc::new(AuthInner {
                verifying_key,
                trusted_issuer,
            }),
        }
    }
}

/// Verify the bearer JWT on `request`. Returns `Ok(())` when the request
/// carries a verified token, otherwise the 401 response to send. **Fail-
/// closed:** every failure mode — absent header, bad scheme, bad signature,
/// expiry, wrong issuer — is an `Err`. (`Box` keeps the `Result` small;
/// the error path is cold and `Response` is fat.)
pub fn check_bearer(auth: &AuthState, request: &Request) -> Result<(), Box<Response>> {
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
            tracing::info!(subject, "request authenticated");
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

    /// Drive `check_bearer` directly — it is the production gate.
    fn verify(request: &AxumRequest<AxumBody>, auth: &AuthState) -> Result<(), Box<Response>> {
        check_bearer(auth, request)
    }

    fn auth_state(key: &ed25519_dalek::SigningKey, issuer: Option<&str>) -> AuthState {
        AuthState::new(key.verifying_key(), issuer.map(str::to_string))
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let auth = auth_state(&key, Some("ratewall"));
        let request = AxumRequest::get("/protected")
            .body(AxumBody::empty())
            .unwrap();
        let err = verify(&request, &auth).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert!(err
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .contains("Bearer"));
    }

    #[tokio::test]
    async fn valid_token_passes_through() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let auth = auth_state(&key, Some("ratewall"));
        let token = issue_token(
            &key,
            "ratewall",
            "demo-user",
            auth::unix_now(),
            DEFAULT_TOKEN_TTL,
        )
        .unwrap();
        let request = AxumRequest::get("/protected")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(AxumBody::empty())
            .unwrap();
        assert!(verify(&request, &auth).is_ok());
    }

    #[tokio::test]
    async fn expired_token_is_401() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let auth = auth_state(&key, Some("ratewall"));
        // Issued "long ago" with the default TTL — now well past expiry.
        let token = issue_token(&key, "ratewall", "demo-user", 1_000, DEFAULT_TOKEN_TTL).unwrap();
        let request = AxumRequest::get("/protected")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(AxumBody::empty())
            .unwrap();
        assert!(verify(&request, &auth).is_err());
    }

    #[tokio::test]
    async fn signed_by_other_key_is_401() {
        let signer = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let verifier = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let auth = auth_state(&verifier, Some("ratewall"));
        let token = issue_token(
            &signer,
            "ratewall",
            "demo-user",
            auth::unix_now(),
            DEFAULT_TOKEN_TTL,
        )
        .unwrap();
        let request = AxumRequest::get("/protected")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(AxumBody::empty())
            .unwrap();
        assert!(verify(&request, &auth).is_err());
    }

    #[tokio::test]
    async fn wrong_scheme_header_is_401() {
        let key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let auth = auth_state(&key, Some("ratewall"));
        let request = AxumRequest::get("/protected")
            .header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
            .body(AxumBody::empty())
            .unwrap();
        assert!(verify(&request, &auth).is_err());
    }
}
