//! ratewall-core — the gateway engine.
//!
//! Everything that is not process bootstrap lives here so integration tests
//! can drive the engine directly, without spinning up the binary
//! (see docs/adr/0002-core-lib-gateway-bin-split.md).
//!
//! Phase 1: configuration with startup validation, the prefix route
//! table, and the request-id + structured-tracing middleware.
//! Phase 2: Ed25519 JWT auth — key management, the demo login issuer, and
//! the fail-closed bearer middleware. Rate limiting and circuit breakers
//! arrive in phases 3–4.

pub mod auth;
pub mod auth_login;
pub mod config;
pub mod middleware;
pub mod middleware_auth;
pub mod router;

/// Crate version, from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_exposed() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
    }
}
