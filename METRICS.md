# METRICS.md: where every quoted number comes from

Numbers quoted about this project (README, resume, release notes) map to a
script, an environment, and a commit here. If a number isn't in this file,
don't quote it. If it is, the "reproduce" line tells you how to check it
on your own hardware.

## Environment (applies to all latency numbers)

Author's Windows 11 desktop, Docker Desktop 29.7.2 with all four containers
plus the oha load generator on one Linux VM. Nothing was isolated: the
generator competes with the system under test for the same CPU. Treat all
absolute milliseconds as "single laptop, everything co-located"; the
relative deltas are the honest signal.

## Latency and throughput

| Claim | Measured by | Evidence | Status |
|---|---|---|---|
| Proxied GET, full chain: p50 1.83ms, p99 5.45ms, ~315k requests in 30s, zero errors | hand-run oha, closed-loop, 20 conn / 30s, limit raised for the run | README "Numbers" section; era `c01158e` | superseded by the scripted baseline below; conservative to quote |
| Proxied GET, full chain: p50 1.22ms, p99 3.43ms (20 conn); p50 20.16ms, p99 76.43ms at 100 conn with zero errors (queueing, not failure) | `just loadtest` (`scripts/load.sh`), committed profile | `bench/baseline.md` Baseline 1, era `fd053ae` | current; the number the resume bullet should use going forward |
| `/healthz` floor: avg 0.50ms, p99 2.88ms | `just loadtest`, 15s run | Baseline 1 | current |
| Direct-to-CRM (no gateway): avg 0.41ms; proxy overhead ≈ 0.9ms p50 / 1.3ms p99 | `just loadtest`, direct-vs-proxied delta | Baseline 1 | current |
| Rate limiter: 429'd ~284k requests at ~10k rps without dropping anything | unthrottled oha attempt against a limited route | noted in `bench/baseline.md` and README | observed once; treat as anecdote, not a benchmark |

"Full chain" means: Ed25519 JWT verification, Redis fixed-window counter,
circuit-breaker admission, HTTP round-trip to the mock CRM, response copy.

Reproduce: `just loadtest` (raises the demo rate limit for the run and
restores it afterwards). Update `bench/baseline.md` with your own row.

## Reliability

| Claim | Measured by | Evidence | Status |
|---|---|---|---|
| Zero dropped requests while a gateway pod was killed under load | `scripts/k8s-demo.sh`: per-request status capture; any non-429 failure fails the run | session runs: 93/93 and 97/97 ok; CI log line "510 requests, 199 ok, 311 rate-limited (limiter working), 0 dropped" | enforced in CI (`kind-smoke`) on every push |
| Zero dropped requests across a rolling update | `scripts/k8s-rolling-update.sh`: same assertion across a 3-replica rollout | session run: 145 requests, 0 dropped | enforced in CI on every push |
| Traffic survives a full Redis outage (fail-open) | `scripts/chaos-redis.sh`: 15 sequential proxied requests during a live Redis stop must all return 200 | demonstrated live 2026-09-02 (`readyz` flipped `"redis":false` and back; 15/15 passed) | enforced in CI (`compose-smoke`) on every push |
| Breaker opens after 5 transport failures, fails fast (3-4ms 503s), self-heals via half-open probe | `scripts/chaos-backend.sh`: must observe 502 then 503, never a hang, then recovery | demonstrated live; breaker gauge 0→2→0 with `circuit_open_total` +31 | enforced in CI on every push |

Reproduce: `just chaos`, `just chaos-backend`, `just k8s-demo`,
`just k8s-rolling-update`. All four also run in CI on every push.

## Test count

| Claim | Measured by | Status |
|---|---|---|
| 85 unit and integration tests, passing with and without Redis | `just test` / CI `build-test` (no Redis); `just fresh-check` (both environments) | 85 as of 2026-09-02; the count moves with the code, the CI job is the source of truth |

## CI failure history (and one correction)

Raw tally on 2026-09-02: 31 workflow runs, 21 green, 10 red. The reds,
classified:

| Count | Class | Fix that shipped |
|---|---|---|
| 3 | early setup noise (workflows, secrets) | workflow stabilized |
| 2 | tests assumed CI had no Redis | tests skip cleanly without Redis |
| 4 | dirty local kind cluster masking fresh-install bugs (`namespaces "ratewall" not found`) | namespace ordered before Secret in `scripts/k8s-demo.sh` (`d5cd9bf`); `just fresh-check --k8s` gate added (`6bd2f8c`) |
| 1 | script assertion flake: rolling-update baseline ping hit a hot rate-limit window and treated a 429 as a drop | bounded 429-tolerant wait added (`d5fa9a1`) |

**Correction:** an earlier resume draft said environment-related CI
failures went "to zero across 20+ subsequent builds." That is no longer
true as stated: run 33679198606 failed after the fix chain, from the
script-assertion class above (a real flake, root-caused and fixed the same
day). The honest version of the bullet:

> Cut a recurring class of CI failures to zero by root-causing four red
> runs to one pattern (stale local state) and shipping a pre-push gate
> that reruns the full suite in the exact CI image and rebuilds the
> Kubernetes cluster from scratch; when a new failure class appeared, it
> was root-caused and fixed within a day.

The pattern this file exists to enforce: every number is measured, labeled
with its environment, and reproducible by a committed script.
