default:
    @just --list

# Build the workspace
build:
    cargo build --workspace

# Run all tests (unit + integration)
test:
    cargo test --workspace

# Start the full demo stack: gateway + CRM/HRM mocks + Redis
up:
    docker compose up --build -d

# Stop the demo stack
down:
    docker compose down

# Tail gateway logs
logs:
    docker compose logs -f gateway

# Format + lint
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace -- -D warnings

# Format in place
fmt:
    cargo fmt --all

# Mandatory fail-open chaos test: kill redis mid-load, traffic keeps flowing
chaos:
    bash scripts/chaos-redis.sh

# Smoke-test the running stack: every public surface, expected status codes
smoke:
    bash scripts/smoke.sh

# Phase 4 chaos: kill a backend, breaker opens, self-heals after recovery
chaos-backend:
    bash scripts/chaos-backend.sh

# Kind demo: deploy to a local cluster, kill a gateway pod under load
k8s-demo:
    bash scripts/k8s-demo.sh

# Kind demo: rolling gateway update under load, zero dropped requests
k8s-rolling-update:
    bash scripts/k8s-rolling-update.sh

# Delete the kind cluster the demos created
k8s-down:
    kind delete cluster --name ratewall

# Load test: regenerate the latency numbers (see bench/baseline.md)
loadtest:
    bash scripts/load.sh

# Install the pre-push hook (fmt + clippy gate) for this clone
install-hooks:
    git config core.hooksPath scripts/hooks
    @echo "installed: pushes now run 'just lint' equivalent first (git push --no-verify to skip)"
