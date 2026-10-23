# CodecTrans

Distributed media transcoding service. **Rust (axum)** central server + workers,
**React (Vite)** frontend. Upload a file, pick a target codec/container/quality,
and transcode **on the server (local CPU) or dispatch to a worker node** that
auto-selects the best **hardware-accelerated** encoder. Watch progress, download
the result.

```
                    ┌────────────────────────────┐
   browser ───────► │  CodecTrans server :8400   │  React UI + /api/*
                    │  - node registry          │
                    │  - job dispatch           │
                    │  - HW-accel auto-select   │
                    │  - uploads/ outputs/      │
                    └───────┬────────────┬──────┘
                            │ poll (long)│  source ↓  result ↑
                     ┌──────▼───┐   ┌────▼─────┐
                     │ node A   │   │ node B   │   CodecTrans-node (Rust)
                     │ :8401    │   │ :8402    │   detects local ffmpeg
                     │ CPU/GPU  │   │ GPU nvenc│   accel, transcodes, reports
                     └──────────┘   └──────────┘
```

## Layout

```
CodecTrans/            central server
├── backend/           Rust (axum) — port 8400
├── frontend/          React + Vite SPA (served by the backend)
├── uploads/  outputs/ runtime files
├── run.sh             build + start server only
└── start-all.sh       build + start server AND a local node

CodecTrans-node/       worker node (one per machine)
├── src/main.rs        capability discovery + register + poll + transcode
├── in/  out/          node-local scratch (created at runtime)
└── run.sh             build + start the node
```

## Requirements

- Rust toolchain (`cargo`), Node.js + npm, `ffmpeg`/`ffprobe` on `PATH`

## Run

```bash
./start-all.sh        # server (:8400) + one local node (:8401)
```

Then open **http://localhost:8400**. Pick **Run on → Auto / a node** to dispatch,
or **Server** for local CPU transcoding.

Run pieces separately:

```bash
# server
cd CodecTrans && ./run.sh
# a node (run on any machine with ffmpeg). Default: connects to localhost server,
# listens on 0.0.0.0, takes its name from the OS account.
cd CodecTrans-node && CT_SERVER=http://192.168.x.x:8400 ./run.sh
```

## Connecting a node

Two ways to get a node onto the server — pick either (or both):

1. **Node connects itself** — set `CT_SERVER` on the node and it registers on
   start (this is the default: `http://localhost:8400`).
2. **Server finds the node** — the node prints its `IP:port` in its terminal on
   start:
   ```
   [node] node 'media-pc' is up
   [node]   bind:  0.0.0.0 (all interfaces — reachable on the LAN)
   [node]   LAN:   http://192.168.x.x:8401  <- give this IP:port to the server
   [node]   server: http://192.168.x.x:8400
   [node]   accel: h264_nvenc, hevc_nvenc
   ```
   Enter that `IP:port` in the server UI's **Search** box (or `POST
   /api/nodes/search`) — the server probes the node's local API, verifies its
   capabilities, and registers it. This is the reliable LAN route (no mDNS
   needed).

Node env:
- `CT_SERVER` — server URL (default `http://localhost:8400`, or `auto` to
  mDNS-discover).
- `CT_NODE_NAME` — node name (default: your OS **account name**).
- `CT_PORT` — node local API port (default `8401`).
- `CT_BIND` — `local` → `127.0.0.1` (this machine only); `all` → `0.0.0.0`
  (reachable on the LAN, **default**).

## Storage Library (scan → re-encode → replace in place)

Point the server at a folder of videos (a NAS share it can mount, or a local dir)
and it builds a catalog you can re-encode and replace — the end goal of "take the
videos in my storage and re-encode them to a more efficient codec."

- `POST /api/library/scan {root?, limit?}` — the server walks the folder
  (**read-only**, your files are never touched) and ffprobes each video into the
  catalog (codec, audio, duration, size, status). Root precedence: request body →
  previously set root → `CT_STORAGE` env.
- `GET /api/library` — `{root, count, entries}`.
- The UI **Library** tab shows the catalog (File / Codec / Size / Duration /
  Status) and the scan box.

**Planned next (incremental):**
- `POST /api/library/reencode` — pick files (or "all H.264") + target codec →
  queue jobs to nodes (HW auto-select as before).
- **In-place replace with a configurable original policy**: `backup` (keep
  `.bak`, recommended) / `overwrite` / `keepboth` (suffix the new file). The new
  file is written to a temp name, ffprobe-verified, then atomically renamed over
  the original — so the source is only ever read during transcoding.
- **Streaming data path**: the server streams the source to the node (chunked)
  and the node streams the result back, so neither side must hold the whole file.

## How acceleration automation works

1. **Node discovers** its real capabilities on start: HW encoders from
   `ffmpeg -encoders`, then **probes each with a real 2-frame test encode**
   (`testsrc` 320×240 → `.mp4`) and keeps only the ones that actually work —
   an encoder can be compiled in with no GPU. (NVENC rejects frames below its
   minimum size, and HW-encoded frames can't sink to `-f null`, so the probe
   writes a real file at ≥320×240. VA-API additionally needs the account in the
   `render` group for `/dev/dri`.)
2. **Node registers** with the server, reporting `haccels` + working
   `hwEncoders`. Re-registration is idempotent by name, so a server restart just
   re-attaches the node (the node detects the 404 and re-registers itself).
3. **Dispatch auto-selects**: you ask for a logical codec (`h264`, `h265`,
   `vp9`, …). The server maps it to the node's best working HW encoder
   (nvenc > vaapi > qsv > amf > videotoolbox). If the node has no working HW
   encoder for that family it falls back to the canonical software encoder
   (`libx264`, `libx265`, `libvpx-vp9`, …). The job's `effectiveVideoCodec` and
   `accelerated` flag tell you what actually ran (⚡ in the UI).
4. **The node** downloads the source from the server, transcodes locally with
   the chosen encoder, reports progress, uploads the result, and reports done.

`forceCodec: true` on dispatch skips the HW rewrite (use the requested encoder
verbatim).

## API

| Method | Path                      | Description |
|--------|---------------------------|-------------|
| GET    | `/api/health`             | status + ffmpeg version |
| GET    | `/api/codecs`             | encoders/containers (filtered to this ffmpeg build) |
| POST   | `/api/upload`             | multipart `file` → `{file, size}` |
| POST   | `/api/transcode`          | run on the **server** → `{id}` |
| POST   | `/api/dispatch`           | run on a **node** (`nodeId` or auto) → `{id, effectiveVideoCodec, accelerated}` |
| GET    | `/api/jobs` / `/api/jobs/:id` | list / one job (`progress` 0..1, `error`, `executedBy`, `accelerated`) |
| GET    | `/api/download/:name`     | download a produced file (path-traversal guarded) |
| GET    | `/api/nodes`              | all nodes + live status + acceleration capabilities |
| POST   | `/api/nodes/search`       | **server finds a node**: probe `{ip, port?}` → verify + register |
| GET    | `/api/nodes/discovered`   | nodes seen on the LAN via mDNS (auto, not yet registered) |
| POST   | `/api/nodes/register`     | node announces capabilities (idempotent by name) |
| POST   | `/api/nodes/:id/heartbeat`| liveness |
| GET    | `/api/nodes/:id/poll`     | node long-polls for its next job (up to 20s) |
| POST   | `/api/nodes/:id/report`   | node reports progress / completion |
| GET    | `/api/nodes/:id/file/:name` | node fetches an input from the server |
| POST   | `/api/nodes/:id/result`   | node uploads a finished output (multipart) |

Node local API (`:8401`): `/api/health`, `/api/capabilities`, `/api/jobs`.

## Notes / constraints

- Jobs + node registry live in memory — restarting the **server** clears them
  (files on disk remain; nodes re-attach automatically). Restarting a **node**
  re-registers it under the same name.
- Some codec/container pairs are rejected by ffmpeg (e.g. `libx264` in `.webm`)
  → job ends `status:"error"` with the captured ffmpeg message.
- `CRF` is applied to x264/x265/vp9/vp8; HW encoders run at their default
  quality for now (per-backend quality flags are a roadmap item).
- Node↔server transfer is in-memory (fine for typical clips); large files will
  want streaming — roadmap.

## Roadmap (차근차근)

- [x] node system: register / heartbeat / poll / dispatch / transfer
- [x] HW-acceleration discovery (real probe) + auto encoder selection
- [x] auto re-registration on server restart
- [x] node naming (account name default) + bind choice (local / 0.0.0.0) +
      terminal IP:port banner + server-side "Search" (probe a node by IP:port)
- [x] mDNS auto-discovery (bonus; LAN-only — server `/api/nodes/discovered`)
- [ ] per-backend quality flags for HW encoders (nvenc `-cq`, vaapi `-quality`)
- [ ] job persistence (SQLite/JSON) across restarts
- [ ] per-node concurrency limit + queue ordering
- [ ] ffprobe metadata on upload (duration / resolution) shown in the UI
- [ ] per-job ffmpeg log streaming (SSE/WebSocket)
- [ ] streaming (not in-memory) node↔server file transfer
