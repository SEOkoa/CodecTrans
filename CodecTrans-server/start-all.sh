#!/usr/bin/env bash
# Build everything and start the central server + one local node.
#   server  -> http://localhost:8400  (React UI + API)
#   node    -> local API http://localhost:8401
# Logs go to /tmp/ct_server.log and /tmp/ct_node.log.
# Override with CT_NODE_NAME / CT_PORT / CT_SERVER if you want a different node.
set -euo pipefail
cd "$(dirname "$0")"

NODE_BIN="${NODE_BIN:-/usr/local/bin}"
command -v npm >/dev/null 2>&1 || export PATH="$NODE_BIN:$PATH"

echo "==> building frontend (skipped when frontend/dist is already built)"
if [ -d frontend/dist/assets ]; then
  echo "    frontend/dist present — skipping"
else
  (cd frontend && npm install --silent && npm run build)
fi

echo "==> building server"
(cd backend && cargo build --release)

echo "==> building node"
(cd ../CodecTrans-node && cargo build --release)

# stop any previous instances
for port in 8400 "${CT_PORT:-8401}"; do
  PID=$(ss -tlnp 2>/dev/null | grep ":$port " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
done
sleep 1

echo "==> starting server on :8400"
nohup ./backend/target/release/CodecTrans > /tmp/ct_server.log 2>&1 &
sleep 2

echo "==> starting node (CT_SERVER=${CT_SERVER:-http://localhost:8400}, CT_PORT=${CT_PORT:-8401})"
( cd ../CodecTrans-node && nohup ./target/release/CodecTransNode > /tmp/ct_node.log 2>&1 & )
sleep 2

echo
echo "  UI:      http://localhost:8400"
echo "  server:  http://localhost:8400/api/health"
echo "  node:    http://localhost:${CT_PORT:-8401}/api/health"
echo "  logs:    /tmp/ct_server.log  /tmp/ct_node.log"
