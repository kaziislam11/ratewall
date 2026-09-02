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

The demo stack runs with zero external dependencies — no API keys, no sign
ups, no cloud accounts:

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
hanging — it does not retry, and it does not queue. (Circuit breakers that
make this smarter are on the roadmap, not in the code yet.)

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
later — the gateway never calls the ledger directly, it just logs in a
shape that can be piped anywhere.

### Startup config validation

The gateway validates its entire config once, at boot, and exits non-zero
on any problem: missing file, malformed TOML, a key it doesn't recognize, an
empty route table, a prefix containing a slash, a backend that isn't an
absolute http(s) URL. Unknown keys are errors, not warnings — a typo like
`ratelimit` instead of `rate_limit` should not silently disable a feature
you thought you'd configured. The reasoning is written up in
[ADR-0003](docs/adr/0003-config-validated-at-startup.md).

The practical consequence: a healthy gateway really is healthy. If it's
running, every route in its table is well-formed, and any error you see
after boot is a runtime event (backend down, backend slow), not a config
bug.

## Constraints — the deliberate kind

These are design decisions, not missing features. Each is written down as
an [ADR](docs/adr/) because I will forget why.

**Fail-open vs fail-closed, decided up front** ([ADR-0001](docs/adr/0001-fail-open-rate-limiting-fail-closed-auth.md)).
Rate limiting will be fail-open: if Redis is unreachable, enforcement stops
but traffic keeps flowing, because a rate limiter protects against abuse,
not trust, and turning an abuse-protection failure into a full outage is
the wrong trade. Auth will be fail-closed: if JWT verification itself
cannot run, requests are rejected, never silently passed through. Both
rules are fixed now, before either feature exists, so the implementation
can't drift into whichever behavior was easier that week.

**The engine is a library; the binary is a shell** ([ADR-0002](docs/adr/0002-core-lib-gateway-bin-split.md)).
Routing, middleware, config — all of it lives in `ratewall-core`. The
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

- **No authentication.** Anything that reaches a configured route is
  proxied. JWT auth (fail-closed, with the gateway able to issue its own
  demo tokens) is the next milestone.
- **No rate limiting.** Redis is in the compose stack already because the
  limiter will need it, but nothing reads from it yet.
- **No circuit breakers.** A dead backend currently produces a 502 per
  request. Per-backend breakers with half-open probes are planned so one
  slow service can't degrade traffic to the others.
- **No metrics endpoint.** `/healthz` exists; `/readyz` and `/metrics`
  (Prometheus format) don't yet.
- **Not battle-tested as a security boundary.** This is a young project.
  Do not put it on the public internet in front of anything you care about
  and walk away.

## Architecture

```
                 ┌──────────────────────── ratewall ────────────────────────┐
 clients         │  axum router                                             │   backends
 ────────        │   ├─ request-id middleware (UUID, or caller-supplied)    │   ─────────
 browsers ──────▶│   ├─ prefix router (/crm/*, /hrm/*) ─────────────────────┼──▶ CRM  (:3000)
 mobile/API      │   ├─ structured request log (tracing)                    │───▶ HRM  (:3001)
                 │   └─ /healthz                                            │
                 └──────────────────────────────────────────────────────────┘
                                        │
                                     Redis
                          (reserved for rate-limit state — unused yet)
```

One `reqwest::Client` is shared across all proxied requests. Hop-by-hop
headers are stripped before forwarding. The route table is a plain
`BTreeMap` resolved on the request path — there is no route registration
magic; what you see in `config.toml` is the entire routing surface.

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
| redis | 6379 | reserved for the rate limiter |

The mock backends accept every HTTP method and echo the request path, which
is enough to prove the gateway's routing round-trips without needing the
real services. Redis isn't contacted by anything yet.

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
```

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
```

That is the whole schema. Everything else is rejected at startup. In the
compose stack the file is mounted read-only at
`/etc/ratewall/config.toml`; point `RATEWALL_CONFIG` somewhere else (or
remount your own file) to reconfigure without touching the image.

## Development

```bash
just build    # cargo build --workspace
just test     # unit + integration tests (17 across config/router/middleware)
just lint     # rustfmt --check + clippy with -D warnings
just up       # docker compose up --build -d
just logs     # tail gateway logs
just down     # tear the stack down
```

CI runs fmt, clippy, build, and the full test suite on every push. The
tests worth knowing about:

- **Config validation tests** cover each rejection path — malformed TOML,
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
later". Run it before pushing.)

## Project layout

```
gateway/            binary: config loading, logging, server bootstrap (thin on purpose)
core/               library: config, router, middleware — the actual engine
mock-backends/      two tiny axum echo services standing in for CRM/HRM
docs/adr/           architecture decision records — the "why" behind the constraints
config.toml         demo configuration, mounted into the gateway container
```

## Roadmap

In rough order, each a self-contained change:

1. **Auth** — Ed25519 JWT verification, fail-closed; a demo login endpoint
   so the stack is usable with zero setup; config hooks to trust an
   external issuer's public key later.
2. **Rate limiting** — Redis-backed, fail-open, keyed by IP then subject,
   with a mandatory kill-Redis-under-load test.
3. **Circuit breakers** — per backend, not global, so a slow CRM doesn't
   degrade HRM traffic; half-open probes for self-healing; `/readyz`
   reflecting real backend and Redis health.
4. **Metrics** — Prometheus endpoint: request counts, latency histograms
   per route, rate-limit rejections, breaker state.
5. **Testing at the edges** — chaos scripts (kill Redis mid-load, kill a
   backend mid-load) and honest, environment-labeled latency numbers.
6. **Kubernetes** — kind manifests with readiness probes wired to real
   health, plus a scripted kill-a-pod-under-load demo.

The audit-ledger integration is deliberately absent from this list. The
gateway's job is to log in an ingestable shape, not to know the ledger
exists.
