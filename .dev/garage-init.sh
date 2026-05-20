#!/bin/sh
# One-shot Garage bootstrap: wait for the daemon, assign cluster layout
# (idempotent), create the bucket + static dev key, grant access. Re-runs
# are safe — every step is a no-op once already applied.
set -eu

export GARAGE_RPC_SECRET=1799bccfd7411eddcf9ebd1e1d2c3e9b1799bccfd7411eddcf9ebd1e1d2c3e9b

BUCKET="${BUCKET:-rawdb}"
KEY_ID="${KEY_ID:-GKDEV0000000000000000000}"
KEY_SECRET="${KEY_SECRET:-dev-secret-key-dev-secret-key-dev-secret-}"
KEY_NAME="dev"

RPC_ADDR="${RPC_ADDR:-garage:3901}"
ADMIN_URL="${ADMIN_URL:-http://garage:3902}"
ADMIN_TOKEN="${ADMIN_TOKEN:-dev-admin-token}"

# Garage v2's CLI demands a full RPC peer identifier
# (<64-hex node id>@host:port); a bare host:port is rejected. Running in a
# separate container we have no local node key, so discover the running
# node's id over the admin API (always reachable once the daemon binds,
# even before any layout is applied) and build the peer string from it.
echo "[init] waiting for garage admin API at $ADMIN_URL..."
NODE_ID=""
last_code=""
last_body=""
for i in $(seq 1 60); do
    for path in /v2/GetClusterStatus /v1/status; do
        # -w prints the HTTP code on its own last line; body precedes it.
        resp=$(curl -s -m 5 -w '\n%{http_code}' \
            -H "Authorization: Bearer $ADMIN_TOKEN" \
            "$ADMIN_URL$path" 2>/dev/null) || resp=""
        last_code=$(printf '%s' "$resp" | tail -n1)
        last_body=$(printf '%s' "$resp" | sed '$d')
        [ "$last_code" = "200" ] || continue
        # The answering node's full id is the only 64-hex token in the
        # response (single-node dev cluster). Prefer an explicit
        # "node"/"id" field, but fall back to any 64-hex run so we're not
        # brittle against admin-API schema churn across Garage versions.
        NODE_ID=$(printf '%s' "$last_body" \
            | grep -oE '"(node|id)"[[:space:]]*:[[:space:]]*"[0-9a-f]{64}"' \
            | grep -oE '[0-9a-f]{64}' | head -n1)
        [ -n "$NODE_ID" ] || NODE_ID=$(printf '%s' "$last_body" \
            | grep -oE '[0-9a-f]{64}' | head -n1)
        [ -n "$NODE_ID" ] && break
    done
    [ -n "$NODE_ID" ] && break
    sleep 1
done
if [ -z "$NODE_ID" ]; then
    echo "[init] ERROR: could not determine garage node id from $ADMIN_URL" >&2
    echo "[init]   last HTTP code: ${last_code:-<none, connection failed>}" >&2
    echo "[init]   last response body: ${last_body:-<empty>}" >&2
    exit 1
fi

export GARAGE_RPC_HOST="$NODE_ID@$RPC_ADDR"
echo "[init] garage node $NODE_ID — RPC host $GARAGE_RPC_HOST"
/garage status >/dev/null

# Layout: assign + apply if not yet applied. A fresh Garage shows the node
# under "HEALTHY NODES" but with no role; once `layout apply` ran, every
# node has a role assigned.
if /garage layout show 2>&1 | grep -q "No nodes currently have a role"; then
    NODE_ID=$(/garage status | awk '/^[a-f0-9]{16}/ {print $1; exit}')
    echo "[init] assigning layout to node $NODE_ID"
    /garage layout assign -z dc1 -c 1G "$NODE_ID"
    /garage layout apply --version 1
else
    echo "[init] layout already applied"
fi

# Bucket
if /garage bucket list 2>/dev/null | awk 'NR>1 {print $3}' | grep -qx "$BUCKET"; then
    echo "[init] bucket $BUCKET exists"
else
    echo "[init] creating bucket $BUCKET"
    /garage bucket create "$BUCKET"
fi

# Static key (idempotent import).
if /garage key list 2>/dev/null | awk 'NR>1 {print $1}' | grep -qx "$KEY_ID"; then
    echo "[init] key $KEY_ID already imported"
else
    echo "[init] importing key $KEY_ID"
    /garage key import --yes "$KEY_ID" "$KEY_SECRET" -n "$KEY_NAME"
fi

# Allow the key to read/write the bucket. Re-applying is a no-op.
/garage bucket allow --read --write --owner "$BUCKET" --key "$KEY_ID" >/dev/null

echo "[init] done"
