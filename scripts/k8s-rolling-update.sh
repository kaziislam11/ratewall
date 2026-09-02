#!/usr/bin/env bash
# Phase 7 demo (BUILD_PLAN.md): a rolling gateway update under continuous
# load with zero dropped requests.
#
# Mechanism: build the gateway image once, tag it as two releases (v1, v2),
# and roll between them — Kubernetes sees a changed image reference, so
# every replica is genuinely replaced. Because /readyz reflects real
# component health, a replica only leaves and rejoins the Service when it
# can actually serve — that is what makes the zero-dropped assertion
# honest rather than lucky.
#
# Prereqs: an up-to-date cluster from scripts/k8s-demo.sh (it reuses it).
set -euo pipefail

KIND="${KIND:-kind}"
CLUSTER="${CLUSTER:-ratewall}"
HOST_PORT="${HOST_PORT:-31080}"
BASE="http://localhost:${HOST_PORT}"
OLD_TAG="v1"
NEW_TAG="v2"

fail() { echo "FAIL: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have "$KIND" || fail "kind not found"
have kubectl || fail "kubectl not found"
have curl || fail "curl not found"
kubectl get ns ratewall >/dev/null 2>&1 \
  || fail "ratewall namespace not found — run scripts/k8s-demo.sh first"

cd "$(dirname "$0")/.."

echo "── release tags ─────────────────────────────────"
# One image, two tags: the rollout is driven by the changed image
# reference, exactly like a real release.
docker build -f gateway/Dockerfile -t ratewall-gateway:$OLD_TAG .
docker tag ratewall-gateway:$OLD_TAG ratewall-gateway:$NEW_TAG
"$KIND" load docker-image ratewall-gateway:$OLD_TAG --name "$CLUSTER"
"$KIND" load docker-image ratewall-gateway:$NEW_TAG --name "$CLUSTER"

# Pin the current release, then bump to the new one.
kubectl --namespace ratewall set image deploy/gateway \
  "gateway=ratewall-gateway:$OLD_TAG"
kubectl --namespace ratewall rollout status deploy/gateway --timeout=120s

echo "── login ────────────────────────────────────────"
for _ in $(seq 1 30); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")" = "200" ] && break
  sleep 2
done
TOKEN="$(curl -s -X POST "$BASE/auth/login" -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
[ -n "$TOKEN" ] || fail "login did not return a token"
AUTH="Authorization: Bearer $TOKEN"

status() { curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" "$@"; }
# The demo's load loop may have just exhausted the 60s rate-limit window
# for the shared demo subject, so the first ping can legitimately 429.
# Wait (bounded past the window) for a 200; anything else is a real
# failure. A 429 here is the limiter working, not the gateway being sick.
PING_OK=""
for _ in $(seq 1 35); do
  case "$(status "$BASE/crm/ping")" in
    200) PING_OK=1; break ;;
    429) sleep 2 ;;
    *) fail "proxied request failed before rollout" ;;
  esac
done
[ -n "$PING_OK" ] || fail "no successful request before rollout (limiter window never freed)"

echo "── rolling update under load ────────────────────"
# The demo limiter allows 100 req/min per subject, and the load loop runs
# for 60s — so enforcement will 429 the tail of the window. Rather than
# disabling the limiter, the loop pauses 2s on any 429: a steady-state
# 429 is the limiter working, not the rollout dropping requests. Only
# errors (5xx, timeouts, connection failures) count as drops.
LOAD_OUT="$(mktemp)"
(
  end=$((SECONDS + 60))
  while [ $SECONDS -lt $end ]; do
    code=$(status "$BASE/crm/rollout")
    echo "$code" >>"$LOAD_OUT"
    if [ "$code" = "429" ]; then sleep 2; fi
    sleep 0.05
  done
) &
LOAD_PID=$!

sleep 2
kubectl --namespace ratewall set image deploy/gateway \
  "gateway=ratewall-gateway:$NEW_TAG"
kubectl --namespace ratewall rollout status deploy/gateway --timeout=180s
echo "rollout complete; letting load run a few more seconds..."

wait "$LOAD_PID"
TOTAL="$(grep -c '^' "$LOAD_OUT")"
OKS="$(grep -c '^200$' "$LOAD_OUT" || true)"
LIMITED="$(grep -c '^429$' "$LOAD_OUT" || true)"
NON_OK="$(grep -vc -e '^200$' -e '^429$' "$LOAD_OUT" || true)"
echo "load: $TOTAL requests, $OKS ok, $LIMITED rate-limited (limiter working), $NON_OK dropped"
if [ "$NON_OK" != "0" ]; then
  echo "dropped-request statuses seen:" >&2
  grep -v -e '^200$' -e '^429$' "$LOAD_OUT" | sort | uniq -c | sort -rn | head >&2
  fail "requests were dropped during the rolling update"
fi
[ "$OKS" -ge 50 ] || fail "suspiciously few successful requests ($OKS) — load loop may have stalled"

# Confirm the Deployment really moved to the new image.
ACTUAL="$(kubectl --namespace ratewall get deploy gateway -o jsonpath='{.spec.template.spec.containers[0].image}')"
echo "deployed image: $ACTUAL"
case "$ACTUAL" in
  *"$NEW_TAG") ;;
  *) fail "expected image tag $NEW_TAG, got $ACTUAL" ;;
esac

rm -f "$LOAD_OUT"
echo "PASS: rolling update completed with zero dropped requests across $TOTAL requests."
