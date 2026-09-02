# ADR-0007: Kubernetes probes that mean something, and one signing key per cluster

Date: 2026-09-02 · Status: accepted · Phases: 7

## Context

Phase 7 puts the gateway on Kubernetes (kind for the demo): 3 gateway
replicas behind one Service. Two questions had to be answered before any
manifest was written:

1. What do the probes actually check? A liveness probe that just proves
   the process can bind a port — the usual `/healthz` default — tells the
   cluster nothing it didn't know. And wiring liveness to a check that
   *depends on other components* (Redis, backends) actively causes harm:
   a Redis blip would get healthy pods restarted, turning degraded
   enforcement into a rolling restart of the whole tier.
2. Who holds the Ed25519 signing key? The compose demo generates a key on
   first boot into a volume — correct for one process, silently wrong for
   a Deployment: each replica would generate its own key, tokens minted
   by one would fail verification on the others, and auth outcomes would
   depend on which pod answered.

## Decision

- **Liveness = `/healthz` (process can serve). Readiness = `/readyz`
  (components can serve).** `/readyz` returns 200 only while every
  circuit breaker is closed and Redis answers PING; that is exactly what
  decides whether a pod should receive traffic. A sick component pulls a
  pod out of the Service endpoints without restarting it; only process
  death restarts anything. This is the payoff of having built `/readyz`
  to report reality in ADR-0006 — the probe wiring is now a consumer of
  it, not a hardcoded 200.
- **One signing key per cluster, distributed as a Secret.** All replicas
  mount the same `gateway-keys` Secret, so any replica can mint and every
  replica can verify. `load_or_create_signing_key` (first-boot
  generation) remains for the compose/single-process demo; the k8s path
  provisions the key externally. In production this key is operator
  domain — the demo scripts generate it with openssl only because the
  demo has no operator.
- **External-issuer mode is the production answer.** When the gateway is
  configured with an issuer's public key (Supabase et al.), pods hold
  only a public key — nothing to coordinate, nothing to leak, replica
  count irrelevant. The shared-Secret decision is the demo-mode bridge.

## Consequences

- Killing a gateway pod under load drops nothing (survivors absorb it);
  the rolling update reuses the same property, asserted by script.
- A Redis outage marks every gateway pod NotReady (readiness fails), but
  the pods keep running and serving — the moment Redis returns,
  readiness recovers without a restart. Fail-open (ADR-0001) applies to
  traffic; readiness merely advertises the truth to the cluster.
- Scale-out is safe in own-keys mode only because of the shared Secret;
  anything that spreads replicas across clusters splits the trust domain
  and needs the external issuer.
- CI gained a kind job (~10 min): it runs both k8s demos on every push,
  so the manifests and probe wiring are exercised on real infrastructure,
  not left to rot as untested YAML.
