//! Per-backend circuit breakers (Phase 4, ADR-0001's "one slow service
//! must not degrade the others").
//!
//! State machine, per backend:
//!
//! ```text
//!      failures >= threshold
//!              │
//!  Closed ─────────────────────▶ Open ──cooldown elapsed──▶ HalfOpen
//!     ▲                                                    │
//!     └──────────────────── success ───────────────────────┤
//!                                                          │ failure
//!                                                          ▼
//!                                                        Open
//! ```
//!
//! - **Closed**: requests flow; transport failures count toward the
//!   threshold. A streak of N consecutive failures opens the breaker.
//! - **Open**: requests fail fast with 503 — the backend never sees them,
//!   and the caller never waits on a dead connection. After `cooldown`
//!   without traffic, one request is let through as a probe.
//! - **HalfOpen**: that probe decides everything. Success closes the
//!   breaker; failure re-opens it (with a fresh cooldown). Only one probe
//!   is ever in flight — concurrent requests during HalfOpen fail fast
//!   rather than stampeding a struggling backend.
//!
//! Classification rule: only *transport* failures (connect refused, DNS,
//! timeout — the "backend is broken or unreachable" family) trip the
//! breaker. HTTP error statuses (401, 404, 500 from the backend) are the
//! backend *answering*, and never count.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The three states of the breaker for one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Normal operation; failures are counted.
    Closed,
    /// Backend is presumed down; requests fail fast.
    Open,
    /// Cooldown elapsed; a single probe request is allowed through.
    HalfOpen,
}

/// What a caller must do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Forward to the backend; report the outcome back with `record`.
    Call,
    /// Do not touch the backend; caller should fail fast (503).
    ShortCircuit,
}

/// Thresholds and timing for one breaker.
#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// Consecutive transport failures that open the breaker.
    pub failure_threshold: u32,
    /// How long an open breaker waits before allowing a probe.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// Internal, time-stamped state guarded by one mutex per backend.
///
/// The critical sections are tiny (no I/O under the lock): reading the
/// current phase, and stamping a result. `opened_at` backs both the Open
/// cooldown and the HalfOpen probe reservation (see `try_admit`).
#[derive(Debug)]
struct Inner {
    state: State,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

impl Inner {
    fn fresh() -> Self {
        Self {
            state: State::Closed,
            consecutive_failures: 0,
            opened_at: None,
        }
    }
}

/// Circuit breaker for one backend. Cheap to clone; share one per backend
/// across all request handlers.
#[derive(Clone)]
pub struct CircuitBreaker {
    cfg: BreakerConfig,
    inner: std::sync::Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("CircuitBreaker")
            .field("state", &inner.state)
            .field("consecutive_failures", &inner.consecutive_failures)
            .field("failure_threshold", &self.cfg.failure_threshold)
            .field("cooldown_secs", &self.cfg.cooldown.as_secs())
            .finish()
    }
}

impl CircuitBreaker {
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            cfg,
            inner: std::sync::Arc::new(Mutex::new(Inner::fresh())),
        }
    }

    /// Ask whether this request may reach the backend.
    ///
    /// In HalfOpen exactly one caller gets `Admit::Call` (the probe);
    /// everyone else short-circuits until the probe resolves.
    pub fn try_admit(&self) -> Admit {
        let mut inner = self.lock();
        match inner.state {
            State::Closed => Admit::Call,
            State::Open => {
                let waited = inner
                    .opened_at
                    .map(|t| t.elapsed() >= self.cfg.cooldown)
                    .unwrap_or(false);
                if waited {
                    inner.state = State::HalfOpen;
                    Admit::Call // this caller *is* the probe
                } else {
                    Admit::ShortCircuit
                }
            }
            State::HalfOpen => Admit::ShortCircuit,
        }
    }

    /// Report the outcome of an admitted call. `success` must be true only
    /// for backend responses that prove the backend is serving again.
    pub fn record(&self, success: bool) {
        let mut inner = self.lock();
        match inner.state {
            State::Closed => {
                if success {
                    inner.consecutive_failures = 0;
                } else {
                    inner.consecutive_failures += 1;
                    if inner.consecutive_failures >= self.cfg.failure_threshold {
                        self.open(&mut inner);
                    }
                }
            }
            // This call was the probe: success closes, failure re-opens
            // with a fresh cooldown. The Open arm is unreachable by
            // construction — only an admitted caller (Closed, or the
            // HalfOpen probe) ever records — and is deliberately a no-op
            // rather than a state change, so a stray record can never
            // silence a breaker that is open for a reason.
            State::HalfOpen if success => {
                inner.state = State::Closed;
                inner.consecutive_failures = 0;
                inner.opened_at = None;
            }
            State::HalfOpen => self.open(&mut inner),
            State::Open => {}
        }
    }

    fn open(&self, inner: &mut MutexGuard<'_, Inner>) {
        inner.state = State::Open;
        inner.consecutive_failures = 0;
        inner.opened_at = Some(Instant::now());
    }

    /// Current state, for /metrics and tests.
    pub fn state(&self) -> State {
        self.lock().state
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned mutex means a panic while holding the lock; the
        // breaker's data is always internally consistent (plain ints and
        // enums), so recovering is safe and better than propagating a panic
        // into a request path.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One breaker per configured backend, keyed by route prefix.
#[derive(Debug, Clone, Default)]
pub struct Breakers {
    per_backend: BTreeMap<String, CircuitBreaker>,
}

impl Breakers {
    /// Build a breaker per route. All share the same thresholds.
    pub fn new(routes: &[crate::config::Route], cfg: &BreakerConfig) -> Self {
        let per_backend = routes
            .iter()
            .map(|route| (route.prefix.clone(), CircuitBreaker::new(cfg.clone())))
            .collect();
        Self { per_backend }
    }

    pub fn get(&self, prefix: &str) -> Option<&CircuitBreaker> {
        self.per_backend.get(prefix)
    }

    /// Snapshot of every breaker's state, for /metrics.
    pub fn states(&self) -> BTreeMap<String, State> {
        self.per_backend
            .iter()
            .map(|(prefix, breaker)| (prefix.clone(), breaker.state()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker(threshold: u32, cooldown_ms: u64) -> CircuitBreaker {
        CircuitBreaker::new(BreakerConfig {
            failure_threshold: threshold,
            cooldown: Duration::from_millis(cooldown_ms),
        })
    }

    #[test]
    fn closed_breaker_admits_everything() {
        let b = breaker(3, 10_000);
        for _ in 0..10 {
            assert_eq!(b.try_admit(), Admit::Call);
        }
    }

    #[test]
    fn failures_below_threshold_stay_closed() {
        let b = breaker(3, 10_000);
        for _ in 0..2 {
            assert_eq!(b.try_admit(), Admit::Call);
            b.record(false);
        }
        assert_eq!(b.state(), State::Closed);
    }

    #[test]
    fn threshold_failures_open_the_breaker() {
        let b = breaker(3, 10_000);
        for _ in 0..3 {
            b.record(false);
        }
        assert_eq!(b.state(), State::Open);
        assert_eq!(b.try_admit(), Admit::ShortCircuit);
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let b = breaker(3, 10_000);
        b.record(false);
        b.record(false);
        b.record(true); // streak broken
        b.record(false);
        b.record(false);
        assert_eq!(b.state(), State::Closed, "2 < 3 after reset");
    }

    #[test]
    fn open_becomes_halfopen_after_cooldown_and_probes() {
        let b = breaker(1, 20);
        b.record(false); // open
        assert_eq!(b.try_admit(), Admit::ShortCircuit);
        std::thread::sleep(Duration::from_millis(30));
        // Cooldown elapsed: first caller becomes the probe.
        assert_eq!(b.try_admit(), Admit::Call);
        assert_eq!(b.state(), State::HalfOpen);
        // Concurrent callers during the probe fail fast.
        assert_eq!(b.try_admit(), Admit::ShortCircuit);
    }

    #[test]
    fn successful_probe_closes_the_breaker() {
        let b = breaker(1, 10);
        b.record(false);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(b.try_admit(), Admit::Call); // probe
        b.record(true);
        assert_eq!(b.state(), State::Closed);
        assert_eq!(b.try_admit(), Admit::Call);
    }

    #[test]
    fn failed_probe_reopens_with_a_fresh_cooldown() {
        let b = breaker(1, 10_000);
        b.record(false); // open
                         // Force the cooldown to look elapsed, take the probe, fail it.
        b.inner.lock().unwrap().opened_at = Some(Instant::now() - Duration::from_secs(11));
        assert_eq!(b.try_admit(), Admit::Call); // probe admitted
        b.record(false);
        assert_eq!(b.state(), State::Open);
        // Fresh cooldown: no new probe immediately.
        assert_eq!(b.try_admit(), Admit::ShortCircuit);
    }

    #[test]
    fn per_prefix_breakers_are_independent() {
        let routes = vec![
            crate::config::Route {
                prefix: "crm".into(),
                backend: "http://crm:3000".into(),
            },
            crate::config::Route {
                prefix: "hrm".into(),
                backend: "http://hrm:3001".into(),
            },
        ];
        let breakers = Breakers::new(&routes, &BreakerConfig::default());
        let crm = breakers.get("crm").unwrap();
        for _ in 0..5 {
            crm.record(false);
        }
        assert_eq!(breakers.get("crm").unwrap().state(), State::Open);
        assert_eq!(breakers.get("hrm").unwrap().state(), State::Closed);
        assert!(breakers.get("nope").is_none());
    }
}
