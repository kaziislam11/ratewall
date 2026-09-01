# Shared image for the mock CRM/HRM echo stubs.
# Context must be the repo root (docker-compose sets it) because core/ is a path dependency.
# Note: the workspace manifest lists all members, so every member's manifest
# must be present even when building a single package.
FROM rust:1.88-slim AS builder
WORKDIR /build

COPY Cargo.toml ./
COPY gateway/Cargo.toml gateway/Cargo.toml
COPY core/ core/
COPY mock-backends/ mock-backends/
RUN cargo build --release -p ratewall-mock-backends

FROM debian:bookworm-slim
COPY --from=builder /build/target/release/mock-crm /usr/local/bin/mock-crm
COPY --from=builder /build/target/release/mock-hrm /usr/local/bin/mock-hrm
