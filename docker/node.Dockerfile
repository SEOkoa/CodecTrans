# CodecTrans node — build context = repository root
#   docker build -f docker/node.Dockerfile -t codectrans/node .
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
COPY CodecTrans-node/Cargo.toml CodecTrans-node/Cargo.lock ./
COPY CodecTrans-node/src ./src
RUN cargo build --release

# Bundled NVIDIA driver *userland* series (575, 535, 525 — extracted, NOT
# installed, so they can coexist with the system 545 libs):
#
#   /opt/nvidia-userland/<series>/lib  libcuda.so.1, libnvidia-encode.so.1,
#                                       libnvidia-ml.so.1, libnvidia-cfg.so.1
#   /opt/nvidia-userland/<series>/bin  nvidia-smi
#
# Why several series: a NAS (e.g. QNAP) passes /dev/nvidia* into the
# container but does NOT inject the host driver userland. The node reads
# the host kernel driver version from /sys/module/nvidia/version (visible
# from inside), tries that series first, and if it isn't bundled fetches
# it at runtime (best effort) — a version-skewed userland fails cuInit
# with CUDA_ERROR_SYSTEM_DRIVER_MISMATCH. Only the files ffmpeg's NVENC
# path + nvidia-smi actually need are copied — no ptxjit/kernel modules.
# Sources: 575 straight from NVIDIA's CUDA repo (Ubuntu ships 575 only as
# a transitional package pointing at 580 — no real 575 .so files); 535/525
# from Ubuntu jammy (older NAS firmware).
FROM ubuntu:22.04 AS nvidia-userland
RUN set -x; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl dpkg; \
    cd /tmp; \
    # ── 575 (NVIDIA CUDA repo, 575.57.08 — any 575.x host driver works) ──
    mkdir -p /opt/nvidia-userland/575/lib /opt/nvidia-userland/575/bin /opt/nvidia-userland/575/ex; \
    U=https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/; \
    curl -sf -o 575-compute.deb $U/libnvidia-compute-575_575.57.08-0ubuntu1_amd64.deb; \
    curl -sf -o 575-encode.deb  $U/libnvidia-encode-575_575.57.08-0ubuntu1_amd64.deb; \
    curl -sf -o 575-cfg.deb     $U/libnvidia-cfg1-575_575.57.08-0ubuntu1_amd64.deb; \
    curl -sf -o 575-ml.deb      $U/nvidia-compute-utils-575_575.57.08-0ubuntu1_amd64.deb; \
    curl -sf -o 575-smi.deb     $U/nvidia-utils-575_575.57.08-0ubuntu1_amd64.deb; \
    for d in 575-*.deb; do dpkg -x "$d" /opt/nvidia-userland/575/ex/; done; \
    for f in libcuda.so* libnvidia-encode.so* libnvidia-ml.so* libnvidia-cfg.so*; do \
        cp -a /opt/nvidia-userland/575/ex/usr/lib/x86_64-linux-gnu/$f /opt/nvidia-userland/575/lib/; \
    done; \
    cp -a /opt/nvidia-userland/575/ex/usr/bin/nvidia-smi /opt/nvidia-userland/575/bin/; \
    echo "series 575 (NVIDIA CUDA repo 575.57.08):"; ls -la /opt/nvidia-userland/575/lib/; \
    rm -rf /opt/nvidia-userland/575/ex /tmp/575-*.deb; \
    # ── 535 / 525 (Ubuntu jammy — older NAS firmware) ──
    for v in 535 525; do \
        mkdir -p /opt/nvidia-userland/$v/lib /opt/nvidia-userland/$v/bin; \
        ok=1; \
        for p in libnvidia-compute-$v libnvidia-encode-$v nvidia-utils-$v; do \
            apt-get download $p >/dev/null 2>&1 || ok=0; \
        done; \
        if [ "$ok" = "1" ]; then \
            mkdir -p /tmp/ex-$v; \
            for d in *.deb; do dpkg -x "$d" /tmp/ex-$v/; done; \
            for f in libcuda.so.1* libnvidia-encode.so.1* libnvidia-ml.so.1* libnvidia-cfg.so.1*; do \
                cp -a /tmp/ex-$v/usr/lib/x86_64-linux-gnu/$f /opt/nvidia-userland/$v/lib/ 2>/dev/null || true; \
            done; \
            cp -a /tmp/ex-$v/usr/bin/nvidia-smi /opt/nvidia-userland/$v/bin/ 2>/dev/null || true; \
            echo "series $v extracted:"; ls -la /opt/nvidia-userland/$v/lib/ | head -20; \
        else \
            echo "series $v not in repos — skipping"; \
        fi; \
        rm -rf /tmp/ex-$v /tmp/*.deb; \
    done; \
    rm -rf /var/lib/apt/lists/*

# Runtime: Ubuntu 22.04 + ffmpeg 4.4 (h264/hevc NVENC — everything the
# RTX 30-series can do; AV1 NVENC doesn't exist on Ampere anyway) +
# the 545 driver userland as the SYSTEM fallback (works with host drivers
# >= 545). The /opt/nvidia-userland series (575/535/525) cover NEWER and
# OLDER host drivers — the node picks the matching one at startup (see
# above) and fetches anything missing from the repos at runtime.
#
#   - GPU-self-contained: plain /dev/nvidia* device passthrough (QNAP's
#     privileged+devices pattern) is enough — no nvidia docker runtime
#   - on hosts WITH the nvidia runtime, the injected host libs take
#     precedence and the built-in ones are simply shadowed
#
# The node does NOT need access to the media itself — it streams sources
# from the server and uploads results.
FROM ubuntu:22.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates curl \
        libnvidia-encode-545 libnvidia-compute-545 nvidia-utils-545 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=nvidia-userland /opt/nvidia-userland /opt/nvidia-userland
WORKDIR /opt/ct
COPY --from=build /src/target/release/CodecTransNode /opt/ct/CodecTransNode
ENV CT_WORKDIR=/opt/ct/work \
    CT_PORT=8401 \
    CT_BIND=all
VOLUME ["/opt/ct/work"]
EXPOSE 8401
ENTRYPOINT ["/opt/ct/CodecTransNode"]
