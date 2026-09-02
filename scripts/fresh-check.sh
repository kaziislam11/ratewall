#!/usr/bin/env bash
# Fresh-environment check (see docs/notes/retrospective.md, "what to avoid"):
# every red CI run after setup was some flavor of "works on my machine
# because my machine is dirty" — a local cluster with leftover state, a
# test environment that didn't match CI's. This script reproduces the
# environments that matter, from scratch, before pushing:
#
#   (default)  fmt + clippy + the full test suite in the exact CI image
#              (rust:1.88-slim), twice — with a disposable Redis
#              (fresh name each run, removed on exit) and with none,
#              matching both environments the flow tests pin.
#   --k8s      additionally delete the kind cluster and run both k8s
#              demos cold — the exact check that would have caught the
#              namespace-before-Secret failure before four red runs.
#
# As a pre-push gate: `just install-hooks RATEWALL_FRESH=1` makes every
# push run the default mode. CI remains the real gate; `git push
# --no-verify` skips any of this.
#
# Prereqs: docker. (--k8s additionally needs kind + kubectl.)
set -euo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

CLUSTER="${CLUSTER:-ratewall}"

step() { printf '\n── %s ──\n' "$1"; }

# fmt + clippy + tests, inside the CI image. --network host matches the
# invocation every prior suite run used; /$(pwd) turns Git Bash's
# /c/Users/... into the //c/Users/... form Docker Desktop accepts without
# MSYS path mangling (MSYS_NO_PATHCONV=1). Optional docker-run args come
# first (unquoted on purpose so `-e K=V` splits).
in_ci_image() {
  MSYS_NO_PATHCONV=1 docker run --rm --network host ${1:-} \
    -v "/$(pwd):/app" -w /app rust:1.88-slim \
    bash -c "rustup component add rustfmt clippy >/dev/null 2>&1
             cargo fmt --all -- --check
             cargo clippy --workspace --all-targets -- -D warnings
             cargo test --workspace"
}

step "suite, no Redis (CI's environment)"
in_ci_image
echo "ok: fail-open environment matches CI"

step "suite, disposable Redis (enforcement environment)"
# The disposable Redis runs on the host network: that is the only path a
# --network host test container can reach reliably on Docker Desktop (a
# randomly-published port on 127.0.0.1 is not reachable from there).
# Guard against an existing Redis on 6379 — otherwise this one fails to
# bind and the tests silently run against the old one, defeating the
# whole point of a fresh environment.
CTR="ratewall-fresh-redis-$$"
if MSYS_NO_PATHCONV=1 docker run --rm --network host redis:7-alpine \
    redis-cli -h 127.0.0.1 -p 6379 ping >/dev/null 2>&1; then
  echo "FAIL: something already answers on 127.0.0.1:6379 (likely 'docker compose up redis')." >&2
  echo "      Stop it first so the disposable Redis can take the port." >&2
  exit 1
fi
docker run -d --name "$CTR" --network host redis:7-alpine >/dev/null
trap 'docker rm -f "$CTR" >/dev/null 2>&1' EXIT
sleep 2
REDIS_URL="redis://127.0.0.1:6379"
in_ci_image "-e RATEWALL_TEST_REDIS=$REDIS_URL"
echo "ok: enforcement environment passes (Redis: $REDIS_URL)"

if [ "${1:-}" = "--k8s" ]; then
  step "k8s demos, cold (delete + recreate the cluster)"
  command -v kind >/dev/null 2>&1 || { echo "FAIL: kind not found" >&2; exit 1; }
  command -v kubectl >/dev/null 2>&1 || { echo "FAIL: kubectl not found" >&2; exit 1; }
  kind delete cluster --name "$CLUSTER" 2>/dev/null || true
  bash scripts/k8s-demo.sh
  bash scripts/k8s-rolling-update.sh
  echo "ok: fresh-cluster path passes"
fi

printf '\nPASS: fresh-environment check clean — safe to push.\n'
