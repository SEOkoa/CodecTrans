# CodecTrans server — build context = repository root
#   docker build -f docker/server.Dockerfile -t codectrans/server .
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
# lockfile first for layer caching, then the crate
COPY CodecTrans-server/backend/Cargo.toml CodecTrans-server/backend/Cargo.lock ./
COPY CodecTrans-server/backend/src ./src
RUN cargo build --release

# Runtime: Ubuntu 24.04 + ffmpeg 6.1 (nvenc incl. AV1 on Ada/Blackwell,
# ffprobe for scan + post-encode health check, ffmpeg for the server-local
# transcode fallback path) + bundled NVIDIA driver userland (same stage as
# the node image — a NAS passes /dev/nvidia* but does NOT inject the host
# driver userland, so the scan's GPU health decode needs the matching
# series inside the container).
FROM ubuntu:24.04 AS nvidia-userland
RUN set -x; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl dpkg; \
    cd /tmp; \
    mkdir -p /opt/nvidia-userland/575/lib /opt/nvidia-userland/575/bin; \
    curl -sf -o /tmp/575.run https://us.download.nvidia.com/XFree86/Linux-x86_64/575.64.05/NVIDIA-Linux-x86_64-575.64.05.run; \
    sh /tmp/575.run --extract-only --target /tmp/575-ex; \
    for f in libcuda.so* libnvidia-encode.so* libnvidia-ml.so* libnvidia-cfg.so* libnvcuvid.so*; do \
        cp -a /tmp/575-ex/$f /opt/nvidia-userland/575/lib/; \
    done; \
    cp -a /tmp/575-ex/nvidia-smi /opt/nvidia-userland/575/bin/; \
    (cd /opt/nvidia-userland/575/lib && \
        ln -sf libcuda.so.575.64.05 libcuda.so.1 && \
        ln -sf libnvidia-encode.so.575.64.05 libnvidia-encode.so.1 && \
        ln -sf libnvidia-ml.so.575.64.05 libnvidia-ml.so.1 && \
        ln -sf libnvidia-cfg.so.575.64.05 libnvidia-cfg.so.1 && \
        ln -sf libnvcuvid.so.575.64.05 libnvcuvid.so.1); \
    rm -rf /tmp/575-ex /tmp/575.run; \
    for v in 535 525; do \
        mkdir -p /opt/nvidia-userland/$v/lib /opt/nvidia-userland/$v/bin; \
        ok=1; \
        for p in libnvidia-compute-$v libnvidia-encode-$v libnvidia-decode-$v nvidia-utils-$v; do \
            apt-get download $p >/dev/null 2>&1 || ok=0; \
        done; \
        if [ "$ok" = "1" ]; then \
            mkdir -p /tmp/ex-$v; \
            for d in *.deb; do dpkg -x "$d" /tmp/ex-$v/; done; \
            for f in libcuda.so.1* libnvidia-encode.so.1* libnvidia-ml.so.1* libnvidia-cfg.so.1* libnvcuvid.so.1*; do \
                cp -a /tmp/ex-$v/usr/lib/x86_64-linux-gnu/$f /opt/nvidia-userland/$v/lib/ 2>/dev/null || true; \
            done; \
            cp -a /tmp/ex-$v/usr/bin/nvidia-smi /opt/nvidia-userland/$v/bin/ 2>/dev/null || true; \
        fi; \
        rm -rf /tmp/ex-$v /tmp/*.deb; \
    done
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# Bundled driver userland for the GPU health decode (see stage above).
COPY --from=nvidia-userland /opt/nvidia-userland /opt/nvidia-userland
WORKDIR /opt/ct
COPY --from=build /src/target/release/CodecTrans /opt/ct/CodecTrans
# built React app — the container serves it; CT_BASE stays a pure data dir
COPY CodecTrans-server/frontend/dist /opt/ct/frontend/dist
ENV CT_BASE=/data \
    CT_FRONTEND=/opt/ct/frontend/dist \
    CT_PORT=8400
VOLUME ["/data"]
EXPOSE 8400
ENTRYPOINT ["/opt/ct/CodecTrans"]
