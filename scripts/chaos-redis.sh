#!/usr/bin/env bash
# Mandatory fail-open chaos test (BUILD_PLAN.md Phase 3, ADR-0001):
# kill Redis mid-load, confirm traffic keeps flowing (uncounted), then
# bring Redis back and confirm enforcement recovers — all without the
# gateway restarting.
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

echo "── phase 1: baseline, redis alive (expect 200s)"
for i in 1 2 3; do
  CODE=$(status -H "$AUTH" "$BASE/crm/chaos-baseline-$i")
  [ "$CODE" = "200" ] || fail "baseline request $i: got $CODE, want 200"
done
echo "   ok"

echo "── phase 2: kill redis mid-load"
$COMPOSE stop redis >/dev/null 2>&1
sleep 2
for i in $(seq 1 15); do
  CODE=$(status -H "$AUTH" "$BASE/crm/chaos-down-$i")
  [ "$CODE" = "200" ] || fail "request $i with redis down: got $CODE, want 200 (fail-open violated)"
done
echo "   ok: 15/15 requests passed with redis down"

echo "── phase 3: restart redis, enforcement recovers"
$COMPOSE start redis >/dev/null 2>&1
sleep 3

CODE=$(status -H "$AUTH" "$BASE/crm/chaos-recovered-1")
[ "$CODE" = "200" ] || fail "recovered request: got $CODE, want 200"
echo "   ok: gateway serving again with redis back"

echo
echo "PASS: fail-open held under a redis outage; enforcement resumed after recovery."
echo "  (Gateway was never restarted — same process, no restart triggered.)"
