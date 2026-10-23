# CodecTrans

> **고지 (Notice)**
> - 본 프로젝트의 코드는 **Qwen3.8 27B** 모델의 도움을 받아 작성되었습니다.
> - 저자의 Rust 지식이 쌓이는 대로 **재설계**할 예정입니다.
> - 본 프로젝트는 **비영리** 목적입니다. 대한민국 「군인의 지위 및 복무에 관한 기본법」 제30조(영리행위 및 겸직 금지)에 따라 군인은 군무 외의 영리 업무 종사 및 국방부 장관 허가 없는 겸직이 금지되어 있으며, 본 프로젝트는 이에 준해 영리화되지 않습니다.
>
> *This project was written with the assistance of the Qwen3.8 27B model, is non-commercial, and will be redesigned as the author's Rust expertise grows.*

Self-hosted media transcoding with a central server + distributed worker nodes (Tdarr-style).
Upload a video, pick a codec (logical like `h264`/`av1`, or a specific HW encoder), and it is
transcoded on the best available accelerator — NVENC (dGPU), QSV / VA-API (Intel iGPU),
VideoToolbox (Apple) — with per-device capability reporting, capped-VBR ("% of source") or CRF
bitrate modes.

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
  (nvenc > vaapi > qsv > amf > videotoolbox > software) and records `effectiveVideoCodec` + `accelerated` per job.
- `frontend/dist` is committed so a clone runs without a Node toolchain; rebuild it with
  `npm run build` in `CodecTrans-server/frontend/` when changing the UI.

## Per-OS support

| | server | node HW accel | status |
|---|---|---|---|
| **Linux** | any distro | NVENC (dGPU), QSV + VA-API (Intel iGPU, needs `libmfx-gen1.2` + `libvpl-tools` + `intel-media-va-driver`, account in `video`+`render`) | ✅ verified |
| **Windows** | any | NVENC (NVIDIA), QSV (Intel iGPU, oneVPL), AMF (AMD) — ffmpeg from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) (full build), start with `run.bat` | ⚠️ **미확인** (unverified — cross-compile check only, not yet run on a real Windows box) |
| **macOS** | any (Apple Silicon OK) | VideoToolbox via `brew install ffmpeg` — ffmpeg ≥ 9 names encoders `h264_videotoolbox`/`hevc_videotoolbox`, older builds `h264_vt`/`hevc_vt` (both detected) | ✅ verified (Apple Silicon, macOS 27, ffmpeg 9.0.2) |

- No HW device? The node simply reports none and the server falls back to software encoders — the service still works everywhere.
- The `sg render` / group setup is Linux-only (VA-API device access); Windows/macOS need nothing extra.
- **Headless macOS quirk:** on a headless Mac (no console session), processes reparented out of
  their SSH session (nohup / tmux / launchd) lose LAN routing (`No route to host`). Run the node
  in the foreground of a persistent SSH connection from the server instead —
  `CodecTrans-node/mac-supervisor.sh` does exactly that (connect, `exec` the node, reconnect on
  exit). Docker avoids the quirk but a Linux container cannot reach VideoToolbox, so it is the
  wrong vehicle for a Mac node.
