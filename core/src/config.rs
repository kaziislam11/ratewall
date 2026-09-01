//! Gateway configuration (Phase 1).
//!
//! Design rule from BUILD_PLAN.md: **config is validated at startup — the
//! gateway refuses to start on bad config rather than failing at request
//! time.** Every later phase (auth keys, rate limits, circuit thresholds)
//! extends this struct; validation grows with it.

use std::collections::BTreeMap;
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
}

fn default_port() -> u16 {
    8080
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
    InvalidBackend { prefix: String, backend: String },
    /// The route table is empty — nothing to proxy.
    NoRoutes,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(msg) => write!(f, "config parse error: {msg}"),
            ConfigError::InvalidPrefix(p) => {
                write!(f, "invalid route prefix {p:?}: must be a single path segment (alphanumeric, no slashes)")
            }
            ConfigError::InvalidBackend { prefix, backend } => {
                write!(f, "route {prefix:?}: backend {backend:?} is not an absolute http(s) URL")
            }
            ConfigError::NoRoutes => {
                write!(f, "no routes configured: add at least one [routes] entry")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl GatewayConfig {
    /// Parse and validate a config from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: GatewayConfig = toml::from_str(text)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the parsed config. Called from `from_toml` and available for
    /// programmatically-built configs (tests, future CLI overrides).
    pub fn validate(&self) -> Result<Vec<Route>, ConfigError> {
        if self.routes.is_empty() {
            return Err(ConfigError::NoRoutes);
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

    /// Request timeout applied to backend calls. Phase 1 ships a fixed sane
    /// default; making it configurable arrives with circuit breakers (Phase 4),
    /// where a timeout is a breaker input rather than a plain request setting.
    pub fn backend_timeout(&self) -> Duration {
        Duration::from_secs(30)
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
        let err = GatewayConfig::from_toml("[routes]\n\"a/b\" = \"http://x:1\"\n")
            .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_))); // TOML key syntax
        // Programmatic path: prefix containing a slash fails validation.
        let config = GatewayConfig {
            port: 8080,
            routes: BTreeMap::from([("a/b".to_string(), "http://x:1".to_string())]),
        };
        assert_eq!(
            config.validate().unwrap_err(),
            ConfigError::InvalidPrefix("a/b".to_string())
        );
    }

    #[test]
    fn non_http_backend_is_rejected() {
        let config = GatewayConfig {
            port: 8080,
            routes: BTreeMap::from([("crm".to_string(), "tcp://crm:3000".to_string())]),
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
        };
        let routes = config.validate().expect("valid");
        assert_eq!(routes[0].backend, "http://crm:3000");
    }
}
