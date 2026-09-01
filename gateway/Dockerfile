# ratewall gateway image.
# Context must be the repo root (docker-compose sets it) because core/ is a path dependency.
# Note: the workspace manifest lists all members, so every member's manifest
# must be present even when building a single package.
FROM rust:1.88-slim AS builder
WORKDIR /build

COPY Cargo.toml ./
COPY gateway/Cargo.toml gateway/Cargo.toml
COPY mock-backends/Cargo.toml mock-backends/Cargo.toml
COPY core/ core/
COPY gateway/ gateway/
RUN cargo build --release -p ratewall-gateway

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/ratewall /usr/local/bin/ratewall
EXPOSE 8080
CMD ["ratewall"]
