#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════
#  CodecTrans node installer (Linux + macOS)
#
#  Guides you through installing a CodecTrans transcode node:
#    1. server address / node name / bind / worker count
#    2. GPU selection (multi-GPU aware — pins CUDA_VISIBLE_DEVICES)
#    3. optional NFS share mount (+ fstab entry so it survives reboot)
#    4. start the node (systemd on Linux, nohup/launchd hints on macOS)
#    5. verify: local API + registration on the server
#
#  Interactive by default. Everything can be passed as flags for
#  unattended installs:
#
#    ./install-node.sh --server http://192.168.x.x:8400 \
#                      --name "NAS node" --jobs 2 --gpus all \
#                      --nfs server:/exports/videos /mnt/videos --yes
#
#  Options:
#    --server URL        CodecTrans server (default: http://127.0.0.1:8400)
#    --name NAME         display name (default: hostname)
#    --bind all|local    bind address (default: all)
#    --jobs N            concurrent encodes (default: 1; 2-3 on 2-slice NVENC)
#    --gpus all|LIST     NVIDIA GPU indices, comma-separated (default: all)
#    --nfs SRC MOUNT     mount an NFS share: SRC = host:/export, MOUNT = path
#    --fstab yes|no      write the NFS mount to /etc/fstab (default: yes)
#    --yes               no prompts, use given flags + defaults
#    --port N            node local API port (default: 8401)
#    --daemon            Linux: install a systemd unit instead of nohup
#    --uninstall         stop the node (and remove the systemd unit)
#    -h, --help          this help
# ═══════════════════════════════════════════════════════════════════
set -euo pipefail

SERVER="http://127.0.0.1:8400"
NAME="$(hostname 2>/dev/null || echo node)"
BIND="all"
JOBS=1
GPUS="all"
NFS_SRC=""
NFS_MOUNT=""
FSTAB="yes"
PORT=8401
YES=0
DAEMON=0
UNINSTALL=0

c_green() { printf '\033[32m%s\033[0m' "$*"; }
c_red()   { printf '\033[31m%s\033[0m' "$*"; }
c_cyan()  { printf '\033[36m%s\033[0m' "$*"; }
c_yellow(){ printf '\033[33m%s\033[0m' "$*"; }

die()  { echo >&2 "$(c_red "✗ $*")"; exit 1; }
info() { echo "$(c_cyan "▸ $*")"; }
ok()   { echo "$(c_green "✓ $*")"; }

banner() {
  cat <<'EOF'
   ╔══════════════════════════════════════════════╗
   ║   CodecTrans — node installation wizard      ║
   ╚══════════════════════════════════════════════╝
EOF
}

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,2\}//'; exit 0; }

# ── arg parsing ──────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server)  SERVER="$2"; shift 2 ;;
    --name)    NAME="$2"; shift 2 ;;
    --bind)    BIND="$2"; shift 2 ;;
    --jobs)    JOBS="$2"; shift 2 ;;
    --gpus)    GPUS="$2"; shift 2 ;;
    --nfs)     NFS_SRC="$2"; NFS_MOUNT="$3"; shift 3 ;;
    --fstab)   FSTAB="$2"; shift 2 ;;
    --yes)     YES=1; shift ;;
    --port)    PORT="$2"; shift 2 ;;
    --daemon)  DAEMON=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in linux|darwin) ;; *) die "unsupported OS: $OS" ;; esac

banner

# ── uninstall path ───────────────────────────────────────────────
if [[ $UNINSTALL -eq 1 ]]; then
  if [[ "$OS" == "linux" ]] && [[ -f /etc/systemd/system/codectrans-node.service ]]; then
    sudo systemctl disable --now codectrans-node 2>/dev/null || true
    sudo rm -f /etc/systemd/system/codectrans-node.service
    sudo systemctl daemon-reload
    ok "systemd unit removed"
  fi
  pkill -f "CodecTransNode" 2>/dev/null && ok "node process stopped" || info "no running node process"
  exit 0
fi

# ── interactive prompts ──────────────────────────────────────────
ask() { # ask VAR "prompt" DEFAULT
  local var="$1" prompt="$2" def="${3:-}" val
  if [[ $YES -eq 1 ]]; then
    printf -v "$var" '%s' "${!var:-$def}"
    return
  fi
  read -r -p "$(c_cyan "▸ $prompt") [${def:-}]: " val || val=""
  [[ -n "$val" ]] && val="$val" || val="$def"
  printf -v "$var" '%s' "$val"
}

if [[ $YES -eq 0 ]]; then
  echo
  ask SERVER "CodecTrans server address" "$SERVER"
  ask NAME   "node display name" "$NAME"
  ask BIND   "bind (all = LAN, local = 127.0.0.1 only)" "$BIND"
  ask JOBS   "concurrent encodes" "$JOBS"
  ask GPUS   "NVIDIA GPUs (all, or indices like 0 / 0,1)" "$GPUS"
  read -r -p "$(c_cyan "▸ mount an NFS share for the library?") [y/N]: " a || a="n"
  if [[ "$a" =~ ^[Yy] ]]; then
    ask NFS_SRC   "NFS source (host:/export)" ""
    ask NFS_MOUNT "mount point" "/mnt/videos"
  fi
  echo
fi

# ── environment checks ───────────────────────────────────────────
info "OS: $OS · node port: $PORT · server: $SERVER"

if ! command -v ffmpeg >/dev/null 2>&1; then
  [[ "$OS" == "linux" ]] && die "ffmpeg not found — install it first (e.g. sudo apt install ffmpeg / dnf install ffmpeg)"
  [[ "$OS" == "darwin" ]] && die "ffmpeg not found — install it first: brew install ffmpeg"
fi
ok "ffmpeg: $(ffmpeg -version 2>/dev/null | head -1 | awk '{print $3}')"

# GPU detection (for the summary + CUDA pinning)
GPU_LIST=""
if command -v nvidia-smi >/dev/null 2>&1; then
  GPU_LIST="$(nvidia-smi --query-gpu=index,name --format=csv,noheader 2>/dev/null || true)"
  NGPU="$(echo -n "$GPU_LIST" | grep -c . || true)"
else
  NGPU=0
fi
if [[ $NGPU -gt 0 ]]; then
  ok "NVIDIA GPUs:"
  echo "$GPU_LIST" | sed 's/^/     /'
else
  if [[ "$OS" == "darwin" ]]; then
    ok "GPU: Apple Silicon / VideoToolbox (single device)"
  else
    info "no NVIDIA GPUs detected (VA-API/QuickSync nodes still work)"
  fi
fi

# GPU selection -> CUDA_VISIBLE_DEVICES
CUDA=""
if [[ $NGPU -gt 0 ]]; then
  if [[ "$GPUS" != "all" ]]; then
    CUDA="$(echo "$GPUS" | tr ',' ' ' | tr -s ' ' ',')"
  fi
fi

# ── binary ───────────────────────────────────────────────────────
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/target/release/CodecTransNode"
if [[ ! -x "$BIN" ]]; then
  if [[ -f "$HERE/Cargo.toml" ]] && command -v cargo >/dev/null 2>&1; then
    info "building CodecTransNode (cargo release)…"
    (cd "$HERE" && cargo build --release) || die "cargo build failed"
  fi
fi
[[ -x "$BIN" ]] || die "binary not found: $BIN — build it on the source machine and copy CodecTrans-node/target/release/CodecTransNode here, then re-run"
ok "binary: $BIN"

# ── NFS (optional) ───────────────────────────────────────────────
if [[ -n "$NFS_SRC" && -n "$NFS_MOUNT" ]]; then
  info "NFS: $NFS_SRC -> $NFS_MOUNT"
  [[ $YES -eq 0 ]] && read -r -p "$(c_cyan "▸ continue?") [Y/n]: " a || a="y"
  [[ ! "$a" =~ ^[Nn] ]] || { NFS_SRC=""; NFS_MOUNT=""; }
fi
if [[ -n "$NFS_SRC" && -n "$NFS_MOUNT" ]]; then
  sudo mkdir -p "$NFS_MOUNT"
  if mountpoint -q "$NFS_MOUNT" 2>/dev/null || grep -qs "$NFS_MOUNT" /proc/mounts; then
    ok "already mounted: $NFS_MOUNT"
  else
    if [[ "$OS" == "darwin" ]]; then
      mount -t nfs -o soft "$NFS_SRC" "$NFS_MOUNT" \
        || die "mount failed — check the export and network (server must export $NFS_SRC)"
    else
      command -v mount.nfs >/dev/null 2>&1 || sudo apt-get install -y nfs-common 2>/dev/null || sudo dnf install -y nfs-utils 2>/dev/null || true
      mount -t nfs -o soft,noatime "$NFS_SRC" "$NFS_MOUNT" \
        || die "mount failed — check the export and network (server must export $NFS_SRC)"
    fi
    ok "mounted $NFS_MOUNT"
    if [[ "$FSTAB" == "yes" ]]; then
      if ! grep -qs "$NFS_MOUNT" /etc/fstab 2>/dev/null; then
        if [[ "$OS" == "darwin" ]]; then
          # macOS fstab uses different options; write a best-effort line.
          echo "$NFS_SRC $NFS_MOUNT nfs soft 0 0" | sudo tee -a /etc/fstab >/dev/null
        else
          echo "$NFS_SRC $NFS_MOUNT nfs soft,noatime,x-systemd.after=network-online.target 0 0" | sudo tee -a /etc/fstab >/dev/null
        fi
        ok "added to /etc/fstab (survives reboot)"
      fi
    fi
  fi
fi

# ── start the node ───────────────────────────────────────────────
ENVLINE="CT_SERVER=$SERVER CT_NAME=$NAME CT_BIND=$BIND CT_PORT=$PORT CT_MAX_JOBS=$JOBS"
[[ -n "$CUDA" ]] && ENVLINE="$ENVLINE CUDA_VISIBLE_DEVICES=$CUDA"

stop_existing() {
  pkill -f "CodecTransNode" 2>/dev/null && sleep 1 || true
}

if [[ "$OS" == "linux" && $DAEMON -eq 1 ]]; then
  info "installing systemd unit (codectrans-node)"
  stop_existing
  # shellcheck disable=SC2016
  sudo tee /etc/systemd/system/codectrans-node.service >/dev/null <<EOF
[Unit]
Description=CodecTrans transcode node
After=network-online.target
Wants=network-online.target

[Service]
Environment=$ENVLINE
ExecStart=$BIN
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
  sudo systemctl daemon-reload
  sudo systemctl enable --now codectrans-node
  ok "systemd: codectrans-node enabled + started"
else
  stop_existing
  info "starting node (nohup) — for a reboot-proof setup use --daemon (Linux) or run inside the server's SSH session (macOS)"
  cd "$HERE"
  env $ENVLINE nohup "$BIN" >>"$HERE/codectrans-node.log" 2>&1 &
  NODE_PID=$!
  ok "node started (pid $NODE_PID, log: $HERE/codectrans-node.log)"
fi

# ── verify ───────────────────────────────────────────────────────
sleep 2
info "verifying…"
LOCAL_OK=0
for i in $(seq 1 15); do
  if curl -fsS "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1; then
    LOCAL_OK=1; break
  fi
  sleep 1
done
if [[ $LOCAL_OK -eq 1 ]]; then
  ok "local API: http://127.0.0.1:$PORT/api/health"
else
  die "node did not come up — check the log (nohup: $HERE/codectrans-node.log, systemd: journalctl -u codectrans-node)"
fi

SERVER_OK=0
for i in $(seq 1 10); do
  if curl -fsS "$SERVER/api/nodes" 2>/dev/null | grep -q .; then
    SERVER_OK=1; break
  fi
  sleep 1
done
if [[ $SERVER_OK -eq 1 ]]; then
  echo
  ok "node registered — dashboard: $SERVER"
  echo
  cat <<EOF
   $(c_green "Done.")
   name:   $NAME
   server: $SERVER
   jobs:   $JOBS  $( [[ -n "$CUDA" ]] && echo "| GPUs: $CUDA" )
   $( [[ -n "$NFS_MOUNT" ]] && echo "nfs:    $NFS_MOUNT" )
   tip:    set the per-worker resources (which backends may run, concurrent
           jobs) from the server dashboard's gear button per node.
EOF
else
  echo
  c_yellow "⚠ server $SERVER unreachable — the node is running locally but"
  c_yellow "  could not announce itself. Check the address / firewall, then"
  c_yellow "  re-run the wizard (it will re-announce on the next heartbeat)."
  exit 1
fi
