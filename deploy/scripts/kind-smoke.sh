#!/usr/bin/env bash
#
# kind-smoke.sh — ephemeral-cluster deployment smoke test for smooth-operator.
#
# Stands up the Helm chart (deploy/k8s) on a kind cluster, backed by a throwaway
# pgvector Postgres, then drives the wire protocol over a *real* WebSocket to the
# live pod — replicating the in-process `rust/.../tests/protocol_smoke.rs` checks
# across the network/Helm/container boundary:
#
#   1. ping                         -> pong
#   2. create_conversation_session  -> immediate_response with a valid session id
#
# Neither action runs an LLM turn, so NO gateway key is needed (the server binds
# and answers protocol-only actions without SMOOAI_GATEWAY_KEY — see config.rs).
#
# ──────────────────────────────────────────────────────────────────────────
#  CROSS-REPO BUILD CONTEXT
# ──────────────────────────────────────────────────────────────────────────
# Single-repo build context: the engine crate `smooai-smooth-operator-core` is
# fetched from crates.io during the Docker build, so the context is just this
# repo's root, with -f pointing at this repo's Dockerfile.
#
# ──────────────────────────────────────────────────────────────────────────
#  USAGE
# ──────────────────────────────────────────────────────────────────────────
#   deploy/scripts/kind-smoke.sh                 # full run: create cluster, build, deploy, smoke, teardown
#   KEEP_CLUSTER=1 deploy/scripts/kind-smoke.sh  # leave the cluster up after the run
#   SKIP_BUILD=1   deploy/scripts/kind-smoke.sh  # reuse an already-loaded image (fast local reruns)
#   USE_EXISTING_CLUSTER=1 deploy/scripts/kind-smoke.sh   # target the current kube-context, don't create/delete a cluster
#
# Environment overrides (all optional):
#   CLUSTER_NAME           kind cluster name                 (default: smooth-agent-smoke)
#   IMAGE                  image tag built + loaded          (default: smooth-operator:smoke)
#   NAMESPACE              k8s namespace                     (default: smooth-agent-smoke)
#   RELEASE                helm release name                 (default: smooth-agent)
#   LOCAL_PORT             host port for the port-forward    (default: 18787)
#   SKIP_BUILD             reuse loaded image, skip docker build + kind load
#   USE_EXISTING_CLUSTER   use current context; skip kind create/delete
#   KEEP_CLUSTER           do not delete the kind cluster on exit
#
# Requires: kind, kubectl, helm, docker. A WS client — websocat OR python3
# (with `websockets`) OR node (with a global `ws`) — is auto-detected.
#
set -euo pipefail

# ── Resolve paths ───────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CHART_DIR="${REPO_ROOT}/deploy/k8s"

# ── Tunables ────────────────────────────────────────────────────────────────
CLUSTER_NAME="${CLUSTER_NAME:-smooth-agent-smoke}"
IMAGE="${IMAGE:-smooth-operator:smoke}"
NAMESPACE="${NAMESPACE:-smooth-agent-smoke}"
RELEASE="${RELEASE:-smooth-agent}"
LOCAL_PORT="${LOCAL_PORT:-18787}"
# Single-repo Docker context: the engine crate is fetched from crates.io, so the
# build context is just this repo's root.
DOCKERFILE="${REPO_ROOT}/Dockerfile"

# Throwaway pgvector Postgres deployed into the cluster.
PG_IMAGE="pgvector/pgvector:pg16"
PG_DB="smooth"
PG_USER="smooth"
PG_PASSWORD="smoke-password"          # ephemeral cluster only — never a real secret
PG_HOST="pgvector"                    # in-cluster Service DNS name
PG_PORT="5432"
DB_URL="postgresql://${PG_USER}:${PG_PASSWORD}@${PG_HOST}:${PG_PORT}/${PG_DB}?sslmode=disable"

PF_PID=""

log()  { printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ── Teardown (trap) ─────────────────────────────────────────────────────────
cleanup() {
    local code=$?
    set +e
    if [[ -n "${PF_PID}" ]] && kill -0 "${PF_PID}" 2>/dev/null; then
        log "Stopping port-forward (pid ${PF_PID})"
        kill "${PF_PID}" 2>/dev/null
        wait "${PF_PID}" 2>/dev/null
    fi
    if [[ "${code}" -ne 0 ]]; then
        warn "Run failed (exit ${code}) — dumping pod state for debugging"
        kubectl -n "${NAMESPACE}" get pods 2>/dev/null
        kubectl -n "${NAMESPACE}" describe pods -l "app.kubernetes.io/name=smooth-operator" 2>/dev/null | tail -40
        kubectl -n "${NAMESPACE}" logs -l "app.kubernetes.io/name=smooth-operator" --tail=80 2>/dev/null
    fi
    if [[ "${USE_EXISTING_CLUSTER:-0}" == "1" ]]; then
        # Don't delete a cluster we didn't create; just drop our namespace.
        kubectl delete namespace "${NAMESPACE}" --wait=false 2>/dev/null
    elif [[ "${KEEP_CLUSTER:-0}" == "1" ]]; then
        warn "KEEP_CLUSTER=1 — leaving kind cluster '${CLUSTER_NAME}' up. Delete with: kind delete cluster --name ${CLUSTER_NAME}"
    else
        log "Deleting kind cluster '${CLUSTER_NAME}'"
        kind delete cluster --name "${CLUSTER_NAME}" 2>/dev/null
    fi
    exit "${code}"
}
trap cleanup EXIT

# ── Preflight ───────────────────────────────────────────────────────────────
require() { command -v "$1" >/dev/null 2>&1 || die "required tool '$1' not found on PATH"; }
require kubectl
require helm
require kind
[[ "${SKIP_BUILD:-0}" == "1" ]] || require docker

# Detect a WebSocket client up-front so we fail fast (before standing up a cluster).
WS_CLIENT=""
if command -v websocat >/dev/null 2>&1; then
    WS_CLIENT="websocat"
elif command -v python3 >/dev/null 2>&1 && python3 -c "import websockets" >/dev/null 2>&1; then
    WS_CLIENT="python"
elif command -v node >/dev/null 2>&1 && node -e "require('ws')" >/dev/null 2>&1; then
    WS_CLIENT="node"
else
    die "no WebSocket client found — install one of: websocat, python3 'websockets' (pip install websockets), or node 'ws' (npm i -g ws)"
fi
log "Using WebSocket client: ${WS_CLIENT}"

# ── 1. Cluster ──────────────────────────────────────────────────────────────
if [[ "${USE_EXISTING_CLUSTER:-0}" == "1" ]]; then
    log "USE_EXISTING_CLUSTER=1 — targeting current kube-context: $(kubectl config current-context)"
else
    if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
        log "Reusing existing kind cluster '${CLUSTER_NAME}'"
    else
        log "Creating kind cluster '${CLUSTER_NAME}'"
        kind create cluster --name "${CLUSTER_NAME}" --wait 120s
    fi
    kubectl cluster-info --context "kind-${CLUSTER_NAME}" >/dev/null
fi

# ── 2. Build + load the image ────────────────────────────────────────────────
if [[ "${SKIP_BUILD:-0}" == "1" ]]; then
    log "SKIP_BUILD=1 — assuming image '${IMAGE}' is already loaded into the cluster"
else
    [[ -f "${DOCKERFILE}" ]] || die "Dockerfile not found at ${DOCKERFILE}"
    log "Building image '${IMAGE}' from '${REPO_ROOT}' (-f ${DOCKERFILE})"
    docker build -f "${DOCKERFILE}" -t "${IMAGE}" "${REPO_ROOT}"

    if [[ "${USE_EXISTING_CLUSTER:-0}" == "1" ]]; then
        warn "USE_EXISTING_CLUSTER=1 with a build: 'kind load' targets a kind cluster. If your context isn't kind, push '${IMAGE}' to a registry the cluster can pull instead."
    fi
    log "Loading image into kind cluster"
    kind load docker-image "${IMAGE}" --name "${CLUSTER_NAME}"
fi

kubectl create namespace "${NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f -

# ── 3. Throwaway pgvector Postgres ──────────────────────────────────────────
# A minimal single-replica Deployment + Service. emptyDir storage — this DB is
# disposable; the chart treats Postgres as external, so we provide it here only
# to satisfy the adapter's CREATE EXTENSION vector / table bootstrap on startup.
log "Deploying throwaway pgvector Postgres (${PG_IMAGE})"
kubectl apply -n "${NAMESPACE}" -f - <<YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pgvector
  labels: { app: pgvector }
spec:
  replicas: 1
  selector:
    matchLabels: { app: pgvector }
  template:
    metadata:
      labels: { app: pgvector }
    spec:
      containers:
        - name: postgres
          image: ${PG_IMAGE}
          env:
            - { name: POSTGRES_DB,       value: "${PG_DB}" }
            - { name: POSTGRES_USER,     value: "${PG_USER}" }
            - { name: POSTGRES_PASSWORD, value: "${PG_PASSWORD}" }
            # Let the default cluster come up under the non-root container user.
            - { name: PGDATA,            value: "/var/lib/postgresql/data/pgdata" }
          ports:
            - containerPort: ${PG_PORT}
          readinessProbe:
            exec:
              command: ["pg_isready", "-U", "${PG_USER}", "-d", "${PG_DB}"]
            initialDelaySeconds: 5
            periodSeconds: 5
            failureThreshold: 20
          volumeMounts:
            - { name: data, mountPath: /var/lib/postgresql/data }
      volumes:
        - name: data
          emptyDir: {}
---
apiVersion: v1
kind: Service
metadata:
  name: ${PG_HOST}
  labels: { app: pgvector }
spec:
  selector: { app: pgvector }
  ports:
    - port: ${PG_PORT}
      targetPort: ${PG_PORT}
YAML

log "Waiting for pgvector Postgres to be Ready"
kubectl -n "${NAMESPACE}" rollout status deployment/pgvector --timeout=180s

# ── 4. helm install the chart ───────────────────────────────────────────────
# Inline DB url (dev-only path; chart writes a managed Secret), 0.0.0.0 bind,
# NO gateway key (protocol-only smoke), single replica + IfNotPresent so the
# kind-loaded image is used (never pulled).
log "helm lint + install the chart from ${CHART_DIR}"
helm lint "${CHART_DIR}"
helm upgrade --install "${RELEASE}" "${CHART_DIR}" \
    --namespace "${NAMESPACE}" \
    --set image.repository="${IMAGE%%:*}" \
    --set image.tag="${IMAGE##*:}" \
    --set image.pullPolicy=IfNotPresent \
    --set replicaCount=1 \
    --set autoscaling.enabled=false \
    --set server.bind="0.0.0.0" \
    --set server.seedKb=false \
    --set database.url="${DB_URL}" \
    --wait --timeout 240s

log "Waiting for the agent pod to be Ready"
kubectl -n "${NAMESPACE}" rollout status \
    "deployment/$(helm get manifest "${RELEASE}" -n "${NAMESPACE}" | awk '/kind: Deployment/{d=1} d&&/name:/{print $2; exit}')" \
    --timeout=180s 2>/dev/null \
    || kubectl -n "${NAMESPACE}" wait --for=condition=available \
        deployment -l "app.kubernetes.io/instance=${RELEASE}" --timeout=180s

# ── 5. port-forward the Service ─────────────────────────────────────────────
SVC="$(kubectl -n "${NAMESPACE}" get svc -l "app.kubernetes.io/instance=${RELEASE}" \
        -o jsonpath='{.items[0].metadata.name}')"
SVC_PORT="$(kubectl -n "${NAMESPACE}" get svc "${SVC}" -o jsonpath='{.spec.ports[0].port}')"
log "Port-forwarding svc/${SVC} ${LOCAL_PORT} -> ${SVC_PORT}"
kubectl -n "${NAMESPACE}" port-forward "svc/${SVC}" "${LOCAL_PORT}:${SVC_PORT}" >/dev/null 2>&1 &
PF_PID=$!

# Wait for the forwarded port to accept TCP connections.
WS_URL="ws://127.0.0.1:${LOCAL_PORT}/ws"
for _ in $(seq 1 30); do
    if (exec 3<>"/dev/tcp/127.0.0.1/${LOCAL_PORT}") 2>/dev/null; then
        exec 3>&- 3<&-
        break
    fi
    sleep 1
done

# ── 6. Protocol smoke over the live WebSocket ───────────────────────────────
# Each client emits a single line of JSON to stdout per assertion outcome and
# exits non-zero on failure. The contract:
#   send {"action":"ping","requestId":"1"}                         -> type == "pong", requestId == "1"
#   send {"action":"create_conversation_session","requestId":"2",  -> type == "immediate_response",
#         "agentId":"<uuid>","userName":"Smoke"}                       data.sessionId is a non-empty uuid,
#                                                                       data.agentId == <uuid>
log "Running protocol smoke against ${WS_URL}"

AGENT_ID="$(uuidgen 2>/dev/null | tr '[:upper:]' '[:lower:]' || python3 -c 'import uuid;print(uuid.uuid4())')"

run_smoke_python() {
    python3 - "$WS_URL" "$AGENT_ID" <<'PY'
import asyncio, json, sys, uuid
import websockets

async def main(url, agent_id):
    async with websockets.connect(url, open_timeout=15, close_timeout=5) as ws:
        # 1. ping -> pong
        await ws.send(json.dumps({"action": "ping", "requestId": "1"}))
        ev = json.loads(await asyncio.wait_for(ws.recv(), timeout=15))
        assert ev.get("type") == "pong", f"expected pong, got: {ev}"
        assert ev.get("requestId") == "1", f"requestId mismatch: {ev}"
        print("  ✓ ping -> pong")

        # 2. create_conversation_session -> immediate_response w/ a valid session id
        await ws.send(json.dumps({
            "action": "create_conversation_session",
            "requestId": "2",
            "agentId": agent_id,
            "userName": "Smoke",
        }))
        ev = json.loads(await asyncio.wait_for(ws.recv(), timeout=15))
        assert ev.get("type") == "immediate_response", f"expected immediate_response, got: {ev}"
        assert ev.get("requestId") == "2", f"requestId mismatch: {ev}"
        data = ev.get("data") or {}
        sid = data.get("sessionId")
        assert isinstance(sid, str) and sid, f"missing sessionId: {ev}"
        uuid.UUID(sid)  # raises if not a valid uuid
        assert data.get("agentId") == agent_id, f"agentId not echoed: {ev}"
        print(f"  ✓ create_conversation_session -> sessionId={sid}")

asyncio.run(main(sys.argv[1], sys.argv[2]))
print("PROTOCOL SMOKE PASSED")
PY
}

run_smoke_node() {
    node - "$WS_URL" "$AGENT_ID" <<'JS'
const WebSocket = require('ws');
const [url, agentId] = process.argv.slice(2);
const ws = new WebSocket(url, { handshakeTimeout: 15000 });
const queue = [];
const waiters = [];
ws.on('message', (raw) => {
    const ev = JSON.parse(raw.toString());
    if (waiters.length) waiters.shift()(ev); else queue.push(ev);
});
const next = () => new Promise((res, rej) => {
    const t = setTimeout(() => rej(new Error('timeout waiting for message')), 15000);
    const deliver = (ev) => { clearTimeout(t); res(ev); };
    if (queue.length) deliver(queue.shift()); else waiters.push(deliver);
});
const assert = (cond, msg) => { if (!cond) { console.error('ASSERT FAIL: ' + msg); process.exit(1); } };
ws.on('error', (e) => { console.error('ws error: ' + e.message); process.exit(1); });
ws.on('open', async () => {
    try {
        ws.send(JSON.stringify({ action: 'ping', requestId: '1' }));
        let ev = await next();
        assert(ev.type === 'pong', 'expected pong, got ' + JSON.stringify(ev));
        assert(ev.requestId === '1', 'requestId mismatch: ' + JSON.stringify(ev));
        console.log('  ✓ ping -> pong');

        ws.send(JSON.stringify({ action: 'create_conversation_session', requestId: '2', agentId, userName: 'Smoke' }));
        ev = await next();
        assert(ev.type === 'immediate_response', 'expected immediate_response, got ' + JSON.stringify(ev));
        assert(ev.requestId === '2', 'requestId mismatch: ' + JSON.stringify(ev));
        const data = ev.data || {};
        const sid = data.sessionId;
        assert(typeof sid === 'string' && sid.length > 0, 'missing sessionId: ' + JSON.stringify(ev));
        assert(/^[0-9a-f-]{36}$/i.test(sid), 'sessionId not a uuid: ' + sid);
        assert(data.agentId === agentId, 'agentId not echoed: ' + JSON.stringify(ev));
        console.log('  ✓ create_conversation_session -> sessionId=' + sid);

        console.log('PROTOCOL SMOKE PASSED');
        ws.close();
        process.exit(0);
    } catch (e) {
        console.error(e.message || String(e));
        process.exit(1);
    }
});
JS
}

run_smoke_websocat() {
    # websocat: text-mode duplex. Feed both requests on stdin (one JSON object
    # per line, text mode) and read the responses back, asserting on the raw
    # JSON lines. -n1 caps it to a single line-of-responses window; we give the
    # server a moment to answer before stdin EOF closes the socket.
    local out
    out="$(printf '%s\n%s\n' \
        "{\"action\":\"ping\",\"requestId\":\"1\"}" \
        "{\"action\":\"create_conversation_session\",\"requestId\":\"2\",\"agentId\":\"${AGENT_ID}\",\"userName\":\"Smoke\"}" \
        | websocat --text --ping-interval 5 "${WS_URL}" 2>/dev/null)" || true

    grep -q '"type":"pong"' <<<"${out}" || die "websocat: no pong in response:\n${out}"
    grep -q '"requestId":"1"' <<<"${out}" || die "websocat: pong requestId mismatch:\n${out}"
    grep -q '"type":"immediate_response"' <<<"${out}" || die "websocat: no immediate_response in response:\n${out}"
    grep -q "\"agentId\":\"${AGENT_ID}\"" <<<"${out}" || die "websocat: agentId not echoed:\n${out}"
    grep -qE '"sessionId":"[0-9a-fA-F-]{36}"' <<<"${out}" || die "websocat: no uuid sessionId:\n${out}"
    printf '  ✓ ping -> pong\n  ✓ create_conversation_session -> immediate_response\nPROTOCOL SMOKE PASSED\n'
}

case "${WS_CLIENT}" in
    python)   run_smoke_python ;;
    node)     run_smoke_node ;;
    websocat) run_smoke_websocat ;;
    *)        die "no usable WebSocket client" ;;
esac

log "✅ kind deployment smoke PASSED — chart serves the protocol over a live pod"
# cleanup() runs on EXIT (teardown / cluster delete).
