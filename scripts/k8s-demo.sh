#!/usr/bin/env bash
# Phase 7 demo (BUILD_PLAN.md): create a kind cluster, deploy the full
# ratewall stack, then kill a gateway pod under load and show that the
# survivors absorb the traffic while the Deployment replaces the pod —
# zero failed requests, no manual intervention.
#
# Prereqs: docker, kind, kubectl, curl. Cluster name is fixed so re-runs
# reuse (or recreate) the same cluster. Tear down with `just k8s-down`.
set -euo pipefail

KIND="${KIND:-kind}"
CLUSTER="${CLUSTER:-ratewall}"
HOST_PORT="${HOST_PORT:-31080}"
BASE="http://localhost:${HOST_PORT}"

fail() { echo "FAIL: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have "$KIND" || fail "kind not found (https://kind.sigs.k8s.io)"
have kubectl || fail "kubectl not found"
have curl || fail "curl not found"
docker info >/dev/null 2>&1 || fail "docker is not running"

cd "$(dirname "$0")/.."

echo "── cluster ──────────────────────────────────────"
if ! "$KIND" get clusters | grep -qx "$CLUSTER"; then
  "$KIND" create cluster --name "$CLUSTER" --config k8s/kind-config.yaml
fi
"$KIND" export kubeconfig --name "$CLUSTER" >/dev/null

echo "── images ───────────────────────────────────────"
docker build -f gateway/Dockerfile -t ratewall-gateway:latest .
docker build -f mock-backends/Dockerfile -t ratewall-mocks:latest .
"$KIND" load docker-image ratewall-gateway:latest --name "$CLUSTER"
"$KIND" load docker-image ratewall-mocks:latest --name "$CLUSTER"

echo "── signing key ──────────────────────────────────"
# One key for all replicas: a token minted by any replica must verify on
# every other one (ADR-0007). Generated once if absent, reused afterwards,
# so tokens minted by earlier demo runs stay valid.
if ! kubectl get secret gateway-keys --namespace ratewall >/dev/null 2>&1; then
  # Generate inside a container writing into the repo dir (a mounted host
  # temp path is unreliable from Git Bash on Windows), then read it back.
  rm -f k8s/.generated-signing_key.pem
  docker run --rm -v "$(pwd)/k8s:/out" debian:bookworm-slim \
    sh -c 'apt-get update -qq >/dev/null 2>&1 \
      && apt-get install -y -qq openssl >/dev/null 2>&1 \
      && openssl genpkey -algorithm ed25519 -out /out/.generated-signing_key.pem' \
    || fail "could not generate a signing key"
  [ -s k8s/.generated-signing_key.pem ] || fail "key generation produced an empty file"
  kubectl create secret generic gateway-keys \
    --namespace ratewall \
    --from-file=signing_key.pem=k8s/.generated-signing_key.pem
  rm -f k8s/.generated-signing_key.pem
  echo "created gateway-keys Secret with a fresh Ed25519 key"
else
  echo "reusing existing gateway-keys Secret"
fi

echo "── deploy ───────────────────────────────────────"
kubectl apply -f k8s/
# Expose via NodePort on the port k8s/kind-config.yaml already maps to the host.
kubectl patch svc gateway --namespace ratewall \
  -p '{"spec":{"type":"NodePort","ports":[{"port":8080,"targetPort":"http","nodePort":31080}]}}' \
  >/dev/null
kubectl --namespace ratewall rollout status deploy/gateway --timeout=180s
kubectl --namespace ratewall rollout status deploy/crm --timeout=120s
kubectl --namespace ratewall rollout status deploy/hrm --timeout=120s
kubectl --namespace ratewall get pods

# Wait until the gateway answers through the NodePort mapping.
echo "── waiting for the gateway on ${BASE} ───────────"
for _ in $(seq 1 30); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")" = "200" ] && break
  sleep 2
done
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/healthz")" = "200" ] \
  || fail "gateway never answered on $BASE"

TOKEN="$(curl -s -X POST "$BASE/auth/login" -H 'content-type: application/json' \
  -d '{"username":"demo","password":"demo-password"}' | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
[ -n "$TOKEN" ] || fail "login did not return a token"
AUTH="Authorization: Bearer $TOKEN"

[ "$(curl -s -o /dev/null -w '%{http_code}' -H "$AUTH" "$BASE/crm/customers/42")" = "200" ] \
  || fail "proxied request failed"

echo "── kill a gateway pod under load ────────────────"
POD="$(kubectl --namespace ratewall get pods -l app=gateway \
  -o jsonpath='{.items[0].metadata.name}')"
echo "killing $POD"

# Continuous authenticated load; every response status is recorded.
LOAD_OUT="$(mktemp)"
(
  end=$((SECONDS + 30))
  while [ $SECONDS -lt $end ]; do
    curl -s -o /dev/null -w '%{http_code}\n' -H "$AUTH" --max-time 5 \
      "$BASE/crm/load" >>"$LOAD_OUT"
    sleep 0.05
  done
) &
LOAD_PID=$!

sleep 3
kubectl --namespace ratewall delete pod "$POD" --wait=false >/dev/null

wait "$LOAD_PID"
TOTAL="$(grep -c '^' "$LOAD_OUT")"
NON_OK="$(grep -vc '^200$' "$LOAD_OUT" || true)"
echo "load: $TOTAL requests, $((TOTAL - NON_OK)) ok, $NON_OK non-ok"
if [ "$NON_OK" != "0" ]; then
  echo "non-200 statuses seen:" >&2
  grep -v '^200$' "$LOAD_OUT" | sort | uniq -c | sort -rn | head >&2
  fail "requests failed while a gateway pod was killed — traffic did not survive"
fi
[ "$TOTAL" -ge 50 ] || fail "suspiciously few requests ($TOTAL) — load loop may have stalled"

# The Deployment must have replaced the killed pod: back to 3 ready replicas.
for _ in $(seq 1 30); do
  READY="$(kubectl --namespace ratewall get deploy gateway \
    -o jsonpath='{.status.readyReplicas}')"
  [ "$READY" = "3" ] && break
  sleep 2
done
[ "$READY" = "3" ] || fail "gateway never returned to 3 ready replicas"
kubectl --namespace ratewall get pods -l app=gateway

rm -f "$LOAD_OUT"
echo "PASS: pod killed under load, survivors absorbed the traffic, replica replaced — zero failed requests."
echo "Next: scripts/k8s-rolling-update.sh (rolling update with zero dropped requests)."
echo "Tear down with: just k8s-down"
