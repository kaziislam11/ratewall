#!/usr/bin/env bash
# Phase 4 chaos test (BUILD_PLAN.md): kill a mock backend mid-load, confirm
# the gateway degrades gracefully and self-heals without a restart:
#
#   1. baseline: CRM answers 200
#   2. kill CRM: requests fail (502), then the breaker opens → fast 503
#      (never a hang)
#   3. restart CRM: after the cooldown, the half-open probe succeeds →
#      circuit closes → 200s resume
#
# Prereqs: the compose stack is up (`just up`) and curl is available.
set -euo pipefail

BASE="${BASE:-http://localhost:8080}"
COMPOSE="${COMPOSE:-docker compose}"

fail() { echo "FAIL: $*" >&2; exit 1; }

status() { curl -s -o /dev/null -w "%{http_code}" "$@"; }

echo "── login"
TOKEN=$(curl -s -X POST -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' \
  "$BASE/auth/login" | sed 's/.*"token":"\([^"]*\)".*/\1/')
[ -n "$TOKEN" ] && [ "$TOKEN" != "$BASE" ] || fail "could not mint a token"
AUTH="Authorization: Bearer $TOKEN"

echo "── phase 1: baseline, CRM alive (expect 200)"
for i in 1 2 3; do
  CODE=$(status -H "$AUTH" "$BASE/crm/backend-chaos-ok-$i")
  [ "$CODE" = "200" ] || fail "baseline request $i: got $CODE, want 200"
done
echo "   ok"

echo "── phase 2: kill CRM mid-load"
$COMPOSE stop crm >/dev/null 2>&1

# First failures surface as 502 (backend unreachable); after enough of them
# the breaker opens and the same dead backend yields fast 503s.
SAW_502=0; SAW_503=0
for i in $(seq 1 12); do
  CODE=$(status -H "$AUTH" "$BASE/crm/backend-chaos-down-$i")
  case "$CODE" in
    502) SAW_502=1 ;;
    503) SAW_503=1 ;;
    *) fail "request $i with CRM down: got $CODE, want 502 or 503" ;;
  esac
done
[ "$SAW_502" = "1" ] || fail "never saw 502 while CRM was down — is the breaker mis-wired?"
[ "$SAW_503" = "1" ] || fail "never saw a 503 short-circuit — breaker never opened"
echo "   ok: saw 502 (unreachable) then 503 (circuit open), never a hang"

echo "── phase 3: restart CRM, breaker must self-heal"
$COMPOSE start crm >/dev/null 2>&1

# Cooldown is 30s in config; poll until a probe closes the circuit.
RECOVERED=""
for _ in $(seq 1 40); do
  sleep 2
  CODE=$(status -H "$AUTH" "$BASE/crm/backend-chaos-recovered")
  if [ "$CODE" = "200" ]; then RECOVERED=yes; break; fi
  [ "$CODE" = "503" ] || [ "$CODE" = "502" ] || fail "during recovery: got $CODE, want 200/502/503"
done
[ -n "$RECOVERED" ] || fail "CRM never recovered — breaker did not close after backend restart"

# And it stays healthy.
for i in 1 2 3; do
  CODE=$(status -H "$AUTH" "$BASE/crm/backend-chaos-after-$i")
  [ "$CODE" = "200" ] || fail "post-recovery request $i: got $CODE, want 200"
done
echo "   ok: circuit closed, traffic resumed"

echo
echo "PASS: a dead backend degraded gracefully (fast 503s) and self-healed without a gateway restart."
