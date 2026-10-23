#!/usr/bin/env bash
# Build + run a CodecTrans worker node.
# Env:
#   CT_SERVER     central server URL  (default http://localhost:8400, or "auto" to mDNS-discover)
#   CT_NODE_NAME  node name           (default: your OS account name)
#   CT_PORT       local API port      (default 8401)
#   CT_BIND       what to listen on:
#                   local  -> 127.0.0.1 (this machine only)
#                   all    -> 0.0.0.0   (reachable on the LAN)  [default]
#
# On start the node prints its IP:port, e.g.:
#     LAN: http://192.168.x.x:8401  <- give this to the server
# Feed that IP:port into the server's "Search" box (or set CT_SERVER on the
# node so it connects itself).
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release

echo "==> starting CodecTrans node"
echo "    CT_SERVER   = ${CT_SERVER:-http://localhost:8400}"
echo "    CT_NODE_NAME= ${CT_NODE_NAME:-<account name>}"
echo "    CT_PORT     = ${CT_PORT:-8401}"
echo "    CT_BIND     = ${CT_BIND:-all}"
exec ./target/release/CodecTransNode
