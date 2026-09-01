//! ratewall-core — the gateway engine.
//!
//! Everything that is not process bootstrap lives here so integration tests
//! can drive the engine directly, without spinning up the binary
//! (see docs/adr/0002-core-lib-gateway-bin-split.md).
//!
//! Phase 0 is intentionally a placeholder: routing, auth middleware, rate
//! limiting and circuit breakers arrive in phases 1–4.

/// Crate version, from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_exposed() {
        assert!(!super::VERSION.is_empty());
    }
}
