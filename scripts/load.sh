#!/usr/bin/env bash
# Load test (Phase 6, layer 5): regenerate the latency numbers behind the
# README's "Numbers, with the environment attached" section, or produce a
# fresh baseline for this machine.
#
# Profile (identical to the committed baseline in bench/baseline.md):
#   - oha in a container (--network host), closed-loop, HTTP/1.1
#   - proxied GET through the full chain (Ed25519 verify, Redis counter,
#     breaker admission, mock-CRM round-trip), 20 connections, 30s
#   - the same at 100 connections, 20s (saturation shape)
#   - /healthz, 15s (gateway floor, no auth/Redis/proxy hop)
#   - direct to the mock CRM, bypassing the gateway, 15s (the overhead delta)
#
# The demo rate limit is raised for the duration and restored afterwards;
# the limiter stays in the request path, it just won't 429 the run.
set -euo pipefail

BASE="${BASE:-http://localhost:8080}"
COMPOSE="${COMPOSE:-docker compose}"
GATEWAY_PORT="${GATEWAY_PORT:-8080}"
CRM_PORT="${CRM_PORT:-3000}"
OHA_IMAGE="${OHA_IMAGE:-ghcr.io/hatoo/oha:latest}"
DURATION_PROXIED="${DURATION_PROXIED:-30s}"
DURATION_SATURATION="${DURATION_SATURATION:-20s}"
DURATION_FLOOR="${DURATION_FLOOR:-15s}"
CONN_PROXIED="${CONN_PROXIED:-20}"
CONN_SATURATION="${CONN_SATURATION:-100}"

fail() { echo "FAIL: $*" >&2; exit 1; }

# oha inside a container: keeps the tool off the host. Arguments are
# forwarded as positional params (`-c 'oha "$@"' _ "$@"`), preserving
# bash's word splitting exactly — a quoted header value with spaces
# survives, which matters because the bearer token is passed that way.
OHA_TOKEN=""
oha() {
  docker run --rm --network host --entrypoint sh -e OHA_TOKEN "$OHA_IMAGE" -c 'oha "$@"' _ "$@"
}

# Extract "NN% in X ms|s" from oha output into milliseconds.
pct_ms() {
  awk -v p="$1" '$1 == p && $2 == "in" {
    v = $3; u = $4;
    if (u == "s") { printf "%.2f", v * 1000 }
    else if (u == "ms") { printf "%.2f", v }
    else if (u == "us") { printf "%.3f", v / 1000 }
    else if (u == "ns") { printf "%.6f", v / 1000000 }
    exit
  }'
}

avg_ms() {
  awk '/^  Average:/ {
    v = $2; u = $3;
    if (u == "s") { printf "%.2f", v * 1000 }
    else if (u == "ms") { printf "%.2f", v }
    else if (u == "us") { printf "%.3f", v / 1000 }
    else if (u == "ns") { printf "%.6f", v / 1000000 }
    exit
  }'
}

echo "── prerequisites"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")" = "200" ] \
  || fail "gateway not healthy at $BASE — run: docker compose up -d --wait"

TOKEN=$(curl -s -X POST -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' \
  "$BASE/auth/login" | sed 's/.*"token":"\([^"]*\)".*/\1/')
[ -n "$TOKEN" ] && [ "$TOKEN" != "$BASE" ] || fail "could not mint a token"

# Raise the rate limit so the run measures latency, not the limiter (the
# limiter under load was validated separately: it 429'd 284k requests at
# ~10k rps and kept serving — see the README baseline notes). Restored in
# the trap below no matter how the script exits.
CONFIG_FILE="$(git rev-parse --show-toplevel)/config.toml"
ORIG_LIMIT=$(sed -n 's/^limit = \([0-9]*\)$/\1/p' "$CONFIG_FILE" | head -1)
[ -n "$ORIG_LIMIT" ] || fail "could not find 'limit =' in $CONFIG_FILE"
restore_limit() {
  sed -i "s/^limit = .*/limit = $ORIG_LIMIT/" "$CONFIG_FILE" >/dev/null 2>&1
  git -C "$(dirname "$CONFIG_FILE")" checkout -q -- config.toml 2>/dev/null || true
}
trap restore_limit EXIT

sed -i "s/^limit = .*/limit = 1000000/" "$CONFIG_FILE"
$COMPOSE restart gateway >/dev/null
for _ in $(seq 1 20); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")" = "200" ] && break
  sleep 1
done
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")" = "200" ] \
  || fail "gateway did not come back after limit raise"
echo "   ok (limit raised to 1e6 for the run; restored on exit)"

RUN_TAG="$(date +%Y%m%d-%H%M%S)"
MARKER="loadtest-$RUN_TAG"

echo
echo "── run 1: proxied GET, full chain, $CONN_PROXIED connections, $DURATION_PROXIED"
OHA_TOKEN="Authorization: Bearer $TOKEN"
OUT1=$(oha --no-tui -z "$DURATION_PROXIED" -c "$CONN_PROXIED" \
  -H "$OHA_TOKEN" \
  "http://localhost:$GATEWAY_PORT/crm/$MARKER-r1")
echo "$OUT1" | grep -E '^\s+\[200\]' >/dev/null || { echo "$OUT1" | tail -20; fail "run 1: expected 200s"; }

echo "── run 2: saturation, $CONN_SATURATION connections, $DURATION_SATURATION"
OUT2=$(oha --no-tui -z "$DURATION_SATURATION" -c "$CONN_SATURATION" \
  -H "$OHA_TOKEN" \
  "http://localhost:$GATEWAY_PORT/crm/$MARKER-r2")
echo "$OUT2" | grep -E '^\s+\[200\]' >/dev/null || { echo "$OUT2" | tail -20; fail "run 2: expected 200s"; }

echo "── run 3: /healthz floor, $CONN_PROXIED connections, $DURATION_FLOOR"
OUT3=$(oha --no-tui -z "$DURATION_FLOOR" -c "$CONN_PROXIED" \
  "http://localhost:$GATEWAY_PORT/healthz")

echo "── run 4: direct to mock CRM (bypasses gateway), $CONN_PROXIED connections, $DURATION_FLOOR"
OUT4=$(oha --no-tui -z "$DURATION_FLOOR" -c "$CONN_PROXIED" \
  "http://localhost:$CRM_PORT/$MARKER-direct")

echo
echo "════ Results (milliseconds) — commit to bench/baseline.md with the environment label ════"
printf '%-46s %8s %8s %8s %8s\n' "scenario" "avg" "p50" "p95" "p99"
printf '%-46s %8s %8s %8s %8s\n' \
  "proxied (auth+ratelimit+breaker), ${CONN_PROXIED}conn" \
  "$(echo "$OUT1" | avg_ms)" \
  "$(echo "$OUT1" | pct_ms 50.00%)" \
  "$(echo "$OUT1" | pct_ms 95.00%)" \
  "$(echo "$OUT1" | pct_ms 99.00%)"
printf '%-46s %8s %8s %8s %8s\n' \
  "proxied saturating, ${CONN_SATURATION}conn" \
  "$(echo "$OUT2" | avg_ms)" \
  "$(echo "$OUT2" | pct_ms 50.00%)" \
  "$(echo "$OUT2" | pct_ms 95.00%)" \
  "$(echo "$OUT2" | pct_ms 99.00%)"
printf '%-46s %8s %8s %8s %8s\n' \
  "/healthz floor" \
  "$(echo "$OUT3" | avg_ms)" \
  "$(echo "$OUT3" | pct_ms 50.00%)" \
  "$(echo "$OUT3" | pct_ms 95.00%)" \
  "$(echo "$OUT3" | pct_ms 99.00%)"
printf '%-46s %8s %8s %8s %8s\n' \
  "direct to CRM (no gateway)" \
  "$(echo "$OUT4" | avg_ms)" \
  "$(echo "$OUT4" | pct_ms 50.00%)" \
  "$(echo "$OUT4" | pct_ms 95.00%)" \
  "$(echo "$OUT4" | pct_ms 99.00%)"
echo
echo "Environment label for the baseline (edit honestly):"
echo "  host: $(uname -s) | docker: $(docker --version | cut -d, -f1) | generator: oha in-container, --network host"
echo
echo "Full run-1 distribution (for reference):"
echo "$OUT1" | sed -n '/Response time distribution/,/^$/p'
