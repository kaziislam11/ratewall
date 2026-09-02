# ADR-0006: Per-backend circuit breakers and a readyz that reports reality

Date: 2026-09-02 · Status: accepted · Phases: 4+

## Context

Before this change, a dead or hanging backend produced one 502 per request —
correct, but every caller paid the full connection timeout, or worse, hung
against a backend that accepts connections and never answers. One slow
service degraded every client touching it, for as long as it stayed broken.
The plan called for per-backend breakers (a slow CRM must not degrade HRM)
and a `/readyz` wired to real health so orchestrator probes mean something.

## Decision

**One breaker per configured backend, sharing one `[breaker]` config:
`failure_threshold` consecutive transport failures open the circuit for
`cooldown_secs`, after which a single probe request decides recovery.**

The state machine is Closed → Open → HalfOpen → (Closed | Open):

- **Open** requests fail fast with `503` and a `Retry-After: 5` header —
  the dead backend is never contacted, and callers pay milliseconds, not
  timeouts.
- **HalfOpen** admits exactly one probe; concurrent requests during the
  probe short-circuit instead of stampeding a struggling backend. A
  successful probe closes the circuit; a failed one re-opens it with a
  fresh cooldown.

**Classification rule — only transport failures count.** Connect refused,
DNS failure, and the per-request timeout (`[breaker].timeout_secs`, applied
to every backend call) all mean "backend is broken or unreachable". An HTTP
status from the backend — including 500 — means the backend *answered*, and
never trips a breaker. Without this rule, a misconfigured route could open
its own breaker and hide a config bug behind "backend down".

**Readiness reports components, not vibes.** `GET /readyz` returns 200 only
when every backend's breaker is closed and Redis answers PING; otherwise
503 with a per-component JSON body (`{"backend:crm":true,"redis":true,...}`).
Open breakers count as not-ready: the backend proved unreachable, and a
pod saying "ready" while its route is 503-ing would make probes decorative
again. `/healthz` stays a dumb 200 — liveness means the process runs;
readiness means the gateway can actually do its job.

## Consequences

- Proven by `scripts/chaos-backend.sh` (runnable via `just chaos-backend`,
  also in CI): kill CRM under load → 502s until the breaker opens → fast
  503s, never a hang → restart CRM → probe closes the circuit → 200s
  resume, all with the gateway never restarted. The full lifecycle is also
  pinned in-process by `core/tests/circuit_flow.rs`, including the
  timeout-trips-breaker rule and "HTTP errors never trip".
- The breaker is *per backend by prefix*, not per backend URL. Two prefixes
  pointing at the same backend get two independent breakers; acceptable for
  a gateway whose route table is a handful of entries, and simpler to reason
  about when reading `readyz` output.
- A closed breaker is only as truthful as the last probe. Between probes,
  recovery behind an open circuit is invisible — the cooldown bounds that
  blind window.
- Timeouts are now a breaker input, so `timeout_secs` trades caller latency
  against how quickly a hanging backend is detected: smaller detects faster
  but risks tripping on legitimately slow (not dead) endpoints.
- `/readyz` performs real Redis and (via breaker state) backend checks on
  every probe — cheap, but it is a live probe, so orchestrator probe
  intervals should account for it (default config: sub-millisecond checks).
