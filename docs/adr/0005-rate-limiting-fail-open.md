# ADR-0005: Rate limiting is fail-open and keyed on subject, then IP

Date: 2026-09-01 · Status: accepted · Phases: 3+

## Context

The gateway now rate-limits proxied routes: a fixed-window counter in Redis,
keyed on the authenticated subject when a token verified, otherwise on the
client IP (from `ConnectInfo`; `x-forwarded-for` is not trusted because a
gateway that trusts a spoofable header as a limiter key is trivially
bypassed). The limiter runs *after* the auth gate, so unauthenticated
requests are rejected at 401 and never consume rate-limit budget.

## Decision

**Fail-open.** If Redis is unreachable — connection refused, timeout, any
counter-operation error — the request passes and the failure is logged with
the reason. This is the mirror of auth's fail-closed rule (ADR-0001): a
rate limiter protects against abuse, not trust, so degrading its enforcement
during a dependency outage is strictly better than failing every request.
Auth degrades *availability* if it fails; the limiter only degrades
*protection* if it fails, and only while Redis is already broken.

Concretely:

- A Redis error never produces a 5xx from the limiter. The request proceeds
  to the backend; the error is counted and logged at WARN.
- When Redis is reachable and the counter exceeds the limit, the response is
  `429` with a `Retry-After` header (the seconds remaining in the window).
- The fixed window is INCR + EXPIRE on `rl:{sub|ip}:{prefix}:{window}`.
  Counters expire on their own; there is no cleanup job.

## Consequences

- Killing Redis mid-traffic must never drop a request. This is enforced by
  `scripts/chaos-redis.sh` (runnable via `just chaos`), which kills the
  Redis container under load, asserts every request still succeeds, then
  restarts Redis and asserts enforcement resumes — against a gateway that
  is never restarted. The same property is pinned in-process by
  `core/tests/ratelimit_flow.rs` with a dead-Redis URL.
- Fail-open means the limit is unenforced during Redis outages. That is the
  accepted trade; the WARN log lines during an outage are the signal that
  protection is degraded.
- `x-forwarded-for` is ignored by design. When the gateway sits behind
  another proxy that terminates client IPs, this needs a configured
  trusted-proxy hop count — a deliberate future change, not a default.
- `/auth/login` remains unthrottled (see ADR-0004): the limiter covers
  proxied routes, and the login endpoint only becomes a real target when a
  real credential store replaces the demo pair, at which point it should sit
  behind this limiter as part of that change.
