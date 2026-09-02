# Retrospective: building ratewall from its plan

A post-project look at what the build plan got right, where the time
actually went, and what to copy or avoid next time. Facts first:
28 CI runs, 19 green, 9 red, across 7 phases and 7 ADRs.

## What the plan got right

**Phase ordering that actually composed.** Each phase consumed the
previous one's output — breakers needed the per-backend timeouts from
routing, metrics needed the validated route table for label
cardinality, the k8s probes consumed `/readyz`, and the rolling-update
demo consumed all of it. No phase needed to be revisited to make a
later one work.

**The fail-open / fail-closed asymmetry written down first (ADR-0001).**
Every later design argument about degradation referenced it. The
contrast made both halves easier: auth is strict because the limiter
is lenient, and the limiter is lenient because auth is strict.

**"Zero third-party secrets to run the demo."** This constraint forced
the bundled demo login and first-boot keypair generation — which is
precisely why a stranger can `docker compose up` and have a working
gateway. Config-over-secrets was the difference between a demo and a
sandbox for one person's machine.

**Chaos tests named in the plan, run in CI.** Kill Redis mid-load, kill
a backend, kill a pod under load — each found bugs the unit tests did
not: the 429 path never counting metrics, the no-Redis CI environment
proving fail-open as a *feature*, the fresh-cluster namespace bug.

**Scope discipline ("what NOT to build").** gRPC, real cloud deploys,
and a Grafana publishing workflow were all genuinely proposed later —
and each was killed by pointing at the plan's non-goals. The plan did
its job precisely when it was boring to obey.

**The plan predicted its own weakness.** It said Phase 6's layers would
be spread across the other phases — right — and one layer (the
repeatable load test with committed baselines) still nearly slipped
through anyway. The end-of-project completeness audit is what caught
it. Plans that admit where they'll fail are the useful ones.

## What each phase actually cost

The plan's phases look uniform; the effort was not.

| Phase | Planned size | Actual cost | Where it went |
|---|---|---|---|
| 0 skeleton | small | small | tooling noise: Docker, first CI run |
| 1 routing/config | small | small code, one real decision | refuse-to-boot config validation (ADR-0003); axum fallback shape |
| 2 auth | medium | **largest** | not the JWT math — the test environment: tracing capture determinism vs parallel tests, key-load failure paths, ed25519-dalek API friction |
| 3 rate limiting | medium | limiter small | the mandatory kill-Redis test; CI-without-Redis became a first-class environment to pin, not an accident |
| 4 breakers | medium | state machine small | the hanging-backend test fixture; half-open semantics under concurrency |
| 5 metrics | medium | registry right-sized | two red CI runs from assuming CI had Redis; tests now assert both environments' exact shapes |
| 6 testing | spread | mostly already built | the last 20% (scripted load test + baseline) took its own dedicated pass |
| 7 k8s | medium | least code, **most failures** | 4 red runs, all one class: a dirty local cluster masking fresh-environment bugs (namespace-before-Secret) |

## The pattern in the 9 red runs

- Runs 1–3: early setup noise (workspace wiring, one .gitignore fix).
- Runs 19–20: environment assumptions — CI has no Redis; tests assumed it did.
- Runs 24–27: **every one the same bug class** — testing against a local
  cluster with leftover state instead of a fresh one.

After run 3, every red run was some flavor of "works on my machine
because my machine is dirty." The fix was never code and always
process: reproduce in the environment CI actually has.

## What the next project should copy

1. **The ADR habit.** Seven short ADRs, each resolving one argument,
   each written before the code it justifies. They settle disputes,
   onboard readers, and give tests something to assert about intent.
2. **The lib/bin split.** `core/` as a library meant integration tests
   drive the real engine in-process, and the binary stays thin. The
   test suite tests the product, not a mock of it.
3. **Chaos tests as CI gates, not local demos.** Every fail-safe claim
   in the README is enforced on every push. Documentation that runs
   doesn't rot.
4. **Environment-labeled numbers.** `bench/baseline.md` +
   `just loadtest` mean the latency claims are regenerable, not
   folklore. Relative deltas are the durable signal; absolute
   milliseconds are not.
5. **A completeness audit against the plan at the end.** Phase-by-phase
   against actual files, scripts, and CI jobs. It caught the
   almost-skipped load-test layer, the missing audit-log subject, and
   the never-created `docs/notes/`.
6. **Deletion-first passes after each phase.** Each one found real
   residue: dead config knobs, one-use helpers, comments describing
   mechanisms that don't exist. The cost of reading a diff is paid
   forever; the cost of a prune pass is paid once.
7. **The pre-push hook.** After CI went red three times on formatting,
   fmt+clippy moved to the point of failure — the push itself. Fast
   feedback at the right gate beats slow feedback at a better one.

## What to avoid

1. **Debugging against a dirty environment.** The entire Phase 7 red
   streak was local-cluster state masking fresh-install bugs. Delete
   the cluster (or container, or volume) and re-run before believing a
   pass.
2. **Test fixtures that fight the runtime.** The tracing-capture tests
   were flaky until the capture got its own process — isolating the
   test beat hardening the fixture, at a fraction of the complexity.
3. **A "done when" that's prose.** Every phase with a crisp, runnable
   completion condition closed cleanly. The one fuzzy layer (load
   testing) drifted until it was scripted.
4. **Push-to-see-if-CI-is-happy.** Runs 19/20 and 24–27 each burned a
   CI run on an assumption a two-minute local check in a fresh
   environment would have falsified. Local-first verification is the
   cheapest test in the ladder. (This one is now a script: `just
   fresh-check` reproduces both test environments with a disposable
   Redis, and `just fresh-check --k8s` runs the k8s demos on a deleted-
   and-recreated cluster — the exact check that would have caught the
   namespace-before-Secret failure before it cost four red runs.)
