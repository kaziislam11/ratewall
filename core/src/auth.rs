//! Phase 2 auth: Ed25519-signed JWTs, fail-closed verification.
//!
//! Design rules (BUILD_PLAN.md decision 2, ADR-0001):
//!
//! - **The gateway owns its signing keys by default.** On first boot with no
//!   key material on disk it generates an Ed25519 keypair and persists it to
//!   the keys directory. Restarting reuses the same keys, so issued tokens
//!   survive restarts. Pointing the gateway at an external issuer (Supabase
//!   et al.) later is a config change: provide `issuer_public_key_pem` and
//!   `issuer`, and local token issuing is disabled.
//! - **Auth is fail-closed.** Any error during verification — key unreadable,
//!   malformed token, bad signature, expired, wrong issuer — results in a 401.
//!   A verification error must never be indistinguishable from "no token".
//!
//! Token format: compact JWS, `EdDSA` algorithm (RFC 8037 `OKP`, RFC 7515
//! JWS). Claims are `sub`, `iss`, `iat`, `exp`. Deliberately minimal — this
//! is a gateway demo issuer, not a full identity provider.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::pkcs8::{spki::EncodePublicKey, DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pem_rfc7468::LineEnding;

/// Header value used for locally issued tokens.
pub const TOKEN_ALG: &str = "EdDSA";

/// Default lifetime for tokens issued by the demo login endpoint.
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// Anything that can go wrong with keys or tokens. Every variant maps to a
/// 401 (or a failed startup), never to a pass-through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// PEM could not be parsed as an Ed25519 key.
    BadKeyMaterial(String),
    /// Token is not three dot-separated base64url segments.
    MalformedToken,
    /// Token header is missing, not valid JSON, or not `EdDSA`.
    BadHeader(String),
    /// Token claims are missing, not valid JSON, or semantically wrong.
    BadClaims(String),
    /// Signature did not verify against the trusted key.
    BadSignature,
    /// Token is expired (`exp` in the past). Listed separately because it is
    /// the one rejection clients are expected to see in normal operation.
    Expired,
    /// Token issuer does not match the configured trusted issuer.
    WrongIssuer,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::BadKeyMaterial(msg) => write!(f, "bad key material: {msg}"),
            AuthError::MalformedToken => {
                write!(f, "malformed token: expected header.payload.signature")
            }
            AuthError::BadHeader(msg) => write!(f, "bad token header: {msg}"),
            AuthError::BadClaims(msg) => write!(f, "bad token claims: {msg}"),
            AuthError::BadSignature => write!(f, "signature verification failed"),
            AuthError::Expired => write!(f, "token expired"),
            AuthError::WrongIssuer => write!(f, "token issuer mismatch"),
        }
    }
}

impl std::error::Error for AuthError {}

// ── Key management ───────────────────────────────────────────────────────

/// Filenames inside the keys directory.
pub const PRIVATE_KEY_FILE: &str = "signing_key.pem";
pub const PUBLIC_KEY_FILE: &str = "signing_key.pub.pem";

/// Load the signing key from `keys_dir`, generating and persisting a new
/// keypair on first boot. Returns the signing key and whether it was newly
/// created (useful for the startup log).
pub fn load_or_create_signing_key(keys_dir: &Path) -> Result<(SigningKey, bool), AuthError> {
    let private_path = keys_dir.join(PRIVATE_KEY_FILE);
    match std::fs::read_to_string(&private_path) {
        Ok(pem) => {
            let key = SigningKey::from_pkcs8_pem(&pem).map_err(|e| {
                AuthError::BadKeyMaterial(format!("{}: {e}", private_path.display()))
            })?;
            Ok((key, false))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut csprng = rand_core::OsRng;
            let key = SigningKey::generate(&mut csprng);
            std::fs::create_dir_all(keys_dir).map_err(|e| {
                AuthError::BadKeyMaterial(format!("cannot create {}: {e}", keys_dir.display()))
            })?;
            std::fs::write(
                &private_path,
                key.to_pkcs8_pem(LineEnding::LF)
                    .expect("pkcs8 PEM encoding cannot fail")
                    .as_bytes(),
            )
            .map_err(|e| {
                AuthError::BadKeyMaterial(format!("cannot write {}: {e}", private_path.display()))
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(keys_dir, std::fs::Permissions::from_mode(0o700));
                let _ =
                    std::fs::set_permissions(&private_path, std::fs::Permissions::from_mode(0o600));
            }
            std::fs::write(
                keys_dir.join(PUBLIC_KEY_FILE),
                key.verifying_key()
                    .to_public_key_pem(LineEnding::LF)
                    .expect("SPKI PEM encoding cannot fail")
                    .as_bytes(),
            )
            .map_err(|e| {
                AuthError::BadKeyMaterial(format!(
                    "cannot write {}: {e}",
                    keys_dir.join(PUBLIC_KEY_FILE).display()
                ))
            })?;
            Ok((key, true))
        }
        Err(err) => Err(AuthError::BadKeyMaterial(format!(
            "cannot read {}: {err}",
            private_path.display()
        ))),
    }
}

// ── Token issuing ────────────────────────────────────────────────────────

/// Issue a compact JWS for `subject` signed with `key`.
///
/// Claims: `sub`, `iss`, `iat`, `exp` (`now + ttl`).
pub fn issue_token(
    key: &SigningKey,
    issuer: &str,
    subject: &str,
    now_unix: u64,
    ttl: Duration,
) -> Result<String, AuthError> {
    let header = serde_json::json!({ "alg": TOKEN_ALG, "typ": "JWT" });
    let claims = serde_json::json!({
        "sub": subject,
        "iss": issuer,
        "iat": now_unix,
        "exp": now_unix.saturating_add(ttl.as_secs()),
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header serializes")),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims serialize")),
    );
    let signature = key.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

// ── Token verification (fail-closed) ─────────────────────────────────────

/// Outcome of a successful verification. The subject is what lands in logs
/// and (in later phases) rate-limit keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSubject {
    pub subject: String,
}

/// Verify a compact JWS against `trusted_key`. **Fail-closed:** every failure
/// mode is an `Err`; there is no code path that returns `Ok` on doubt.
pub fn verify_token(
    trusted_key: &VerifyingKey,
    token: &str,
    trusted_issuer: Option<&str>,
    now_unix: u64,
) -> Result<AuthenticatedSubject, AuthError> {
    // 1. Shape: exactly three non-empty base64url segments.
    let mut parts = token.split('.');
    let (Some(header_b64), Some(claims_b64), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AuthError::MalformedToken);
    };
    if header_b64.is_empty() || claims_b64.is_empty() || sig_b64.is_empty() {
        return Err(AuthError::MalformedToken);
    }

    // 2. Header: must declare EdDSA. Anything else is rejected before any
    //    crypto runs — algorithm confusion is a classic JWT break and
    //    fail-closed means refusing it outright.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| AuthError::BadHeader("not base64url".into()))?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).map_err(|e| AuthError::BadHeader(e.to_string()))?;
    if header.get("alg").and_then(|v| v.as_str()) != Some(TOKEN_ALG) {
        return Err(AuthError::BadHeader(format!("expected alg {TOKEN_ALG}")));
    }

    // 3. Signature over the raw signing input.
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| AuthError::BadSignature)?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| AuthError::BadSignature)?;
    let signing_input = format!("{header_b64}.{claims_b64}");
    trusted_key
        .verify(signing_input.as_bytes(), &sig)
        .map_err(|_| AuthError::BadSignature)?;

    // 4. Claims: parse, then check issuer and expiry.
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(claims_b64)
        .map_err(|_| AuthError::BadClaims("not base64url".into()))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&claims_bytes).map_err(|e| AuthError::BadClaims(e.to_string()))?;
    if let Some(expected_issuer) = trusted_issuer {
        if claims.get("iss").and_then(|v| v.as_str()) != Some(expected_issuer) {
            return Err(AuthError::WrongIssuer);
        }
    }
    let exp = claims
        .get("exp")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| AuthError::BadClaims("missing numeric exp".into()))?;
    if now_unix >= exp {
        return Err(AuthError::Expired);
    }
    let subject = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AuthError::BadClaims("missing sub".into()))?
        .to_string();

    Ok(AuthenticatedSubject { subject })
}

/// Current unix time in seconds. Separate helper so tests can pin time.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Extract a bearer token from an `Authorization` header value.
/// Returns `None` for anything that is not `Bearer <token>`.
pub fn bearer_token(authorization_header: Option<&str>) -> Option<&str> {
    let value = authorization_header?;
    let rest = value.strip_prefix("Bearer ")?; // case-sensitive per RFC 6750
    let rest = rest.trim();
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut rand_core::OsRng)
    }

    #[test]
    fn issued_token_verifies_round_trip() {
        let key = fresh_key();
        let token = issue_token(
            &key,
            "ratewall",
            "demo-user",
            1_700_000_000,
            DEFAULT_TOKEN_TTL,
        )
        .expect("issue");
        let verified = verify_token(
            &key.verifying_key(),
            &token,
            Some("ratewall"),
            1_700_000_000,
        )
        .expect("verify");
        assert_eq!(verified.subject, "demo-user");
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let key = fresh_key();
        let token = issue_token(
            &key,
            "ratewall",
            "demo-user",
            1_700_000_000,
            DEFAULT_TOKEN_TTL,
        )
        .expect("issue");
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        // Flip bits inside the claims payload.
        let mut claims = URL_SAFE_NO_PAD.decode(&parts[1]).unwrap();
        for byte in claims.iter_mut() {
            *byte ^= 0x01;
        }
        parts[1] = URL_SAFE_NO_PAD.encode(&claims);
        let tampered = parts.join(".");
        assert_eq!(
            verify_token(&key.verifying_key(), &tampered, None, 1_700_000_000),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn wrong_key_is_rejected() {
        let signer = fresh_key();
        let verifier = fresh_key();
        let token = issue_token(
            &signer,
            "ratewall",
            "demo-user",
            1_700_000_000,
            DEFAULT_TOKEN_TTL,
        )
        .expect("issue");
        assert_eq!(
            verify_token(&verifier.verifying_key(), &token, None, 1_700_000_000),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn expired_token_is_rejected() {
        let key = fresh_key();
        let token =
            issue_token(&key, "ratewall", "demo-user", 1_000, DEFAULT_TOKEN_TTL).expect("issue");
        assert_eq!(
            verify_token(&key.verifying_key(), &token, Some("ratewall"), 1_000 + 900),
            Err(AuthError::Expired)
        );
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let key = fresh_key();
        let token = issue_token(
            &key,
            "not-ratewall",
            "demo-user",
            1_700_000_000,
            DEFAULT_TOKEN_TTL,
        )
        .expect("issue");
        assert_eq!(
            verify_token(
                &key.verifying_key(),
                &token,
                Some("ratewall"),
                1_700_000_000
            ),
            Err(AuthError::WrongIssuer)
        );
    }

    #[test]
    fn alg_none_is_rejected() {
        // The classic JWT algorithm-confusion attack: unsigned token with
        // "alg":"none". Must be rejected before anything else.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(br#"{"sub":"attacker","exp":9999999999}"#);
        let token = format!("{header}.{claims}.");
        let key = fresh_key();
        // Empty signature segment → malformed before header parsing.
        assert!(
            verify_token(&key.verifying_key(), &token, None, 1).is_err(),
            "alg:none token must not verify"
        );
    }

    #[test]
    fn garbage_tokens_are_rejected() {
        let key = fresh_key();
        for bad in ["", "abc", "a.b", "a.b.c.d", "...", "x.y.z"] {
            assert!(
                verify_token(&key.verifying_key(), bad, None, 1).is_err(),
                "token {bad:?} must not verify"
            );
        }
    }

    #[test]
    fn token_missing_sub_is_rejected() {
        let key = fresh_key();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(br#"{"iss":"ratewall","exp":9999999999}"#);
        let signing_input = format!("{header}.{claims}");
        let sig = key.sign(signing_input.as_bytes());
        let token = format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
        assert!(matches!(
            verify_token(&key.verifying_key(), &token, Some("ratewall"), 1),
            Err(AuthError::BadClaims(_))
        ));
    }

    #[test]
    fn load_or_create_is_idempotent_and_persistent() {
        let dir = std::env::temp_dir().join(format!("ratewall-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (key1, created1) = load_or_create_signing_key(&dir).expect("first boot");
        assert!(created1);
        let (key2, created2) = load_or_create_signing_key(&dir).expect("second boot");
        assert!(!created2);
        assert_eq!(key1.to_bytes(), key2.to_bytes());
        // A token issued on "first boot" still verifies on "second boot".
        let token = issue_token(&key1, "ratewall", "u", 1_700_000_000, DEFAULT_TOKEN_TTL).unwrap();
        assert!(verify_token(&key2.verifying_key(), &token, None, 1_700_000_000).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bearer_extraction_is_strict() {
        assert_eq!(bearer_token(Some("Bearer tok123")), Some("tok123"));
        assert_eq!(bearer_token(Some("bearer tok123")), None); // scheme is case-sensitive
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(Some("Bearer a b")), None);
        assert_eq!(bearer_token(Some("Basic abc")), None);
        assert_eq!(bearer_token(None), None);
    }
}
