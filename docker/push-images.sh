#!/usr/bin/env bash
# Build + push the CodecTrans server/node images to a registry.
#   docker/push-images.sh <registry-user> [tag]   -> Docker Hub
#   docker/push-images.sh <gh-user> latest ghcr   -> GHCR (public)
#
# Examples:
#   docker/push-images.sh seokoa                 # hub.docker.io/seokoa/codectrans-{server,node}:latest
#   docker/push-images.sh seokoa 2026.10 ghcr    # ghcr.io/seokoa/codectrans/codectrans-{server,node}:2026.10
#
# On the NAS afterwards:
#   Docker Hub:  set `image: docker.io/<user>/codectrans-server:latest` (+ node)
#                in docker/docker-compose.yml and `docker compose up -d`.
#   GHCR:        docker pull ghcr.io/<user>/codectrans/codectrans-server:<tag>
#                docker tag  ghcr.io/<user>/codectrans/codectrans-server:<tag> codectrans/server:latest
#                (same for node) — the stock compose works unchanged.

set -euo pipefail
cd "$(dirname "$0")/.."

USER="${1:?usage: push-images.sh <registry-user> [tag] [hub|ghcr]}"
TAG="${2:-latest}"
REG="${3:-hub}"

if [ "$REG" = "ghcr" ]; then
  S="ghcr.io/$USER/codectrans/codectrans-server:$TAG"
  N="ghcr.io/$USER/codectrans/codectrans-node:$TAG"
else
  S="docker.io/$USER/codectrans-server:$TAG"
  N="docker.io/$USER/codectrans-node:$TAG"
fi

echo "==> building server image"
docker build -f docker/server.Dockerfile -t "$S" .
echo "==> pushing $S"
docker push "$S"

echo "==> building node image"
docker build -f docker/node.Dockerfile -t "$N" .
echo "==> pushing $N"
docker push "$N"

echo
echo "Done. On the NAS:"
if [ "$REG" = "ghcr" ]; then
  echo "  docker pull $S && docker tag $S codectrans/server:latest"
  echo "  docker pull $N && docker tag $N codectrans/node:latest"
else
  echo "  set in docker/docker-compose.yml:"
  echo "    server:  image: $S"
  echo "    node:    image: $N"
fi
echo "  docker compose -f docker/docker-compose.yml up -d"
