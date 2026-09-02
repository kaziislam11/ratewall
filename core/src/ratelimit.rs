//! Phase 3 rate limiting: Redis-backed fixed-window counters, fail-open.
//!
//! Design rules (BUILD_PLAN.md decision 1, ADR-0001):
//!
//! - **Fail-open.** If Redis is unreachable or a counter operation errors,
//!   the request passes and the failure is logged and counted. A rate
//!   limiter protects against abuse, not trust; turning its dependency's
//!   outage into a full outage is the wrong trade. (Auth is the opposite:
//!   fail-closed.) The degradation is visible — every fail-open decision
//!   logs a warning, so a dead Redis shows up in monitoring, not silence.
//! - **Keyed by subject, then IP.** Once authenticated, the token's `sub`
//!   is the natural key (an IP behind NAT hides many users); requests that
//!   reach the limiter unauthenticated fall back to client IP.
//! - **Fixed window** (`INCR` + `EXPIRE`) — deliberately simple. It has a
//!   known boundary effect (up to 2× limit across a window edge), which is
//!   acceptable for abuse protection and is called out in ADR-0005.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Request is within the limit; `remaining` counts down to the cap.
    Allow,
    /// Request exceeds the cap for this window; caller sends 429.
    Limit(#[allow(dead_code)] u64 /* retry_after_secs */),
    /// Redis could not be reached (or errored): the request passes
    /// uncounted. This is the fail-open branch and must never become a
    /// rejection.
    Open,
}

/// Redis-backed fixed-window counter.
///
/// One instance is shared by the whole gateway. All operations are
/// fail-open: any Redis error resolves to `Decision::Open`.
pub struct RateLimiter {
    conn: deadpool_redis::Pool,
    /// Requests allowed per key per window.
    limit: u32,
    /// Window length.
    window: Duration,
    /// Optional namespace for the Redis keys (used by tests to isolate
    /// windows across runs; production leaves it empty).
    key_prefix: String,
    /// Count of fail-open decisions, for monitoring.
    fail_open_count: Arc<AtomicU64>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("limit", &self.limit)
            .field("window_secs", &self.window.as_secs())
            .finish_non_exhaustive()
    }
}

impl Clone for RateLimiter {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            limit: self.limit,
            window: self.window,
            key_prefix: self.key_prefix.clone(),
            fail_open_count: Arc::clone(&self.fail_open_count),
        }
    }
}

impl RateLimiter {
    /// Build a limiter from a Redis URL. Returns an error only if the URL is
    /// unparseable — an unreachable server at startup is fine (fail-open);
    /// the pool connects lazily.
    pub fn connect(
        redis_url: &str,
        limit: u32,
        window: Duration,
        key_prefix: Option<&str>,
    ) -> Result<Self, String> {
        let cfg = deadpool_redis::Config::from_url(redis_url);
        let pool = cfg
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .map_err(|e| format!("invalid redis url {redis_url:?}: {e}"))?;
        Ok(Self {
            conn: pool,
            limit,
            window,
            key_prefix: key_prefix.unwrap_or("").to_string(),
            fail_open_count: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Fixed-window counter: INCR the key, set expiry on first hit, compare
    /// against the limit. Any Redis error → `Decision::Open` (fail-open).
    pub async fn decide(&self, key: &str) -> Decision {
        let window_key = format!("rl{}:{key}:{}", self.key_prefix, self.window_key_suffix());
        let mut conn = match self.conn.get().await {
            Ok(conn) => conn,
            Err(err) => return self.fail_open(err),
        };
        match redis::pipe()
            .atomic()
            .cmd("INCR")
            .arg(&window_key)
            .cmd("EXPIRE")
            .arg(&window_key)
            .arg(self.window.as_secs().max(1))
            .ignore()
            .query_async::<_, (i64,)>(&mut conn)
            .await
        {
            Ok((count,)) => {
                if count as u64 > self.limit as u64 {
                    Decision::Limit(self.window.as_secs().max(1))
                } else {
                    Decision::Allow
                }
            }
            Err(err) => self.fail_open(err),
        }
    }

    /// Requests that were passed uncounted because Redis was unavailable.
    pub fn fail_open_total(&self) -> u64 {
        self.fail_open_count.load(Ordering::Relaxed)
    }

    fn window_key_suffix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() / self.window.as_secs().max(1))
            .unwrap_or(0)
    }

    fn fail_open(&self, err: impl std::fmt::Display) -> Decision {
        self.fail_open_count.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(%err, "rate limiter fail-open: redis unavailable, request passed uncounted");
        Decision::Open
    }
}

/// The key a request is limited under: authenticated subject if a token was
/// verified, else the client IP.
pub fn limit_key(subject: Option<&str>, client_ip: &str) -> String {
    match subject {
        Some(subject) => format!("sub:{subject}"),
        None => format!("ip:{client_ip}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_key_prefers_subject_over_ip() {
        assert_eq!(limit_key(Some("demo"), "10.0.0.1"), "sub:demo");
        assert_eq!(limit_key(None, "10.0.0.1"), "ip:10.0.0.1");
    }

    #[test]
    fn connect_rejects_unparseable_url() {
        // Scheme so broken the pool builder refuses — a config problem, not
        // a runtime one, so this is the only error path.
        assert!(RateLimiter::connect(":://nope", 10, Duration::from_secs(60), None).is_err());
    }

    #[tokio::test]
    async fn unreachable_redis_fails_open() {
        // Port 1 is reserved and refuses connections. The limiter must
        // return Open (request passes), never Limit or an error.
        let limiter =
            RateLimiter::connect("redis://127.0.0.1:1", 10, Duration::from_secs(60), None).unwrap();
        for _ in 0..25 {
            assert_eq!(limiter.decide("ip:10.0.0.1").await, Decision::Open);
        }
        assert_eq!(limiter.fail_open_total(), 25);
    }
}
