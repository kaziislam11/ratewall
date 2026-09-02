# Roadmap: what comes after v1.0.0

Every item here exists because we watched it break, or watched its absence
hurt, during the v1.0.0 work. Sources are the live chaos demos against the
compose stack, the CI run history, and
[the retrospective](retrospective.md). The "done when" lines follow the
project's core rule: a claim is not a claim until CI or a script enforces
it.

## Tier 1: v1.1, close what we watched break

Small, observed, shippable in a day or two. These keep the "every claim is
enforced" promise honest.

### 1. `/readyz` must fail fast during an outage

**Evidence:** during the Redis chaos demo, `/readyz` reported
`"redis":false` exactly once and then went slow: about 68 seconds where
polls with a 2s client timeout got no answer at all. The backend chaos
demo showed the same shape, lagging about 13s behind the actual kill.
Meanwhile proxied traffic stayed fast through both outages. The readiness
check appears to wait out a full dial timeout instead of failing fast. In
Kubernetes this turns a healthy-enough pod into one that flaps readiness
slowly while it could serve fine.

**Done when:** a chaos test kills Redis and every `/readyz` response
during the outage window completes in under 1 second while saying
`"redis":false`. Enforced by an assertion in the chaos script (it already
polls `/readyz` during the outage window; the timing and the body just
have to be checked, not narrated).

### 2. Fail-open needs a metric, not just a WARN line

**Evidence:** when Redis died in the demo, the only trace of degradation
was a WARN log line that `docker logs` happened to be difficult to fetch
on the demo machine. `/metrics` showed nothing: no counter moved, so a
dashboard could not distinguish "limiter enforcing" from "limiter blind."

**Done when:** `ratewall_limiter_failopen_total` exists in `/metrics`
output, increments while Redis is down, and a unit or integration test
asserts the counter moves. The chaos script additionally asserts it
climbed after the outage window.

### 3. Breaker transitions must be observable

**Evidence:** the backend chaos demo watched `breaker_state` jump from 0
(closed) straight to 2 (open) and back. The half-open state (1) was never
caught by 1-second polling, so nothing records when or why the breaker
decided anything. After the fact you see scars (`circuit_open_total`)
but not transitions.

**Done when:** each open, half-open, and close transition emits one log
line with the route prefix and the reason, and an integration test
captures the log for at least one full trip (fail five times, see "open",
recover, see "close"). A `ratewall_breaker_transitions_total` counter is
acceptable in place of log capture if it keeps the test deterministic.

### 4. Replace the fixed window with a token bucket (or sliding window)

**Evidence:** CI run 33679198606 failed because the rolling-update
script's baseline ping landed right after the demo's load loop had
exhausted the shared subject's 100-req/60s fixed window. The script was
patched to tolerate a hot window, but the underlying race is real:
fixed windows are full at boundaries, and anything sharing a subject
across phases can eat a 429 at the worst moment.

**Done when:** the limiter uses a token bucket (or sliding window), the
CI flake scenario is covered by a test (saturate a window, then assert a
request just past the boundary is allowed), and `just chaos` plus the
compose smoke still pass with the new limiter semantics.

## Tier 2: v2, make it production-real for real backends

The point of v2 is that the actual deployment target is a real CRM/HRM
behind the gateway.

### 5. JWKS support for external issuers

**Rationale:** external-issuer mode today wants a PEM file exported by
hand. Real issuers (Supabase, Keycloak, Auth0) publish keys at a JWKS
URL and rotate them. Without JWKS, "point it at your identity provider"
is a manual ceremony and rotations break the gateway.

**Done when:** the gateway accepts `issuer_jwks_url`, fetches and caches
keys, verifies a token signed by a key that was rotated after boot, and
an integration test drives that rotation without a restart.

### 6. Trusted identity headers for backends

**Rationale:** backends today must parse the JWT themselves to know who
is calling. The standard gateway move is to inject `x-auth-subject`
after verification so plain-HTTP backends need no JWT library at all.
This is the feature that makes hooking up a real CRM trivial.

**Done when:** the gateway injects the verified subject header, strips
any caller-supplied copy of the same header (spoofing is not a feature),
and an integration test proves both: spoofed inbound header dropped,
verified subject injected on the proxied request.

### 7. Throttle `/auth/login`

**Rationale:** documented wart in the README. Unthrottled is fine while
the credential is the published demo pair; the moment real credentials
sit behind it, it is a brute-force target.

**Done when:** the login route shares the limiter (configurable lower
limit), and a test hammers the endpoint past its limit and sees 429
while a normal login still succeeds.

### 8. Verify or support WebSocket and SSE upgrades

**Rationale:** unverified. The proxy forwards HTTP/1.1 request/response
pairs; a CRM with live updates may need upgrades to survive the hop.
Better to know before the first real backend needs it.

**Done when:** a test with an upgrade-capable mock backend documents
either "works" (assert a round-trip over the upgrade) or "explicitly
rejected with a clear 4xx and a README note," with no third state.

### 9. Multiple upstreams per route

**Rationale:** `crm = ["http://crm1:3000", "http://crm2:3000"]` with
round-robin and a breaker per upstream turns the breaker from a courtesy
into actual capacity protection.

**Done when:** a route can list two backends, requests alternate, one
upstream's breaker opening does not affect the other, and the config
validation tests cover the new shape.

## Tier 3: ecosystem, cheap and high leverage

### 10. Publish images to ghcr.io on tag push

**Done when:** pushing a `v*` tag builds both images, pushes them to
ghcr.io, and a CI job pulls and boots the compose stack from the
published images alone.

### 11. Dependency and fuzz gates

**Done when:** `cargo-deny` (or `cargo-audit`) runs in CI on every push
with a maintained advisory database, and the JWT parser has a fuzz
target that has survived at least one sustained fuzzing session in CI
or nightly.

### 12. Self-publishing releases

**Done when:** pushing a `v*` tag creates a GitHub Release whose notes
come from the tag message, with the changelog updated in the same push
(the v1.0.0 tag was moved by hand for exactly this; see CHANGELOG.md).

## Non-goals (still non-goals)

These stay out, and the reasons are load-bearing decisions, not moods:

- **No hot reload.** ADR-0003 and the README say config is read once at
  boot. For a few dozen lines of config, reload machinery adds more risk
  than it removes. Still true in v2.
- **No gRPC.** Nothing here needs it (retrospective: it was proposed and
  declined once already).
- **No audit ledger inside the gateway.** The gateway logs in an
  ingestable shape; the ledger is a separate project that reads that
  shape. ADR-0001's promise is about log shape, not integration.
- **No public-internet hardening claims.** The README is explicit that
  this is not battle-tested as a security boundary. Items 5, 6, and 11
  shrink that gap but do not close it; do not let a v2 changelog imply
  they did.
- **Scope discipline per phase.** The retrospective's strongest lesson:
  one session, one phase, reviewable diffs. The tiers above are ordered
  so each item can ship alone.
