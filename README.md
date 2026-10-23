# CodecTrans

Self-hosted media transcoding with a central server + distributed worker nodes (Tdarr-style).
Upload a video, pick a codec (logical like `h264`/`av1`, or a specific HW encoder), and it is
transcoded on the best available accelerator — NVENC (dGPU), QSV / VA-API (Intel iGPU) — with
per-device capability reporting, capped-VBR ("% of source") or CRF bitrate modes.

```
CodecTrans/
├── CodecTrans-server/   # central server: Rust (axum) API + React UI, one port (:8400)
│   ├── backend/         #   Rust: uploads, jobs, node registry, dispatch, transcode
│   └── frontend/        #   React (Vite) — dist/ is committed, no node/npm needed to run
└── CodecTrans-node/     # worker node: registers with a server, runs ffmpeg jobs locally
```

## Quick start (test machine)

Requires: `ffmpeg` (8.x tested), `cargo`, and for Intel iGPU HW accel the QSV stack
(`libmfx-gen1.2` + `libvpl-tools` + `intel-media-va-driver`, account in `video`+`render` groups).

```sh
git clone <repo> && cd CodecTrans

# server (+ local node) — builds if needed, serves UI at :8400
./CodecTrans-server/start-all.sh

# or node only (on another machine), then feed its IP:port to the server's "Search" box
CT_SERVER=http://<server>:8400 ./CodecTrans-node/run.sh
```

- UI: `http://<server>:8400`
- server API: `:8400/api/health` · node local API: `:8401/api/health`
- node bind: `CT_BIND=local|all` (default `all`), `CT_NODE_NAME`, `CT_PORT`

## Notes

- The node probes every (device, backend, encoder) combination at startup and reports
  accelerators per physical device: `GPU #0 (nvenc)`, `CPU #0 (qsv)`, `CPU #0 (vaapi)`, …
- The server auto-rewrites logical codecs to the node's best working HW encoder
  (nvenc > vaapi > qsv > software) and records `effectiveVideoCodec` + `accelerated` per job.
- `frontend/dist` is committed so a clone runs without a Node toolchain; rebuild it with
  `npm run build` in `CodecTrans-server/frontend/` when changing the UI.
