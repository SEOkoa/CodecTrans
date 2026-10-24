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

Requires: `ffmpeg` (8.x tested), `cargo`, a **Redis** server for the node registry
(default `127.0.0.1:6379`, override with `CT_REDIS`), and for Intel iGPU HW accel the QSV stack
(`libmfx-gen1.2` + `libvpl-tools` + `intel-media-va-driver`, account in `video`+`render` groups).

```sh
git clone <repo> && cd CodecTrans

# Redis (node registry) — one container is enough
docker run -d --name ct-redis --restart unless-stopped -p 127.0.0.1:6379:6379 redis:7-alpine

# server (+ local node) — builds if needed, serves UI at :8400
./CodecTrans-server/start-all.sh

# or node only (on another machine), then feed its IP:port to the server's "Search" box
CT_SERVER=http://<server>:8400 ./CodecTrans-node/run.sh
```

- UI: `http://<server>:8400`
- server API: `:8400/api/health` · node local API: `:8401/api/health`
- node bind: `CT_BIND=local|all` (default `all`), `CT_NODE_NAME`, `CT_PORT`

## Installing a node (wizard)

`CodecTrans-node/install-node.sh` walks a new machine through the whole setup —
server address, display name, bind, concurrent jobs, **GPU selection** (pins
`CUDA_VISIBLE_DEVICES` on multi-GPU boxes), and an **optional NFS share**
(mounts it and writes `/etc/fstab` so the mount survives reboots). It then
starts the node (nohup by default, `--daemon` for a systemd unit on Linux)
and **verifies** the local API and the node's registration on the server:

```sh
# interactive (asks each question)
./install-node.sh

# unattended
./install-node.sh --server http://192.168.x.x:8400 --name "NAS node" \
    --jobs 2 --gpus all --nfs nas:/exports/videos /mnt/videos --yes
```

On macOS the wizard still works, but remember the headless-SSH routing quirk
(see below): for a reboot-proof Mac node, run it inside the server's
`mac-supervisor.sh` SSH loop instead.

## Notes

- The **node registry lives in Redis** (`ct:node:*` keys): node identity (UUID) survives
  server restarts — queued jobs are never orphaned — and offline nodes are tracked with
  their last-seen time. The UI hides offline nodes by default (`Online / Offline / All`
  toggle) and can filter by HW-accel backend (nvenc/qsv/vaapi/…) or encoder codec
  family (h264/h265/av1/…); the same filters are available on the API
  (`GET /api/nodes?status=…&backend=…&family=…`). Jobs themselves stay in memory.
  Without Redis the server retries the connection on startup and won't bind.
- **Library automation** (UI tabs *Library* / *Logic*): register watch folders; the server
  scans them read-only and auto-queues newly found videos. The *Logic* tab is a
  **ComfyUI-style node graph**: drag **조건 (condition)** and **액션 (action)** nodes
  around a dotted canvas and wire them together (socket → socket). A file enters at the
  ▶ **시작 (start)** node and follows the wires to the **first action node it reaches**;
  condition nodes split into ✓ (true) / ✗ (false) branches, so chaining conditions in
  series is AND, fan-out is OR. Condition kinds: source codec, size/bitrate thresholds,
  *name contains*, "node has HW accel for this codec"; actions: start encoding
  (codec / CRF or % of source bitrate / audio), healthcheck, skip. A file that reaches
  no action uses the folder's default target. Re-encoded files land next to their
  source and are skipped on rescan (tombstone guard). The status column shows a live
  progress bar, a ⟳ re-encode button and the error message on failure; a summary row
  counts files / done / errors / bytes saved. Jobs stuck in `dispatched` for 10+ min
  (node died) self-heal back to the queue. A result is only marked `done` after its
  on-disk size is verified — a corrupt/torn upload surfaces as an error you can
  re-queue, never a silent 0-byte "done". Old ordered block lists are auto-migrated to
  an equivalent graph on first load.
- The node probes every (device, backend, encoder) combination at startup and reports
  accelerators per physical device: `GPU #0 (nvenc)`, `CPU #0 (qsv)`, `CPU #0 (vaapi)`, …
  **Multi-GPU nodes:** the server pins each job to a concrete physical device
  (least-loaded first) — `CUDA_VISIBLE_DEVICES` for NVENC, `-vaapi_device` / `-qsv_device`
  render nodes otherwise — so a two-GPU box actually uses both; raise the node's
  concurrent-job count (dashboard gear) to let them run in parallel.
- **Post-encode chain:** the encode action node has an output port — chain
  *인코딩 이후* nodes on it and they run (in order) when the finished file
  arrives: **출력 검증** (ffprobe health check — video stream present +
  duration ≥ 50% of source; verify ALL files, a random N% sample, or every
  Nth — a stable key hash, so big folders aren't probed file by file; on
  failure: mark error **or send back to the queue** to retry), **원본 대치**
  (the finished file takes the source's path; the original is kept as
  `name.orig.ext` and tombstoned so rescans never re-queue it), **원본 위치
  에 저장** (keep the output next to the source — the library default, made
  explicit), **큐로 되돌리기** (re-queue the entry after the chain). A failed
  health check stops the chain (never replaces a corrupt file).
- **⚡ HW-only encode:** the encode action has an *HW 전용* checkbox — the
  file is never sent to a node without a working hardware encoder for its
  codec (no software fallback, no other-node fallback). With no capable node
  online the entry shows a clear error and a later rescan retries it; the
  pair with the *HW 가속 지원* condition lets you branch "encode if a
  capable node exists / skip otherwise".
- The folder-row **✓/⟲ checkboxes** in the Library tab still work for
  folders without a graph (legacy target flags fold into the same post-encode
  plan).
- The Library tab's **🔍 폴더 찾아보기** opens a filesystem tree browser
  (`GET /api/fs/children`) — click through directories, jump to any absolute path,
  see video files inline, and add the selected folder to the watch list in one click.
- The server auto-rewrites logical codecs to the node's best working HW encoder
  (nvenc > vaapi > qsv > amf > videotoolbox > software) and records `effectiveVideoCodec` + `accelerated` per job.
- `frontend/dist` is committed so a clone runs without a Node toolchain; rebuild it with
  `npm run build` in `CodecTrans-server/frontend/` when changing the UI.

## Running on a NAS (Docker Compose, e.g. QNAP)

`docker/docker-compose.yml` brings up **redis + server + node** in one go.

**1. Get the images on the NAS.** Compose cannot download the app images by
itself — it only pulls them, so they have to live in a registry (or be
`docker save`d over):

```bash
# on your build box (needs docker + `docker login`):
docker/push-images.sh <your-docker-hub-user>      # -> docker.io/<user>/codectrans-{server,node}:latest
# or GHCR:
docker/push-images.sh <your-gh-user> latest ghcr
```

then on the NAS either point the compose `image:` fields at the registry
paths (Docker Hub — no auth needed) or `docker pull` + `docker tag` them to
`codectrans/{server,node}:latest` (GHCR). No-registry option:
`docker save` both images into one tar on the build box, copy it to the NAS
via File Station, `docker load -i`, then `up`.

**2. Point the media mounts at your shares.** QNAP pools live at `/mnt/1`,
`/mnt/2`, … — one bind-mount line per shared folder:

```yaml
volumes:
  - server-data:/data
  - /mnt/1/media:/media/media     # share on pool 1
  - /mnt/2/movies:/media/movies   # share on pool 2
```

Register `/media/media`, `/media/movies` in the web UI (🔍 폴더 찾아보기
lists them once the mount is live).

**3. GPU (e.g. RTX 3050).** The compose runs the node with
`runtime: nvidia` + `NVIDIA_DRIVER_CAPABILITIES=video,utility`. Prerequisites
on the NAS: an installed NVIDIA driver **and** the nvidia-container-toolkit
(`docker info` must list the `nvidia` runtime). Then the containerized node
auto-probes the GPU and reports its real NVENC encoders — an RTX 3050
(Ampere) reports H.264/HEVC NVENC (no AV1 encode; that's Ada/RTX 40 and
newer), newer cards also AV1. No GPU: delete the `runtime` line + the
`NVIDIA_*` vars and the node runs software-only (enable `allowSoftware` in
the dashboard gear). Verified in-container: node on the nvidia runtime
probed `nvenc device 0` and encoded AV1 (Blackwell test box) end to end.

**4. Start:**

```bash
docker compose -f docker/docker-compose.yml up -d
# open http://<nas-ip>:8400
```

- **The node does not need the media share** — it streams sources from the
  server and uploads finished files back; only the *server* container needs
  the media bind mounts.
- **Persistence:** node identity/registry lives in the `redis-data` volume,
  catalog + library config + uploads in `server-data`. Removing a service
  without `-v` keeps both.
- **Architecture:** all current QNAP TS lines + QuTS hero are `x86_64` — the
  default images run as-is (verify with `uname -m` on the QNAP; ARM boxes
  need `docker buildx --platform` with a cross target).
- **Uploads:** finished files stream back through the server; the compose
  sets `CT_MAX_UPLOAD_MB=16384`. Raise it for large 4K masters.
- **Ownership:** container processes run as root, so outputs land root-owned
  (0644) — readable by QNAP services. If you need specific uid/gid, remap
  the share or run the server service with `user: "1000:1000"` (QNAP QUGID)
  and chown the `server-data` volume once.
- **Images:** `ubuntu:24.04` + ffmpeg 6.1 (NVENC incl. AV1 on Ada/Blackwell),
  ~800MB each, `linux/amd64`.

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
