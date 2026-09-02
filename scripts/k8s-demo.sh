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

echo "── namespace ────────────────────────────────────"
# The Secret below lives in this namespace — create it before anything
# that references it. (A fresh cluster has nothing pre-created.)
kubectl apply -f k8s/00-namespace.yaml

echo "── signing key ──────────────────────────────────"
# One key for all replicas: a token minted by any replica must verify on
# every other one (ADR-0007). Generated once if absent, reused afterwards,
# so tokens minted by earlier demo runs stay valid.
if ! kubectl get secret gateway-keys --namespace ratewall >/dev/null 2>&1; then
  # Generate inside a container (openssl isn't assumed on the host) and
  # pipe the PEM out; no host filesystem writes, which also sidesteps
  # DockerDesktop file-sharing permissions on bind-mounted repo dirs.
  KEY_PEM="$(mktemp)"
  docker run --rm debian:bookworm-slim \
    sh -c 'apt-get update -qq >/dev/null 2>&1 \
      && apt-get install -y -qq openssl >/dev/null 2>&1 \
      && openssl genpkey -algorithm ed25519' >"$KEY_PEM" \
    || fail "could not generate a signing key"
  [ -s "$KEY_PEM" ] || fail "key generation produced an empty file"
  kubectl create secret generic gateway-keys \
    --namespace ratewall \
    --from-file=signing_key.pem="$KEY_PEM"
  rm -f "$KEY_PEM"
  echo "created gateway-keys Secret with a fresh Ed25519 key"
else
  echo "reusing existing gateway-keys Secret"
fi

echo "── deploy ───────────────────────────────────────"
# Apply the manifests only — kind-config.yaml (the cluster definition)
# lives beside them but is not a k8s manifest.
kubectl apply $(find k8s -name '*.yaml' ! -name 'kind-config.yaml' | sed 's/^/-f /')
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
# A steady-state 429 is the demo limiter doing its job (the load loop runs
# for a while at ~4 rps against a 100 req/min cap), not a dropped request.
# Only anything other than 200/429 means traffic actually failed.
LIMITED="$(grep -c '^429$' "$LOAD_OUT" || true)"
NON_OK="$(grep -vc -e '^200$' -e '^429$' "$LOAD_OUT" || true)"
echo "load: $TOTAL requests, $((TOTAL - NON_OK - LIMITED)) ok, $LIMITED rate-limited (limiter working), $NON_OK dropped"
if [ "$NON_OK" != "0" ]; then
  echo "dropped-request statuses seen:" >&2
  grep -v -e '^200$' -e '^429$' "$LOAD_OUT" | sort | uniq -c | sort -rn | head >&2
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
