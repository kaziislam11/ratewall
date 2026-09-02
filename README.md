# ratewall

A small reverse proxy / API gateway written in Rust. It sits in front of
internal HTTP services and does the boring-but-critical things a gateway
should do: route by path prefix, tag every request with an ID, log what
happened to it, and refuse to start if its configuration is broken.

It exists because I kept needing the same thing in front of internal apps
(CRM and HRM right now, an audit ledger's ingestion path eventually) and
every option was either a full service-mesh migration or a bash script
behind nginx. This is the middle ground: one binary, one config file, and
behavior that is explicit about how it fails.

The demo stack runs with zero external dependencies: no API keys, no sign
ups, no cloud accounts.

```bash
docker compose up --build -d
curl -i http://localhost:8080/healthz    # 200
curl -i http://localhost:8080/crm/customers/42
```

## What it actually does today

This section describes shipped behavior, not intentions. Everything below
is exercised by tests in the repo and verified against the live compose
stack.

### Prefix routing

You give it a table of path prefixes and backend URLs:

```toml
port = 8080

[routes]
crm = "http://crm:3000"
hrm = "http://hrm:3001"
```

Anything under `/crm/...` is proxied to the CRM backend with the prefix
stripped, same for `/hrm/...`. The method, query string, and request body
are preserved; hop-by-hop headers (`Connection`, `Keep-Alive`,
`Transfer-Encoding`, and friends per RFC 7230) are stripped, as they should
never be forwarded by a proxy. A prefix with no route gets a 404 that says
so:

```
$ curl -i http://localhost:8080/nope/x
HTTP/1.1 404 Not Found
no route for prefix "nope"
```

If a backend is unreachable, the gateway returns a fast 502 rather than
hanging. It does not retry, and it does not queue. After enough consecutive
failures (default 5), that backend's **circuit breaker opens** and requests
fail fast with 503, so the dead backend isn't contacted at all. After a
cooldown (default 30s), one probe request decides whether the circuit
closes again. Details and the reasoning live in
[ADR-0006](docs/adr/0006-circuit-breakers-and-readyz.md). Only transport
failures (refused connections, timeouts) trip the breaker; a backend
answering with HTTP errors is a backend that's up.

### Request IDs

Every request gets an `x-request-id`. If the caller supplies one (capped at
128 chars), it is honored; otherwise a UUID v4 is generated. The ID comes
back on the response and appears in every log line for that request, so you
can correlate a user complaint to a specific request across gateway and
backend logs.

### Structured request logging

Each request emits a structured `tracing` event:

```
request{request_id=0b6f... method=GET path=/crm/t}
  → proxied request prefix=crm target=http://crm:3000/t status=200 latency_ms=0
```

Route, status, and latency are in there deliberately. The point is that
this log shape is exactly what the audit-ledger project wants to ingest
later. The gateway never calls the ledger directly, it just logs in a shape
that can be piped anywhere.

### Startup config validation

The gateway validates its entire config once, at boot, and exits non-zero
on any problem: missing file, malformed TOML, a key it doesn't recognize, an
empty route table, a prefix containing a slash, a backend that isn't an
absolute http(s) URL. Unknown keys are errors, not warnings. A typo like
`ratelimit` instead of `rate_limit` should not silently disable a feature
you thought you'd configured. The reasoning is written up in
[ADR-0003](docs/adr/0003-config-validated-at-startup.md).

The practical consequence: a healthy gateway really is healthy. If it's
running, every route in its table is well-formed, and any error you see
after boot is a runtime event (backend down, backend slow), not a config
bug.

## Constraints: the deliberate kind

These are design decisions, not missing features. Each is written down as
an [ADR](docs/adr/) because I will forget why.

**Fail-open vs fail-closed, decided up front** ([ADR-0001](docs/adr/0001-fail-open-rate-limiting-fail-closed-auth.md)).
Rate limiting will be fail-open: if Redis is unreachable, enforcement stops
but traffic keeps flowing, because a rate limiter protects against abuse,
not trust, and turning an abuse-protection failure into a full outage is
the wrong trade. Auth will be fail-closed: if JWT verification itself
cannot run, requests are rejected, never silently passed through. Both
rules are fixed now, before either feature exists, so the implementation
can't drift into whichever behavior was easier that week. The limiter is
implemented exactly that way ([ADR-0005](docs/adr/0005-rate-limiting-fail-open.md)):
fixed-window counters in Redis, `429` + `Retry-After` when the cap is hit,
and a pass-through (logged at WARN) when Redis is down. `just chaos` kills
the Redis container under load and proves traffic keeps flowing; `just
chaos-backend` kills a mock backend and proves the breaker trips, fails
fast, and self-heals. Both run in CI on every push.

**The engine is a library; the binary is a shell** ([ADR-0002](docs/adr/0002-core-lib-gateway-bin-split.md)).
Routing, middleware, config, all of it lives in `ratewall-core`. The
`gateway` binary only loads config, sets up logging, and starts the server.
Integration tests import the library and drive the whole app in-process,
which makes them fast and deterministic. If you find business logic in
`main.rs`, that's a bug in the code review, not in the tests.

**Config lives in one place, read once.** No per-request config reads, no
hot reload. Restart the process to change behavior. For a gateway whose
config is a few dozen lines, reload machinery adds more risk than it
removes.

**No third-party secrets to run the demo.** Everything the compose stack
needs is in the repo. Pointing the gateway at real services is a config
file change, not a code change.

## What it does *not* do yet

Stated plainly so nobody has to reverse-engineer it from the source:

- **Not an identity provider.** The auth described below verifies tokens
  and (in demo mode) issues short-lived ones for a hardcoded user. There is
  no user database, no password reset, no refresh flow.

### Authentication (Ed25519 JWT, fail-closed)

Proxied routes require a valid bearer token. `/healthz` and `/auth/*` are
the only unauthenticated paths.

On first boot the gateway generates an Ed25519 keypair, stores it on the
`gateway-keys` volume (private key mode 0600), and reuses it on every
restart, so issued tokens survive restarts and image rebuilds.

```bash
# 1. Mint a token (demo credentials, documented on purpose).
#    Tokens live 15 minutes. After that, log in again:
TOKEN=$(curl -s -X POST -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' \
  http://localhost:8080/auth/login | sed 's/.*"token":"\([^"]*\)".*/\1/')

# 2. Use it. No token, garbage, tampered, or expired → 401, always:
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/crm/customers/42
# → {"path":"/customers/42","service":"crm"}
```

Tokens are compact JWS with `alg: EdDSA` (RFC 8037), claims `sub`, `iss`,
`iat`, `exp`, and a 15-minute lifetime (configurable via
`[auth] token_ttl_secs`). Verification is **fail-closed**: any error
(malformed header, wrong algorithm, the classic `alg: none` attack, bad
signature, expired, wrong issuer) is a 401.

Every 401 body is deliberately identical (`missing or malformed bearer
token` for a bad/absent header, `invalid token` for everything else); the
gateway never tells an unauthenticated caller *why* it said no. The
specific reason (`signature verification failed`, `token expired`, …) goes
to the gateway log on the same line as the request id, so if your own
token gets rejected, grab the `x-request-id` from the response and check
`docker compose logs gateway`:

```
request{request_id=… method=GET path=/crm/x}: request rejected by auth err=token expired
```

To trust an external identity provider instead (Supabase et al.), set the
`[auth]` section with `issuer` + `issuer_public_key_pem` (an Ed25519 public
key in PEM). The demo login endpoint then disables itself; the gateway
only verifies, it never issues.

```toml
[auth]
issuer = "https://your-idp.example"
issuer_public_key_pem = "/keys/idp-public.pem"
# token_ttl_secs = 900   # lifetime of demo-issued tokens
```

This is a demo issuer, not an identity provider. The demo credential is
published here on purpose: the point is that `docker compose up` works
with zero setup. Don't use the demo credentials anywhere real.

Also worth knowing: `/auth/login` is **unthrottled** today. That's fine
while the credential is the published demo pair, but the moment a real
credential store sits behind it, or an external issuer's endpoints are
reachable through a rate-limited route without the login route being
limited too, it becomes a brute-force target and needs the limiter
wired in front of it. (Proxied routes *are* rate-limited, see
[ADR-0005](docs/adr/0005-rate-limiting-fail-open.md).)

- **Not battle-tested as a security boundary.** This is a young project.
  Do not put it on the public internet in front of anything you care about
  and walk away.

## Architecture

```
                 ┌──────────────────────── ratewall ────────────────────────┐
 clients         │  axum router                                             │   backends
 ────────        │   ├─ request-id middleware (UUID, or caller-supplied)    │   ─────────
 browsers ──────▶│   ├─ prefix router (/crm/*, /hrm/*) ─────────────────────┼──▶ CRM  (:3000)
 mobile/API      │   ├─ per-backend circuit breakers (open → fast 503)      │───▶ HRM  (:3001)
                 │   ├─ structured request log (tracing)                    │
                 │   └─ /healthz · /readyz (real component health)          │
                 └──────────────────────────────────────────────────────────┘
                                        │
                                     Redis
                          (rate-limit counters; PINGed by /readyz)
```

One `reqwest::Client` is shared across all proxied requests, with a
per-request timeout from `[breaker].timeout_secs`. Hop-by-hop headers are
stripped before forwarding. The route table is a plain `BTreeMap` resolved
on the request path; there is no route registration magic. What you see in
`config.toml` is the entire routing surface.

## Running it

### The demo stack

```bash
docker compose up --build -d
```

Four containers come up, all with healthchecks:

| Service | Port | What it is |
|-----------|------|------------|
| gateway | 8080 | this repo's binary |
| crm | 3000 | mock backend echoing `{"service":"crm","path":...}` |
| hrm | 3001 | mock backend echoing `{"service":"hrm","path":...}` |
| redis | 6379 | rate-limit counters (fail-open); PINGed by `/readyz` |

The mock backends accept every HTTP method and echo the request path, which
is enough to prove the gateway's routing round-trips without needing the
real services.

Some things to try:

```bash
# Proxy a GET with a query string
curl 'http://localhost:8080/crm/customers/42?verbose=1'
# → {"path":"/customers/42","service":"crm"}

# POST is forwarded too, including the body
curl -X POST -H 'content-type: application/json' \
     -d '{"name":"Ada"}' http://localhost:8080/crm/customers

# Supply your own request id and watch it come back
curl -i -H 'x-request-id: my-trace-123' http://localhost:8080/hrm/employees

# Follow it in the logs
docker compose logs -f gateway

# Check what the gateway actually thinks of its components
# (breaker state per backend + Redis PING; 503 body names the sick one)
curl -i http://localhost:8080/readyz

# Scrape Prometheus metrics: per-route request counts, latency
# histograms, 429s, circuit-open 503s, live breaker state
curl http://localhost:8080/metrics

# Chaos: kill Redis mid-load. Traffic keeps flowing (fail-open);
# enforcement resumes when it comes back. Gateway never restarts.
just chaos

# Chaos: kill the CRM mock. 502s first, then fast 503s once the breaker
# opens, then self-heals when CRM comes back. Gateway never restarts.
just chaos-backend
```

### On Kubernetes (kind)

The same stack runs on a local kind cluster: 3 gateway replicas behind
one Service, with the probes wired to mean something. Readiness on
`/readyz` (real component health: breakers + Redis), liveness on
`/healthz` (process can serve). All replicas share one signing key
(Secret), so a token minted by any replica verifies on all of them
(ADR-0007).

```bash
# One-shot demo: creates the cluster, builds + loads the images, deploys,
# kills a gateway pod mid-load, and asserts zero failed requests while
# the Deployment replaces it.
just k8s-demo

# Rolling update under load: bumps the gateway image tag, asserts zero
# dropped requests across the rollout.
just k8s-rolling-update

# When you're done:
just k8s-down
```

Requires docker, kind, kubectl, and curl. The gateway is reachable at
`localhost:31080` (a NodePort mapped out of the kind node); the same
`/auth/login`, `/crm/*`, `/readyz` surface applies. Both demos also run
in CI on every push, so the manifests can't rot.

### Building just the binary

If you have a Rust toolchain (1.88+; older versions fail on a transitive
dependency's use of edition 2024):

```bash
cargo build --release -p ratewall-gateway
RATEWALL_CONFIG=./config.toml ./target/release/ratewall
```

The gateway reads its config path from `RATEWALL_CONFIG`
(default `/etc/ratewall/config.toml`) and the log filter from `RUST_LOG`.

## Configuration reference

```toml
# Listen port.
port = 8080

# Path prefix → backend base URL. Prefixes must be a single clean path
# segment (letters/digits, no slashes). Backends must be absolute
# http(s) URLs. The prefix is stripped before forwarding: /crm/customers
# hits the backend as /customers.
[routes]
crm = "http://crm:3000"
hrm = "http://hrm:3001"

# Optional. Omit the whole section to run in own-keys demo mode (the
# gateway generates its signing keypair and serves POST /auth/login).
[auth]
# External-issuer mode: issuer and key must be set together, and doing
# so disables the demo login endpoint:
# issuer = "https://your-idp.example"
# issuer_public_key_pem = "/keys/idp-public.pem"
# Where the gateway stores its own keypair (own-keys mode):
# keys_dir = "/var/lib/ratewall/keys"
# Lifetime of demo-issued tokens, in seconds (default 900 = 15 min):
# token_ttl_secs = 900
```

Everything outside this schema is rejected at startup. In the
compose stack the file is mounted read-only at
`/etc/ratewall/config.toml`; point `RATEWALL_CONFIG` somewhere else (or
remount your own file) to reconfigure without touching the image.

## Development

```bash
just build    # cargo build --workspace
just test     # unit + integration tests
just lint     # rustfmt --check + clippy with -D warnings
just up       # docker compose up --build -d
just logs     # tail gateway logs
just down     # tear the stack down
```

CI runs fmt, clippy, build, and the full test suite on every push. The
tests worth knowing about:

- **Config validation tests** cover each rejection path: malformed TOML,
  unknown keys, slash-containing prefixes, non-http backends, empty tables.
- **Router integration tests** spawn a real mock backend in-process on an
  ephemeral port and drive the full axum stack with `tower::ServiceExt::
  oneshot`: round-tripping path and query, POST forwarding, 404 on unknown
  prefixes, 502 on unreachable backends, request-id propagation.
- **Middleware tests** assert that a caller-supplied request id is
  preserved and that a fresh one is generated otherwise.

If you don't have Rust installed locally, the same checks run in Docker:

```bash
docker run --rm -v "$PWD":/build -w /build rust:1.88-slim \
  sh -c "rustup component add clippy rustfmt >/dev/null \
    && cargo fmt --all -- --check \
    && cargo clippy --workspace --all-targets -- -D warnings \
    && cargo test --workspace"
```

(That command exists because the author's machine had Docker but no Rust
toolchain, and "CI will catch it" turned out to mean "four failed runs
later". Run it before pushing, or let the hook do it: `just
install-hooks` points git at `scripts/hooks`, and every `git push` then
runs fmt + clippy first (skippable once with `git push --no-verify`;
CI remains the real gate for tests).)

For the full pre-push treatment, `just fresh-check` runs the entire test
suite in the exact CI image twice (once with a disposable Redis, once with
none: the two environments the tests pin), and `just fresh-check --k8s`
also deletes and recreates the kind cluster before re-running both k8s
demos. That last one exists because four CI runs failed on "works locally
with a dirty cluster, fails on a fresh one". `RATEWALL_FRESH=1 git push`
runs the default mode from the pre-push hook.

## Project layout

```
gateway/            binary: config loading, logging, server bootstrap (thin on purpose)
core/               library: config, router, middleware. The actual engine
mock-backends/      two tiny axum echo services standing in for CRM/HRM
k8s/                kind manifests: gateway ×3, Redis, mock backends, probes
scripts/            smoke, chaos, load, and kind demo scripts (see justfile)
docs/adr/           architecture decision records: the why behind the constraints
docs/notes/         notes worth keeping: the retrospective and the post-v1 roadmap live here
CHANGELOG.md        notable changes per version, Keep a Changelog format
config.toml         demo configuration, mounted into the gateway container
```

## Roadmap

Shipped:

- **Auth**: Ed25519 JWT verification, fail-closed. A demo login endpoint
  so the stack is usable with zero setup, plus config hooks to trust an
  external issuer's public key later.
- **Rate limiting**: Redis-backed, fail-open, keyed by subject then IP,
  with the mandatory kill-Redis-under-load test (`just chaos`).
- **Circuit breakers**: per backend, not global, so a slow CRM doesn't
  degrade HRM traffic; half-open probes for self-healing; `/readyz`
  reflecting real backend and Redis health. Kill-a-backend chaos
  (`just chaos-backend`) runs in CI on every push.
- **Metrics**: `/metrics` in the Prometheus text format with request
  counts and latency histograms per route, status-class responses,
  rate-limit rejections, and live breaker state. Zero dependencies; the
  registry is hand-rolled because two counters and a histogram don't
  justify a client library.
- **Testing at the edges**: smoke + both chaos scripts asserted on
  every push in CI, not just on demand.
- **Load testing**: honest, environment-labeled p99 latency numbers (see
  the Numbers section).
- **Kubernetes**: kind manifests with the readiness probe wired to real
  health (`/readyz`) and liveness to `/healthz`; 3 replicas sharing one
  signing key; scripted kill-a-pod-under-load and zero-dropped-requests
  rolling-update demos (`just k8s-demo`, `just k8s-rolling-update`),
  asserted on every push in a kind CI job.

The audit-ledger integration is deliberately absent from this list. The
gateway's job is to log in an ingestable shape, not to know the ledger
exists.

## Numbers, with the environment attached

Latency through the full proxied path (Ed25519 JWT verification, Redis
fixed-window counter, breaker admission, HTTP round-trip to the mock CRM,
response copy), measured with
[oha](https://github.com/hatoo/oha) against the compose stack, closed-loop,
HTTP/1.1, 30-second runs, temp limit raised so the rate limiter wasn't the
subject under test (it was exercised separately and is honest: it 429'd
284k unauthorized-ish requests in the first attempt at ~10k rps and kept
serving).

| Scenario | avg | p50 | p95 | p99 | notes |
|---|---|---|---|---|---|
| Proxied GET, auth + rate limit + breaker in path, 20 conn | 2.04 ms | 1.83 ms | 3.78 ms | 5.45 ms | ~315k req in 30s, zero errors |
| Same, 100 connections (saturating) | 25.1 ms | 24.2 ms | 40.5 ms | 50.0 ms | ~80k req in 20s, zero errors (queueing, not failure) |
| `/healthz` (no auth, no Redis, no proxy hop) | 0.51 ms | 0.37 ms | 1.37 ms | 2.29 ms | gateway's own floor |
| Direct to mock CRM, bypassing the gateway | 0.47 ms | 0.32 ms | 1.35 ms | 2.31 ms | what the proxy adds: ~1.5 ms at p50, ~3 ms at p99 |

**Environment:** author's Windows 11 desktop, Docker Desktop with all four
containers on one Linux VM, load generator (`oha` in a container, `--network
host`) on the same machine. Nothing was isolated; the generator competes
with the system under test for the same CPU. Treat these as "single laptop,
everything co-located" numbers: the *relative* costs (proxy overhead vs
direct, limiter vs no limiter) are the honest signal, the absolute
milliseconds are not.

To reproduce: `just loadtest` (or `bash scripts/load.sh`). It raises the
rate limit for the run, restores it afterwards, and prints the table; the
committed baseline lives in
[bench/baseline.md](bench/baseline.md). If your numbers differ, yours are
the ones that matter for your hardware.
