# CodecTrans node — build context = repository root
#   docker build -f docker/node.Dockerfile -t codectrans/node .
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
COPY CodecTrans-node/Cargo.toml CodecTrans-node/Cargo.lock ./
COPY CodecTrans-node/src ./src
RUN cargo build --release

# Bundled NVIDIA driver *userland* series (580, 575, 535, 525 — extracted, NOT
# installed, so they can coexist with the system 545 libs):
#
#   /opt/nvidia-userland/<series>/lib  libcuda.so.1, libnvidia-encode.so.1,
#                                       libnvidia-ml.so.1, libnvidia-cfg.so.1,
#                                       libnvcuvid.so.1
#   /opt/nvidia-userland/<series>/bin  nvidia-smi
#
# NOTE: libnvcuvid MUST be bundled with each series. Since 575,
# libnvidia-encode.so.1 has a hard DT_NEEDED on libnvcuvid.so.1; without
# a same-series copy on the LD path it resolves to a STALE system one
# (e.g. the 545 fallback below) and NvEncOpenEncodeSessionEx dies with
# "unsupported device (2)" — cuInit still passes, so the failure is easy
# to misread as a host problem.
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
    # ── 575 (official NVIDIA .run, 575.64.05 — the QNAP host driver).
    #    The CUDA repo only carries 575.57.08, and an OLDER userland than
    #    the host kernel (575.57.08 < 575.64.05) fails with
    #    SYSTEM_DRIVER_MISMATCH — the exact point release is required. ──
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
    echo "series 575 (official .run 575.64.05):"; ls -la /opt/nvidia-userland/575/lib/; \
    rm -rf /tmp/575-ex /tmp/575.run; \
    # ── 580 (official NVIDIA .run, 580.159.04 — the current QNAP host driver,
    #    QuTS hero h5.2.11. Same rule as 575: exact point release, extracted
    #    not installed, libnvcuvid included). ──
    mkdir -p /opt/nvidia-userland/580/lib /opt/nvidia-userland/580/bin; \
    curl -sf -o /tmp/580.run https://us.download.nvidia.com/XFree86/Linux-x86_64/580.159.04/NVIDIA-Linux-x86_64-580.159.04.run; \
    sh /tmp/580.run --extract-only --target /tmp/580-ex; \
    for f in libcuda.so* libnvidia-encode.so* libnvidia-ml.so* libnvidia-cfg.so* libnvcuvid.so*; do \
        cp -a /tmp/580-ex/$f /opt/nvidia-userland/580/lib/; \
    done; \
    cp -a /tmp/580-ex/nvidia-smi /opt/nvidia-userland/580/bin/; \
    (cd /opt/nvidia-userland/580/lib && \
        ln -sf libcuda.so.580.159.04 libcuda.so.1 && \
        ln -sf libnvidia-encode.so.580.159.04 libnvidia-encode.so.1 && \
        ln -sf libnvidia-ml.so.580.159.04 libnvidia-ml.so.1 && \
        ln -sf libnvidia-cfg.so.580.159.04 libnvidia-cfg.so.1 && \
        ln -sf libnvcuvid.so.580.159.04 libnvcuvid.so.1); \
    echo "series 580 (official .run 580.159.04):"; ls -la /opt/nvidia-userland/580/lib/; \
    rm -rf /tmp/580-ex /tmp/580.run; \
    # ── 535 / 525 (Ubuntu jammy — older NAS firmware) ── \
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
            echo "series $v extracted:"; ls -la /opt/nvidia-userland/$v/lib/ | head -20; \
        else \
            echo "series $v not in repos — skipping"; \
        fi; \
        rm -rf /tmp/ex-$v /tmp/*.deb; \
    done; \
    # ── 545 (system fallback for the noble runtime base — noble's repos \
    #    don't ship the 545 series; the .so files go straight to the \
    #    runtime's default linker path) ── \
    mkdir -p /opt/nvidia-545; \
    cd /tmp; \
    for p in libnvidia-compute-545 libnvidia-encode-545 nvidia-utils-545; do \
        apt-get download $p >/dev/null 2>&1 || true; \
    done; \
    for d in *.deb; do dpkg -x "$d" /tmp/ex-545 >/dev/null 2>&1 || true; done; \
    for f in libcuda.so* libnvidia-encode.so* libnvidia-ml.so* libnvidia-cfg.so*; do \
        cp -a /tmp/ex-545/usr/lib/x86_64-linux-gnu/$f /opt/nvidia-545/ 2>/dev/null || true; \
    done; \
    echo "545 fallback extracted:"; ls -la /opt/nvidia-545/ 2>/dev/null | head -20; \
    rm -rf /tmp/ex-545 /tmp/*.deb; \
    rm -rf /var/lib/apt/lists/*

# Runtime: Ubuntu 24.04 + ffmpeg 6.1 (h264/hevc/AV1 NVENC — av1_nvenc needs
# ffmpeg >= 5.1, so the 22.04/ffmpeg-4.4 base could never do it; 6.1 also
# gives proper -hwaccel cuda cuvid->nvenc zero-copy pipelines) + the 545
# driver userland as the SYSTEM fallback (works with host drivers >= 545;
# noble's repos don't ship the 545 series, so the .so files are extracted
# from the jammy packages in the nvidia-userland stage below). The
# /opt/nvidia-userland series (575/535/525) cover NEWER and OLDER host
# drivers — the node picks the matching one at startup (see above) and
# fetches anything missing from the repos at runtime.
#
#   - GPU-self-contained: plain /dev/nvidia* device passthrough (QNAP's
#     privileged+devices pattern) is enough — no nvidia docker runtime
#   - on hosts WITH the nvidia runtime, the injected host libs take
#     precedence and the built-in ones are simply shadowed
#
# The node does NOT need access to the media itself — it streams sources
# from the server and uploads results.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# 545-series userland as the default-path fallback (see comment above).
COPY --from=nvidia-userland /opt/nvidia-545 /usr/lib/nvidia-545
RUN for f in /usr/lib/nvidia-545/lib*.so*; do \
        ln -sf "/usr/lib/nvidia-545/$(basename "$f")" "/usr/lib/x86_64-linux-gnu/$(basename "$f")"; \
    done
COPY --from=nvidia-userland /opt/nvidia-userland /opt/nvidia-userland
WORKDIR /opt/ct
COPY --from=build /src/target/release/CodecTransNode /opt/ct/CodecTransNode
ENV CT_WORKDIR=/opt/ct/work \
    CT_PORT=8401 \
    CT_BIND=all
VOLUME ["/opt/ct/work"]
EXPOSE 8401
ENTRYPOINT ["/opt/ct/CodecTransNode"]
