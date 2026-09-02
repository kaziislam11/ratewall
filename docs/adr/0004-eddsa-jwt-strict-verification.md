# ADR-0004: JWT verification is strict, minimal, and in-house (no JWT library)

Date: 2026-09-01 · Status: accepted · Phase: 2

## Context

Phase 2 adds authentication. Two decisions had to be made together:

1. Which tokens to accept. The gateway should own its signing keys by
   default (zero-setup demo) and also support an external issuer later
   (Supabase et al.) — a config change, not a code change.
2. How to implement EdDSA JWTs. Use a JWT crate, or hand-roll the compact
   JWS format over `ed25519-dalek`?

## Decision

- **Ed25519 (`EdDSA`) only.** The verifier checks the `alg` header against
  exactly one value before any other processing. This structurally rejects
  the classic algorithm-confusion family of JWT attacks (`alg: none`,
  RS256/HS256 confusion) rather than trying to enumerate them.
- **Claims are `sub`, `iss`, `iat`, `exp` and nothing else.** `exp` is
  mandatory; expired means rejected. `iss` is checked against the
  configured trusted issuer.
- **Hand-rolled JWS over `ed25519-dalek`.** The compact serialization is
  ~40 lines: base64url-decode three segments, verify signature over the
  raw signing input, decode and check claims. Using a JWT library here
  would have pulled in a multi-feature dependency chain for behavior we
  would then have had to subset anyway (EdDSA-only, no JWE, no JSON
  flattening). The full verification surface is visible in one file and
  covered by unit tests including tampering and `alg: none`.
- **Two modes, chosen by config.** No `[auth]` section → own-keys mode:
  generate/persist a keypair at `keys_dir` (on a volume), enable
  `POST /auth/login` with documented demo credentials. `issuer` +
  `issuer_public_key_pem` → external mode: verify only, login disabled.
  The half-configured states are startup errors (ADR-0003).
- **Fail-closed everywhere.** Key material unreadable or corrupt at boot →
  the gateway exits. Any verification error → 401. Rejection bodies do not
  distinguish failure modes ("invalid token" for everything); the detail
  goes to logs where operators can see it.

## Consequences

- The demo stack is usable with zero setup: compose up, login, use the
  token. Keys live on the `gateway-keys` volume so tokens survive
  restarts and image rebuilds.
- No JWT library dependency; the crypto core is the well-audited
  `ed25519-dalek`, and our code only formats/verifies.
- If token requirements grow (refresh tokens, scopes, JWKS rotation), the
  hand-rolled verifier is small enough to replace with a library at that
  point — the `AuthError`/`AuthenticatedSubject` interface would not
  change.
- The demo credential is public knowledge by design. Operators in
  external-issuer mode get their real identity story from their IdP; the
  demo mode must never face the internet.
- `/auth/login` is unthrottled. Today that costs nothing: the only
  credential is the published demo pair, so brute-forcing buys an
  attacker nothing. It becomes a real problem at the moment either (a) a
  real credential store replaces the hardcoded check — login must then
  sit behind the Phase 3 rate limiter or it is an online password
  oracle — or (b) login is exposed on a network where unauthenticated
  request volume itself is the threat. Wire the limiter in front of this
  route as part of whichever change brings real credentials; do not
  ship real credentials without it.
