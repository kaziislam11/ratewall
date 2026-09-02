# Load-test baseline — `just loadtest`

Every entry below was produced by `scripts/load.sh` against the compose
stack (`gateway + crm + hrm + redis`, all healthy) and is labeled with the
hardware that produced it. The relative deltas are the durable signal; the
absolute milliseconds are not — your laptop is not this laptop.

Reproduce with `just loadtest`, then update this file with your own numbers
and your own environment line. Deltas vs the same hardware's previous row
are meaningful; deltas across rows are not.

## Baseline 1 — 2026-09-02

- **Environment:** author's Windows 11 desktop, Docker Desktop (Docker
  29.7.2), all four containers + the oha generator on one Linux VM,
  nothing isolated — generator and system under test share CPU. oha
  in-container (`ghcr.io/hatoo/oha`), closed-loop, HTTP/1.1, profile
  identical to `scripts/load.sh` defaults.
- **Build:** commit `fd053ae` era (Phases 0–5 + Phase 6 tooling), image
  rebuilt before the run.
- **Profile:** proxied GET through the full chain (Ed25519 verify, Redis
  fixed-window counter, breaker admission, mock-CRM round-trip), 20
  connections / 30s; saturation run 100 connections / 20s; `/healthz`
  15s; direct-to-CRM 15s. Rate limit raised to 1e6 for the run and
  restored (the limiter stayed in the path; under a separate unthrottled
  attempt it 429'd ~284k requests at ~10k rps without dropping anything).

| scenario | avg | p50 | p95 | p99 |
|---|---|---|---|---|
| proxied (auth + ratelimit + breaker), 20 conn | 1.35 ms | 1.22 ms | 2.34 ms | 3.43 ms |
| proxied saturating, 100 conn | 23.08 ms | 20.16 ms | 47.79 ms | 76.43 ms |
| `/healthz` floor | 0.50 ms | 0.33 ms | 1.38 ms | 2.88 ms |
| direct to CRM (no gateway) | 0.41 ms | 0.28 ms | 1.13 ms | 2.09 ms |

**Reading it:** proxy overhead over direct ≈ 0.9 ms at p50 and ≈ 1.3 ms at
p99. At 100 connections the system saturates by queueing — p50 rises ~16×,
but there were zero errors, zero 5xx; the queue drains. `/healthz` vs
direct-to-CRM shows the gateway's own per-request floor (~0.05–0.2 ms) is
noise compared with the proxy round-trip.

Earlier hand-run numbers (same machine, same profile, commit `c01158e`
era) measured p50 1.83 ms / p99 5.45 ms proxied — slower because that
session predated the run-1 freshness of this script's flow and carried a
different mock-backend code path; treat those as superseded.
