//! Gateway configuration (Phase 1).
//!
//! Design rule from BUILD_PLAN.md: **config is validated at startup — the
//! gateway refuses to start on bad config rather than failing at request
//! time.** Every later phase (auth keys, rate limits, circuit thresholds)
//! extends this struct; validation grows with it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// Top-level gateway configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Listen port for the gateway itself.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Route table: path prefix → backend. Populated from `[routes]` in
    /// config.toml, e.g. `crm = "http://crm:3000"`.
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
    /// Auth configuration from `[auth]`. Defaults to "own keys" mode.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Rate limiting from `[ratelimit]`. Defaults to 100 req/min keyed by
    /// subject (or IP when unauthenticated).
    #[serde(default)]
    pub ratelimit: RateLimitConfig,
    /// Circuit breakers from `[breaker]`. Defaults to 5 consecutive
    /// transport failures opening a backend for a 30s cooldown.
    #[serde(default)]
    pub breaker: BreakerConfig,
}

fn default_port() -> u16 {
    8080
}

fn default_token_ttl_secs() -> u64 {
    900 // 15 minutes
}

fn default_rl_limit() -> u32 {
    100
}

fn default_rl_window_secs() -> u64 {
    60
}

fn default_breaker_failure_threshold() -> u32 {
    5
}

fn default_breaker_cooldown_secs() -> u64 {
    30
}

fn default_breaker_timeout_secs() -> u64 {
    15
}

/// Circuit-breaker configuration (Phase 4).
///
/// One breaker per configured backend, all sharing these thresholds. Only
/// transport failures count toward `failure_threshold`; HTTP error statuses
/// are the backend answering and never trip anything.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BreakerConfig {
    /// Consecutive transport failures that open a backend's breaker.
    #[serde(default = "default_breaker_failure_threshold")]
    pub failure_threshold: u32,
    /// Seconds an open breaker waits before allowing one probe request.
    #[serde(default = "default_breaker_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Per-request timeout applied to backend calls (secs). A timeout is a
    /// transport failure: it counts toward the threshold.
    #[serde(default = "default_breaker_timeout_secs")]
    pub timeout_secs: u64,
}

// Manual impl, same reasoning as `AuthConfig`: a derived Default would
// yield zeros when the whole `[breaker]` section is absent.
impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_breaker_failure_threshold(),
            cooldown_secs: default_breaker_cooldown_secs(),
            timeout_secs: default_breaker_timeout_secs(),
        }
    }
}

/// Auth configuration (Phase 2).
///
/// Two modes, mutually exclusive by validation:
///
/// - **Own keys (default).** No `[auth]` section at all, or an empty one.
///   The gateway generates an Ed25519 keypair in `keys_dir` on first boot
///   and issues tokens at `/auth/login`.
/// - **External issuer.** Set `issuer` + `issuer_public_key_pem`; the
///   gateway then only *verifies* tokens from that issuer and the demo
///   login endpoint is disabled.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Directory holding the signing keypair (own-keys mode). In the demo
    /// compose stack this is a named volume so tokens survive restarts.
    #[serde(default)]
    pub keys_dir: Option<PathBuf>,
    /// Trusted issuer for externally-issued tokens (external-issuer mode).
    #[serde(default)]
    pub issuer: Option<String>,
    /// Path to the trusted issuer's Ed25519 public key in PEM
    /// (external-issuer mode).
    #[serde(default)]
    pub issuer_public_key_pem: Option<PathBuf>,
    /// Lifetime of tokens issued by the demo login endpoint, in seconds.
    #[serde(default = "default_token_ttl_secs")]
    pub token_ttl_secs: u64,
}

// Manual impl: the derived one would give `token_ttl_secs: 0` whenever the
// whole `[auth]` section is absent (serde's section-level `default` skips
// per-field defaults), which would mint instantly-expired tokens.
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            keys_dir: None,
            issuer: None,
            issuer_public_key_pem: None,
            token_ttl_secs: default_token_ttl_secs(),
        }
    }
}

impl AuthConfig {
    /// True when the gateway issues its own tokens (demo login enabled).
    pub fn issues_own_tokens(&self) -> bool {
        self.issuer.is_none() && self.issuer_public_key_pem.is_none()
    }
}

/// Rate-limit configuration (Phase 3).
///
/// Fixed window per key. Keyed by authenticated subject when a token was
/// verified, else by client IP. Enforcement is fail-open: if Redis is
/// unreachable, requests pass uncounted (ADR-0001).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Redis URL for the shared counters.
    pub redis_url: String,
    /// Requests allowed per key per window.
    #[serde(default = "default_rl_limit")]
    pub limit: u32,
    /// Window length in seconds.
    #[serde(default = "default_rl_window_secs")]
    pub window_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            redis_url: default_rl_redis_url(),
            limit: default_rl_limit(),
            window_secs: default_rl_window_secs(),
        }
    }
}

fn default_rl_redis_url() -> String {
    "redis://redis:6379".into()
}

/// A validated route binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Path prefix as written in config (e.g. "crm"). Served at `/{prefix}/*`.
    pub prefix: String,
    /// Backend base URL (e.g. "http://crm:3000").
    pub backend: String,
}

/// Errors that make the config unusable. The gateway exits on any of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    /// A route prefix is not a clean single path segment.
    InvalidPrefix(String),
    /// A backend URL is not an absolute http(s) URL.
    InvalidBackend {
        prefix: String,
        backend: String,
    },
    /// The route table is empty — nothing to proxy.
    NoRoutes,
    /// The `[auth]` section is internally contradictory.
    InvalidAuth(String),
    /// The `[ratelimit]` section has unusable values.
    InvalidRateLimit(String),
    /// The `[breaker]` section has unusable values.
    InvalidBreaker(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(msg) => write!(f, "config parse error: {msg}"),
            ConfigError::InvalidPrefix(p) => {
                write!(f, "invalid route prefix {p:?}: must be a single path segment (alphanumeric, no slashes)")
            }
            ConfigError::InvalidBackend { prefix, backend } => {
                write!(
                    f,
                    "route {prefix:?}: backend {backend:?} is not an absolute http(s) URL"
                )
            }
            ConfigError::NoRoutes => {
                write!(f, "no routes configured: add at least one [routes] entry")
            }
            ConfigError::InvalidAuth(msg) => write!(f, "invalid [auth] config: {msg}"),
            ConfigError::InvalidRateLimit(msg) => write!(f, "invalid [ratelimit] config: {msg}"),
            ConfigError::InvalidBreaker(msg) => write!(f, "invalid [breaker] config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl GatewayConfig {
    /// Parse and validate a config from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: GatewayConfig =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the parsed config. Called from `from_toml` and available for
    /// programmatically-built configs (tests, future CLI overrides).
    pub fn validate(&self) -> Result<Vec<Route>, ConfigError> {
        // Auth: issuer and its public key must be set together (external
        // mode), and `keys_dir` must be present in own-keys mode. In tests
        // and programmatic configs a missing keys_dir falls back to the
        // default at startup, so only reject the impossible combinations
        // here: a half-specified external issuer.
        match (&self.auth.issuer, &self.auth.issuer_public_key_pem) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(ConfigError::InvalidAuth(
                    "issuer and issuer_public_key_pem must be set together".into(),
                ));
            }
            _ => {}
        }
        if !self.auth.issues_own_tokens() && self.auth.keys_dir.is_none() {
            return Err(ConfigError::InvalidAuth(
                "keys_dir is required to hold the verifier's own signing key".into(),
            ));
        }

        if self.routes.is_empty() {
            return Err(ConfigError::NoRoutes);
        }
        if self.ratelimit.limit == 0 {
            return Err(ConfigError::InvalidRateLimit(
                "limit must be at least 1".into(),
            ));
        }
        if self.ratelimit.window_secs == 0 {
            return Err(ConfigError::InvalidRateLimit(
                "window_secs must be at least 1".into(),
            ));
        }
        if self.ratelimit.redis_url.is_empty() {
            return Err(ConfigError::InvalidRateLimit(
                "redis_url must not be empty".into(),
            ));
        }
        if self.breaker.failure_threshold == 0 {
            return Err(ConfigError::InvalidBreaker(
                "failure_threshold must be at least 1".into(),
            ));
        }
        if self.breaker.cooldown_secs == 0 {
            return Err(ConfigError::InvalidBreaker(
                "cooldown_secs must be at least 1".into(),
            ));
        }
        if self.breaker.timeout_secs == 0 {
            return Err(ConfigError::InvalidBreaker(
                "timeout_secs must be at least 1".into(),
            ));
        }
        let mut routes = Vec::with_capacity(self.routes.len());
        for (prefix, backend) in &self.routes {
            if prefix.is_empty()
                || !prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(ConfigError::InvalidPrefix(prefix.clone()));
            }
            let ok = backend.starts_with("http://") || backend.starts_with("https://");
            if !ok || backend.len() <= "http://".len() {
                return Err(ConfigError::InvalidBackend {
                    prefix: prefix.clone(),
                    backend: backend.clone(),
                });
            }
            routes.push(Route {
                prefix: prefix.clone(),
                backend: backend.trim_end_matches('/').to_string(),
            });
        }
        Ok(routes)
    }

    /// Request timeout applied to backend calls, from `[breaker].timeout_secs`.
    /// A timeout is a breaker input: it counts as a transport failure.
    pub fn backend_timeout(&self) -> Duration {
        Duration::from_secs(self.breaker.timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
port = 8080

[routes]
crm = "http://crm:3000"
hrm = "http://hrm:3001"
"#;

    #[test]
    fn valid_config_parses_and_validates() {
        let config = GatewayConfig::from_toml(VALID).expect("valid config");
        assert_eq!(config.port, 8080);
        let routes = config.validate().expect("routes");
        assert_eq!(routes.len(), 2);
        let crm = routes.iter().find(|r| r.prefix == "crm").unwrap();
        assert_eq!(crm.backend, "http://crm:3000");
    }

    #[test]
    fn defaults_apply_when_fields_missing() {
        let config = GatewayConfig::from_toml("[routes]\ncrm = \"http://crm:3000\"\n")
            .expect("minimal config");
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn empty_route_table_is_rejected() {
        let err = GatewayConfig::from_toml("port = 8080\n").unwrap_err();
        assert_eq!(err, ConfigError::NoRoutes);
    }

    #[test]
    fn malformed_toml_is_a_parse_error() {
        let err = GatewayConfig::from_toml("this is not toml <<<").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // Catches config typos at startup instead of silently ignoring them.
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[ratelimit]\nrequests = 10\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn prefix_with_slash_is_rejected() {
        // A quoted TOML key containing a slash parses fine but must fail
        // validation: prefixes are single path segments.
        let err = GatewayConfig::from_toml("[routes]\n\"a/b\" = \"http://x:1\"\n").unwrap_err();
        assert_eq!(err, ConfigError::InvalidPrefix("a/b".to_string()));
    }

    #[test]
    fn non_http_backend_is_rejected() {
        let config = GatewayConfig {
            port: 8080,
            routes: BTreeMap::from([("crm".to_string(), "tcp://crm:3000".to_string())]),
            auth: AuthConfig::default(),
            ratelimit: RateLimitConfig::default(),
            breaker: BreakerConfig::default(),
        };
        assert_eq!(
            config.validate().unwrap_err(),
            ConfigError::InvalidBackend {
                prefix: "crm".to_string(),
                backend: "tcp://crm:3000".to_string()
            }
        );
    }

    #[test]
    fn backend_with_no_host_is_rejected() {
        let config = GatewayConfig {
            port: 8080,
            routes: BTreeMap::from([("crm".to_string(), "http://".to_string())]),
            auth: AuthConfig::default(),
            ratelimit: RateLimitConfig::default(),
            breaker: BreakerConfig::default(),
        };
        assert!(matches!(
            config.validate().unwrap_err(),
            ConfigError::InvalidBackend { .. }
        ));
    }

    #[test]
    fn trailing_slash_in_backend_is_trimmed() {
        let config = GatewayConfig {
            port: 8080,
            routes: BTreeMap::from([("crm".to_string(), "http://crm:3000/".to_string())]),
            auth: AuthConfig::default(),
            ratelimit: RateLimitConfig::default(),
            breaker: BreakerConfig::default(),
        };
        let routes = config.validate().expect("valid");
        assert_eq!(routes[0].backend, "http://crm:3000");
    }

    #[test]
    fn auth_defaults_to_own_keys_mode() {
        let config = GatewayConfig::from_toml(VALID).expect("valid");
        assert!(config.auth.issues_own_tokens());
        assert_eq!(config.auth.token_ttl_secs, 900);
        // Deserialized `auth` defaults carry the 15-minute TTL too.
        let config =
            GatewayConfig::from_toml("[routes]\ncrm = \"http://crm:3000\"\n").expect("minimal");
        assert_eq!(config.auth.token_ttl_secs, 900);
    }

    #[test]
    fn auth_issuer_without_key_is_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[auth]\nissuer = \"https://ext\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAuth(_)));
    }

    #[test]
    fn auth_key_without_issuer_is_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[auth]\nissuer_public_key_pem = \"/keys/ext.pem\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAuth(_)));
    }

    #[test]
    fn auth_external_mode_with_both_fields_is_valid() {
        let config = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[auth]\nissuer = \"https://ext\"\nissuer_public_key_pem = \"/keys/ext.pem\"\nkeys_dir = \"/keys\"\n",
        )
        .expect("valid external issuer config");
        assert!(!config.auth.issues_own_tokens());
    }

    #[test]
    fn auth_unknown_keys_are_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[auth]\nsecret = \"hunter2\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn ratelimit_defaults_apply_when_section_missing() {
        let config =
            GatewayConfig::from_toml("[routes]\ncrm = \"http://crm:3000\"\n").expect("valid");
        assert_eq!(config.ratelimit.limit, 100);
        assert_eq!(config.ratelimit.window_secs, 60);
        assert_eq!(config.ratelimit.redis_url, "redis://redis:6379");
    }

    #[test]
    fn ratelimit_section_overrides_defaults() {
        let config = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[ratelimit]\nredis_url = \"redis://r:1\"\nlimit = 5\nwindow_secs = 30\n",
        )
        .expect("valid");
        assert_eq!(config.ratelimit.limit, 5);
        assert_eq!(config.ratelimit.window_secs, 30);
    }

    #[test]
    fn ratelimit_zero_limit_is_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[ratelimit]\nredis_url = \"redis://r:1\"\nlimit = 0\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRateLimit(_)));
    }

    #[test]
    fn ratelimit_zero_window_is_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[ratelimit]\nredis_url = \"redis://r:1\"\nwindow_secs = 0\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRateLimit(_)));
    }

    #[test]
    fn ratelimit_empty_redis_url_is_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[ratelimit]\nredis_url = \"\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidRateLimit(_)));
    }

    #[test]
    fn ratelimit_unknown_keys_are_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[ratelimit]\nbogus = true\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn breaker_defaults_apply_when_section_missing() {
        let config =
            GatewayConfig::from_toml("[routes]\ncrm = \"http://crm:3000\"\n").expect("valid");
        assert_eq!(config.breaker.failure_threshold, 5);
        assert_eq!(config.breaker.cooldown_secs, 30);
        assert_eq!(config.breaker.timeout_secs, 15);
        assert_eq!(config.backend_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn breaker_section_overrides_defaults() {
        let config = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[breaker]\nfailure_threshold = 2\ncooldown_secs = 60\ntimeout_secs = 3\n",
        )
        .expect("valid");
        assert_eq!(config.breaker.failure_threshold, 2);
        assert_eq!(config.breaker.cooldown_secs, 60);
        assert_eq!(config.backend_timeout(), Duration::from_secs(3));
    }

    #[test]
    fn breaker_zero_values_are_rejected() {
        for (field, toml_line) in [
            ("failure_threshold", "failure_threshold = 0"),
            ("cooldown_secs", "cooldown_secs = 0"),
            ("timeout_secs", "timeout_secs = 0"),
        ] {
            let text = format!("[routes]\ncrm = \"http://crm:3000\"\n\n[breaker]\n{toml_line}\n");
            let err = GatewayConfig::from_toml(&text).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidBreaker(_)),
                "{field} = 0 should be InvalidBreaker, got {err:?}"
            );
        }
    }

    #[test]
    fn breaker_unknown_keys_are_rejected() {
        let err = GatewayConfig::from_toml(
            "[routes]\ncrm = \"http://crm:3000\"\n\n[breaker]\naggressive = true\n",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }
}
