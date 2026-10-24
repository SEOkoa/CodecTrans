# CodecTrans node — build context = repository root
#   docker build -f docker/node.Dockerfile -t codectrans/node .
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
COPY CodecTrans-node/Cargo.toml CodecTrans-node/Cargo.lock ./
COPY CodecTrans-node/src ./src
RUN cargo build --release

# Runtime: Ubuntu 22.04 + ffmpeg 4.4 (h264/hevc NVENC — everything the
# RTX 30-series can do; AV1 NVENC doesn't exist on Ampere anyway) +
# the NVIDIA driver *userland* libraries (libcuda.so.1 via
# libnvidia-compute, libnvidia-encode, nvidia-smi) from the 545 series
# (CUDA 12.3 — the oldest series still shipped as a real package on
# jammy; everything older is a transitional stub):
#
#   - works with any host driver >= 545 (NVIDIA forward-compat: newer
#     kernel driver + older userland libs), which covers every modern
#     QNAP driver for the 3050
#   - makes the image GPU-self-contained: with plain /dev/nvidia* device
#     passthrough (QNAP's privileged+devices pattern) NVENC works WITHOUT
#     the nvidia docker runtime — the host driver's libraries are not
#     injected, the image carries its own
#   - on hosts WITH the nvidia runtime, the injected host libs take
#     precedence and the built-in ones are simply shadowed
#
# The node does NOT need access to the media itself — it streams sources
# from the server and uploads results.
FROM ubuntu:22.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates \
        libnvidia-encode-545 libnvidia-compute-545 nvidia-utils-545 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/ct
COPY --from=build /src/target/release/CodecTransNode /opt/ct/CodecTransNode
ENV CT_WORKDIR=/opt/ct/work \
    CT_PORT=8401 \
    CT_BIND=all
VOLUME ["/opt/ct/work"]
EXPOSE 8401
ENTRYPOINT ["/opt/ct/CodecTransNode"]
