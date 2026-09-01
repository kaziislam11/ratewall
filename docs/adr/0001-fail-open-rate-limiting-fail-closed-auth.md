# ADR-0001: Fail-open on rate limiting, fail-closed on auth

Date: 2026-08-31 · Status: accepted · Phases: 0 (decision fixed now, implemented in phases 2–3)

## Context

ratewall sits in front of internal services (CRM, HRM, later the audit
ledger's ingestion path). Two protective mechanisms depend on external
state and can fail differently: rate limiting depends on Redis reachability,
JWT verification depends on key/config availability. We need a rule for
what happens when each dependency is unavailable.

## Decision

- **Rate limiting is fail-open.** If Redis is unreachable, limit enforcement
  stops but traffic keeps flowing. A rate limit protects against abuse, not
  trust: blocking all clients because Redis is down is worse than briefly
  skipping enforcement. Failures are logged and counted so the degradation
  is visible.
- **Auth is fail-closed.** If JWT verification itself cannot run (key
  unreadable, crypto error), requests are rejected — never silently passed
  through. Authentication is the trust boundary; verification failure must
  not be indistinguishable from "no token".

## Consequences

- Redis outage degrades only enforcement, never availability. The outage is
  visible in logs and later in `/metrics` (Phase 5).
- Any exception during JWT verification is treated as an authorization
  failure (401), never as an absent token.
- Phase 3 must include a mandatory fail-open test (kill Redis under load →
  traffic keeps flowing); Phase 2 must include a fail-closed test
  (verification error → 401, never a pass-through).
