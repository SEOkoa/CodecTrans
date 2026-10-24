# CodecTrans server — build context = repository root
#   docker build -f docker/server.Dockerfile -t codectrans/server .
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
# lockfile first for layer caching, then the crate
COPY CodecTrans-server/backend/Cargo.toml CodecTrans-server/backend/Cargo.lock ./
COPY CodecTrans-server/backend/src ./src
RUN cargo build --release

# Runtime: slim Debian + ffmpeg (ffprobe = scan + post-encode health check,
# ffmpeg = the server-local transcode fallback path)
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
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
