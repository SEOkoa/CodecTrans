#!/usr/bin/env bash
# Build the frontend + backend, then start the single server on :8400.
set -euo pipefail
cd "$(dirname "$0")"

NODE="${NODE:-/usr/local/bin}"
# make sure npm/node are found
if ! command -v npm >/dev/null 2>&1; then
  export PATH="$NODE:$PATH"
fi

echo "==> building frontend (skipped when frontend/dist is already built)"
if [ -d frontend/dist/assets ]; then
  echo "    frontend/dist present — skipping"
else
  (cd frontend && npm install --silent && npm run build)
fi

echo "==> building backend"
(cd backend && cargo build --release)

echo "==> starting CodecTrans on :8400"
exec ./backend/target/release/CodecTrans
