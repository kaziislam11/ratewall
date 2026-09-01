# ADR-0002: Split core (library) from gateway (binary)

Date: 2026-08-31 · Status: accepted · Phase: 0

## Context

The workspace follows the h2proxy pattern of a thin binary over a library.
We need to fix where the ratewall logic lives: in the binary's module tree,
or in a separate library crate.

## Decision

- `core/` — library `ratewall-core`: router, middleware, config, rate
  limiter, circuit breaker. Knows nothing about argv, environment, or
  process lifecycle.
- `gateway/` — binary `ratewall`: config/ENV loading, log initialization,
  server bootstrap. Deliberately thin; any business logic here is a review
  error.

## Consequences

- Integration tests (Phase 6) import `ratewall_core` and exercise the app
  in-process — faster and more deterministic than subprocess testing.
- Demo scripts and future CLIs can reuse the same engine with different
  middleware stacks.
- Cost: one more crate; the binary must stay thin — business logic in it is
  a review error.
