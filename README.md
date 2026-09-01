# ratewall

A fail-safe reverse proxy / API gateway in Rust, sitting in front of internal
services (Simple RAS CRM + HRM now, an audit ledger's ingestion path later).

Zero third-party secrets required to run the demo — `docker compose up`
works for a stranger out of the box.

## Quickstart

```bash
docker compose up --build -d
curl -i http://localhost:8080/healthz   # → 200
```

That's the whole demo for Phase 0: gateway + two mock backends (CRM/HRM) +
Redis, no manual config.

## Services

| Service  | Port | Notes                                   |
|----------|------|-----------------------------------------|
| gateway  | 8080 | reverse proxy (this repo's main binary) |
| crm      | 3000 | mock echo backend                       |
| hrm      | 3001 | mock echo backend                       |
| redis    | 6379 | rate-limit state (used from Phase 3)    |

## Status

- [x] **Phase 0** — skeleton: workspace, `/healthz`, compose, CI
- [ ] Phase 1 — routing + config + request-id + tracing
- [ ] Phase 2 — auth (Ed25519 JWT, fail-closed)
- [ ] Phase 3 — rate limiting (Redis, fail-open)
- [ ] Phase 4 — circuit breakers + `/readyz`
- [ ] Phase 5 — metrics
- [ ] Phase 6 — layered tests + chaos + load
- [ ] Phase 7 — Kubernetes (kind)

## Development

```bash
just build   # cargo build --workspace
just test    # cargo test --workspace
just lint    # fmt --check + clippy -D warnings
just up      # docker compose up --build -d
just down    # docker compose down
```

See [BUILD_PLAN.md](BUILD_PLAN.md) for the full plan and
[docs/adr/](docs/adr/) for the decisions made so far.
