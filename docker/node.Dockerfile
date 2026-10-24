# CodecTrans node — build context = repository root
#   docker build -f docker/node.Dockerfile -t codectrans/node .
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
COPY CodecTrans-node/Cargo.toml CodecTrans-node/Cargo.lock ./
COPY CodecTrans-node/src ./src
RUN cargo build --release

# Runtime: Ubuntu 24.04 + ffmpeg 6.1 (nvenc incl. AV1 on Ada/Blackwell GPUs,
# h264/hevc nvenc on the RTX 30-series). The node does NOT need access to
# the media itself — it streams sources from the server and uploads results.
# GPU: use the nvidia docker runtime (see docker/docker-compose.yml) — the
# host driver's libraries are injected, no CUDA toolkit needed in the image.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/ct
COPY --from=build /src/target/release/CodecTransNode /opt/ct/CodecTransNode
ENV CT_WORKDIR=/opt/ct/work \
    CT_PORT=8401 \
    CT_BIND=all
VOLUME ["/opt/ct/work"]
EXPOSE 8401
ENTRYPOINT ["/opt/ct/CodecTransNode"]
