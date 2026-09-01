# ADR-0003: Config is validated at startup; the gateway refuses to start on bad config

Date: 2026-09-01 · Status: accepted · Phases: 1+

## Context

Phase 1 introduces the first real configuration surface: the listen port and
the route table mapping path prefixes (`/crm/*`, `/hrm/*`) to backend URLs.
A misconfigured route table is worse than a missing one — a gateway that
starts with an empty table looks healthy but silently serves 404s for every
proxied path. Failures discovered at request time are also the hardest to
diagnose: they surface as user-facing errors far from their cause.

## Decision

The gateway loads and validates all configuration **once, at startup**, and
exits non-zero on any of:

- missing or unreadable config file
- malformed TOML
- unknown keys (catches typos instead of silently ignoring them)
- empty route table
- route prefix that is not a single clean path segment
- backend that is not an absolute http(s) URL

A valid start is therefore a guarantee: every configured route is well-formed
and every proxy failure after boot is a runtime event, not a config bug.

## Consequences

- Container orchestrators (compose, later Kubernetes) get a clean
  crash-loop signal on bad config instead of a zombie gateway.
- Validation lives in `ratewall-core` (not the binary) so tests can target it
  directly (ADR-0002 split).
- Later phases (auth keys, rate limits, breaker thresholds) extend the same
  struct and the same validate-at-boot rule — no per-request config reads.
- The cost is strictness: an unknown key is an error, not a warning. That is
  deliberate — silent config drift is how gateways rot.
