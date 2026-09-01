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
