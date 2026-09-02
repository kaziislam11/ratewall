# Changelog

All notable changes to ratewall are documented here, newest first. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
each version heading matches a git tag. When cutting a release, move the
`[Unreleased]` items under a new version header and tag it.

## [Unreleased]

Nothing yet.

## [1.0.0] - 2026-09-02

Initial release: a fail-safe reverse proxy / API gateway in Rust, sitting
in front of internal services. Every fail-safe claim below is enforced by
CI on every push — the full test suite in two environments (with and
without Redis), a compose smoke test, two chaos tests, and a kind job
that kills a gateway pod under load and rolls an update under load with
zero dropped requests — not asserted in prose.

### Added

- **JWT auth, fail-closed.** The gateway owns an Ed25519 keypair:
  generated on first boot, persisted locally, verified per request.
  Missing, malformed, expired, or tampered tokens are rejected with 401
  before the request reaches any backend; if verification itself cannot
  run, requests are rejected, never silently passed through. Pointing it
  at an external issuer is a config swap, not a code change.
- **Demo login endpoint.** `POST /auth/login` mints a 15-minute token for
  a hardcoded demo user, so the stack is demoable with zero setup and no
  third-party identity provider.
- **Redis-backed rate limiting, fail-open.** Fixed-window counters keyed
  by subject (or IP when unauthenticated). If Redis is unreachable,
  enforcement stops and traffic keeps flowing — the failure is logged,
  not propagated.
- **Per-backend circuit breakers.** Consecutive transport failures open a
  backend's breaker and further requests fail fast with 503 instead of
  hanging; after a cooldown a half-open probe decides whether the circuit
  closes again. One slow backend never degrades the others.
- **Real-health `/readyz`.** Reports actual Redis and per-backend health,
  so a readiness probe (and a human) can trust a 200. Liveness is the
  plain process check on `/healthz`.
- **Prometheus `/metrics`.** Request counts and latency histograms per
  route, rate-limit rejections, and circuit-breaker state, in the text
  exposition format.
- **Request IDs and audit-shaped request logs.** Every request gets an
  `x-request-id` (caller-supplied honored, capped at 128 chars) and
  emits a structured log line with request id, route, status, latency,
  and the verified auth subject — the shape an audit ledger would
  ingest.
- **Proxy routing with startup-validated config.** `/crm/*` and `/hrm/*`
  route to configured backends; the gateway refuses to start on bad
  config rather than failing per-request.
- **Demo stack with nothing to sign up for.** `docker compose up` brings
  up the gateway, mock CRM/HRM backends, and Redis, all healthy on
  first boot.
- **Kubernetes manifests for kind.** Gateway ×3 replicas with readiness
  wired to `/readyz` and liveness to `/healthz`, Redis, mock backends,
  and a NodePort for access from outside the cluster.
- **Chaos tests.** `just chaos` kills Redis mid-load and asserts traffic
  keeps flowing; `just chaos-backend` kills a backend and asserts the
  breaker trips fast and self-heals without a restart.
- **Kubernetes demos.** `just k8s-demo` kills a gateway pod under load
  and asserts the survivors absorb traffic with zero dropped requests;
  `just k8s-rolling-update` rolls a new image under load with the same
  zero-drop assertion.
- **Load-test tooling.** `just loadtest` regenerates latency numbers with
  the producing environment attached; `bench/baseline.md` holds the
  committed baseline.
- **Fresh-environment check.** `just fresh-check` runs the full suite in
  the exact CI image (with and without Redis) and, with `--k8s`, rebuilds
  the kind cluster cold — the two "works on my machine because my machine
  is dirty" failure classes, caught before they reach CI.

[Unreleased]: https://github.com/kaziislam11/ratewall/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/kaziislam11/ratewall/releases/tag/v1.0.0
