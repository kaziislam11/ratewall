#!/usr/bin/env bash
# Smoke test for the full compose stack: boot everything, then hit every
# public surface and assert the expected status codes. This is the test
# that proves the README's claims are true, not aspirational.
#
# Prereqs: docker compose up -d --build has completed (all services healthy).
set -euo pipefail

BASE="${BASE:-http://localhost:8080}"

fail() { echo "FAIL: $*" >&2; exit 1; }

status() { curl -s -o /dev/null -w "%{http_code}" "$@"; }

wait_healthy() {
  for _ in $(seq 1 30); do
    [ "$(status "$BASE/healthz")" = "200" ] && return 0
    sleep 1
  done
  fail "gateway /healthz never returned 200"
}

wait_healthy

echo "── health & readiness"
[ "$(status "$BASE/healthz")" = "200" ] || fail "/healthz"
echo "   ok"

echo "── login (demo credential, 15-minute token)"
LOGIN_CODE=$(status -X POST -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' "$BASE/auth/login")
[ "$LOGIN_CODE" = "200" ] || fail "/auth/login: got $LOGIN_CODE, want 200"
TOKEN=$(curl -s -X POST -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' \
  "$BASE/auth/login" | sed 's/.*"token":"\([^"]*\)".*/\1/')
[ -n "$TOKEN" ] && [ "$TOKEN" != "$BASE" ] || fail "could not mint a token"
echo "   ok"

echo "── auth is fail-closed"
[ "$(status "$BASE/crm/without-token")" = "401" ] || fail "no token must be 401"
[ "$(status -H "Authorization: Bearer not.a.jwt" "$BASE/crm/garbage-token")" = "401" ] \
  || fail "garbage token must be 401 (fail-closed)"
echo "   ok"

echo "── proxying round-trips with a valid token"
BODY=$(curl -s -H "Authorization: Bearer $TOKEN" "$BASE/crm/customers/42")
echo "$BODY" | grep -q '"service":"crm"' || fail "crm body missing service field: $BODY"
echo "$BODY" | grep -q '"path":"/customers/42"' || fail "crm body wrong path: $BODY"
BODY=$(curl -s -H "Authorization: Bearer $TOKEN" "$BASE/hrm/employees/7")
echo "$BODY" | grep -q '"service":"hrm"' || fail "hrm body missing service field: $BODY"
echo "   ok"

echo "── metrics endpoint (Prometheus format)"
[ "$(status "$BASE/metrics")" = "200" ] || fail "/metrics"
METRICS=$(curl -s "$BASE/metrics")
echo "$METRICS" | grep -q '^# HELP ratewall_requests_total' || fail "metrics missing HELP header"
echo "$METRICS" | grep -q 'ratewall_breaker_state{prefix="crm"}' || fail "metrics missing breaker gauge"
echo "   ok"

echo "── unknown prefix and edge cases"
[ "$(status -H "Authorization: Bearer $TOKEN" "$BASE/nope/thing")" = "404" ] \
  || fail "unknown prefix must be 404"
# Rejected requests, each for a different reason: wrong/missing media type
# is 415, present-but-invalid JSON is 400, wrong method is 405.
[ "$(status -X POST "$BASE/auth/login")" = "415" ] \
  || fail "login with no content-type must be 415"
[ "$(status -X POST -H 'content-type: application/json' --data-binary '' "$BASE/auth/login")" = "400" ] \
  || fail "login with an empty JSON body must be 400"
[ "$(status -X POST -H 'content-type: application/json' -d 'not-json' "$BASE/auth/login")" = "400" ] \
  || fail "login with malformed JSON must be 400"
[ "$(status -X PUT "$BASE/auth/login")" = "405" ] || fail "PUT /auth/login must be 405"
echo "   ok"

echo
echo "PASS: smoke — every public surface behaved as documented."
