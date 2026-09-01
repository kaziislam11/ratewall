//! ratewall-core — the gateway engine.
//!
//! Everything that is not process bootstrap lives here so integration tests
//! can drive the engine directly, without spinning up the binary
//! (see docs/adr/0002-core-lib-gateway-bin-split.md).
//!
//! Phase 1 adds: configuration with startup validation, the prefix route
//! table, and the request-id + structured-tracing middleware. Auth, rate
//! limiting and circuit breakers arrive in phases 2–4.

pub mod config;
pub mod middleware;
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
