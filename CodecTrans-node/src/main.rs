//! CodecTrans-node — a distributed transcoding worker for the CodecTrans server.
//!
//! On start it:
//!   1. detects the local ffmpeg build's HW-acceleration capabilities
//!      (`ffmpeg -haccels` + HW encoders from `-encoders`)
//!   2. registers with the central server (POST /api/nodes/register)
//!   3. runs a heartbeat loop and a long-poll loop
//!
//! When the server dispatches a job to this node it:
//!   - downloads the input from the server
//!   - transcodes locally with ffmpeg (using the server-chosen encoder, which
//!     may be an HW-accelerated one)
//!   - reports progress, then uploads the result and reports completion
//!
//! A small local axum API (port CT_PORT, default 8401) exposes
//! /api/health, /api/capabilities and /api/jobs for inspection.
//!
//! Env:
//!   CT_SERVER    server base URL      (default http://localhost:8400)
//!                set "auto" to discover the server on the LAN via mDNS
//!   CT_NODE_NAME node name            (default: the OS account/user name)
//!   CT_PORT      local API port       (default 8401)
//!   CT_MAX_JOBS  max concurrent encode jobs (default 1; the server can
//!                override this live via the poll response / dashboard gear)
//!
//! LAN discovery (mDNS, zero-config):
//!   - This node always advertises itself as `_codectrans._tcp.local.` on the
//!     LAN, so a CodecTrans server can find it without any config.
//!   - If CT_SERVER=auto, this node also browses `_codectrans-server._tcp.local.`
//!     and connects to the first server it finds.

#![allow(non_snake_case, unused)]

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, ResolvedService, IfKind};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ============================================================
//  config / paths / time
// ============================================================

fn server_url() -> String {
    std::env::var("CT_SERVER").unwrap_or_else(|_| "http://localhost:8400".into())
}
/// Default node name = the OS account (user) name, so each machine/account is
/// distinguishable at a glance. Overridable via CT_NODE_NAME.
fn node_name() -> String {
    if let Ok(n) = std::env::var("CT_NODE_NAME") {
        let t = n.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    for var in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(u) = std::env::var(var) {
            let t = u.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    std::process::Command::new("whoami")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".into())
}
/// Best-effort LAN IPv4 of this host (egress interface). Used for mDNS TXT.
fn lan_ip() -> String {
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        // "connect" to a public address picks the egress interface without
        // sending any packet.
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}
/// mDNS/`.local.` hostname for this host (used in the A/AAAA record).
fn mdns_hostname() -> String {
    let base = std::env::var("HOSTNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "host".into());
    format!("{base}.local.")
}
fn local_port() -> u16 {
    std::env::var("CT_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8401)
}
/// Local API bind address. CT_BIND:
///   "local"  -> 127.0.0.1  (only this machine can reach it)
///   "all"    -> 0.0.0.0    (any interface; reachable on the LAN) — default
fn bind_addr() -> String {
    match std::env::var("CT_BIND").unwrap_or_default().to_lowercase().as_str() {
        "local" | "localhost" | "127.0.0.1" => "127.0.0.1".to_string(),
        _ => "0.0.0.0".to_string(),
    }
}
fn work_dir() -> PathBuf {
    // Scratch area for the in-flight source (in/) and finished file (out/).
    // CT_WORKDIR overrides it (Docker: the compile-time manifest dir is
    // baked into the image, so point it at a clean path instead).
    let d = std::env::var("CT_WORKDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let _ = std::fs::create_dir_all(&d);
    d
}
fn in_dir() -> PathBuf {
    let d = work_dir().join("in");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn out_dir() -> PathBuf {
    let d = work_dir().join("out");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ============================================================
//  ffmpeg capability discovery
// ============================================================

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Caps {
    ffmpeg_version: String,
    haccels: Vec<String>,
    /// Flat list of working HW encoders (what the server's dispatch logic uses).
    hw_encoders: Vec<String>,
    /// Grouped per physical device: GPU #0 (nvenc...), CPU #0 (qsv/vaapi)...
    accels: Vec<Accel>,
}

fn is_hw_encoder(enc: &str) -> bool {
    let e = enc.to_lowercase();
    e.ends_with("_nvenc")
        || e.ends_with("_vaapi")
        || e.ends_with("_qsv")
        || e.ends_with("_amf")
        || e.ends_with("_mft")
        || e.ends_with("_vt") // videotoolbox (macOS): h264_vt, hevc_vt, mjpeg_vt
        || e.contains("videotoolbox")
}

/// One accelerator: a physical device (discrete GPU or iGPU) + backend pair.
/// The node reports these so the server/UI can group and number them:
/// "GPU #0" (NVIDIA, nvenc), "CPU #0" (Intel iGPU, qsv/vaapi), and so on.
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Accel {
    pub kind: String,     // "gpu" (discrete) | "cpu" (iGPU / integrated)
    pub device: u32,      // 0-based index within its kind
    pub name: String,     // e.g. "NVIDIA RTX PRO 4000 Blackwell"
    pub backend: String,  // "nvenc" | "qsv" | "vaapi"
    pub encoders: Vec<String>,
}

/// A physical GPU device found on this box.
#[derive(Clone)]
struct GpuDev {
    kind: String,               // "gpu" | "cpu"
    index: u32,                 // 0-based within its kind
    name: String,
    vendor: String,             // "nvidia" | "intel"
    render_node: Option<String>, // /dev/dri/renderDxxx (VA-API/QSV path)
}

/// Human-readable name for an Intel iGPU (lspci, with a plain fallback).
fn intel_gpu_name(sys_dev: &str) -> String {
    // /sys/class/drm/renderD128/device -> /sys/devices/pci0000:00/0000:00:02.0
    if let Ok(link) = std::fs::read_link(sys_dev) {
        if let Some(bdf) = link.to_string_lossy().rsplit('/').next() {
            // "0000:00:02.0" -> lspci wants "00:02.0" (last two segments)
            let parts: Vec<&str> = bdf.rsplit(':').collect();
            let bdf2 = if parts.len() >= 2 {
                format!("{}:{}", parts[1], parts[0])
            } else {
                bdf.to_string()
            };
            if let Ok(o) = std::process::Command::new("lspci").args(["-s", &bdf2]).output() {
                let line = String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string();
                // "00:02.0 VGA compatible controller: Intel Corporation ..."
                if let Some(colon) = line.find(':') {
                    let after = line[colon + 1..].trim();
                    // "VGA compatible controller: Intel Corporation ..." -> the
                    // part after the 2nd ": "
                    if let Some(n) = after.split(": ").nth(1) {
                        if !n.is_empty() {
                            // drop the trailing " (rev 06)" noise
                            return n.split(" (rev ").next().unwrap_or(n).to_string();
                        }
                    }
                }
            }
        }
    }
    "Intel iGPU".into()
}

/// macOS GPU name (VideoToolbox device). system_profiler is slow-ish but this
/// runs once at startup.
fn mac_gpu_name() -> String {
    if let Ok(o) = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let l = line.trim();
            for key in ["\"chipset_model\"", "\"chip_model\""] {
                if let Some(i) = l.find(key) {
                    let rest = l[i + key.len()..].trim().trim_start_matches(':').trim().trim_matches('"');
                    if !rest.is_empty() {
                        return rest.to_string();
                    }
                }
            }
        }
    }
    "Apple GPU".into()
}

/// Windows video controllers (vendor, name) via WMI. NVIDIA is reported by
/// nvidia-smi already, so skip it here to avoid double numbering.
fn windows_video_controllers() -> Option<Vec<(String, String)>> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
        ])
        .output()
        .ok()?;
    let mut res: Vec<(String, String)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let n = line.trim();
        if n.is_empty() {
            continue;
        }
        let l = n.to_lowercase();
        if l.contains("nvidia") {
            continue;
        }
        if l.contains("intel") {
            res.push(("intel".into(), n.to_string()));
        } else if l.contains("amd") || l.contains("radeon") {
            res.push(("amd".into(), n.to_string()));
        }
    }
    Some(res)
}

/// Enumerate GPU devices per platform:
///  - NVIDIA via nvidia-smi (Linux + Windows)
///  - Linux iGPUs via /dev/dri render nodes + PCI vendor id
///  - Windows iGPU/AMD via WMI (Win32_VideoController)
///  - macOS: one VideoToolbox device
fn enumerate_gpus() -> Vec<GpuDev> {
    let os = std::env::consts::OS;

    // --- macOS: single VideoToolbox device ---
    if os == "macos" {
        return vec![GpuDev {
            kind: "gpu".into(),
            index: 0,
            name: mac_gpu_name(),
            vendor: "apple".into(),
            render_node: None,
        }];
    }

    let mut out: Vec<GpuDev> = Vec::new();

    // --- NVIDIA (discrete GPUs) ---
    let mut nvidia_devs: Vec<(u32, String)> = Vec::new();
    if let Ok(o) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,name", "--format=csv,noheader"])
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let mut parts = t.splitn(2, ',');
            let idx = parts
                .next()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(out.len() as u32);
            let name = parts.next().map(|s| s.trim().to_string()).unwrap_or_else(|| "NVIDIA GPU".into());
            nvidia_devs.push((idx, name));
        }
    }

    // --- Linux render nodes: classify by PCI vendor ---
    let mut intel: Vec<(u32, String, String)> = Vec::new(); // (idx, node, name)
    let mut nvidia_nodes: Vec<String> = Vec::new();
    let mut n_idx = 0u32;
    if os == "linux" {
        if let Ok(rd) = std::fs::read_dir("/dev/dri") {
            let mut entries: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("renderD"))
                .collect();
            entries.sort();
            for n in entries {
                let sys = format!("/sys/class/drm/{}/device", n);
                let vendor = std::fs::read_to_string(format!("{sys}/vendor"))
                    .ok()
                    .map(|v| v.trim().to_string())
                    .unwrap_or_default();
                match vendor.as_str() {
                    "0x8086" => {
                        let name = intel_gpu_name(&sys);
                        intel.push((n_idx, format!("/dev/dri/{n}"), name));
                        n_idx += 1;
                    }
                    "0x10de" => nvidia_nodes.push(format!("/dev/dri/{n}")),
                    _ => {}
                }
            }
        }
    }

    // --- Windows: WMI video controllers (Intel iGPU -> QSV, AMD -> AMF) ---
    if os == "windows" {
        if let Some(ctrls) = windows_video_controllers() {
            let mut i_idx = 0u32;
            let mut a_idx = 0u32;
            for (vendor, name) in ctrls {
                match vendor.as_str() {
                    "intel" => out.push(GpuDev {
                        kind: "cpu".into(),
                        index: i_idx,
                        name,
                        vendor: "intel".into(),
                        render_node: None, // QSV on Windows picks its device
                    }),
                    "amd" => out.push(GpuDev {
                        kind: "gpu".into(),
                        index: a_idx,
                        name,
                        vendor: "amd".into(),
                        render_node: None,
                    }),
                    _ => {}
                }
                if vendor == "intel" {
                    i_idx += 1;
                } else {
                    a_idx += 1;
                }
            }
        }
    }

    // Pair nvidia-smi order with render-node order (both follow PCI order).
    for (i, (idx, name)) in nvidia_devs.iter().enumerate() {
        out.push(GpuDev {
            kind: "gpu".into(),
            index: *idx,
            name: name.clone(),
            vendor: "nvidia".into(),
            render_node: nvidia_nodes.get(i).cloned(),
        });
    }
    for (idx, node, name) in intel {
        out.push(GpuDev {
            kind: "cpu".into(),
            index: idx,
            name,
            vendor: "intel".into(),
            render_node: Some(node),
        });
    }
    out
}

/// Startup diagnostics shared with the server: headless/container nodes
/// (QNAP) have no shell access, so the probe/driver findings are sent with
/// every registration and exposed via `GET /api/nodes` (the `diag` field).
static DIAG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
fn dlog(msg: String) {
    println!("[node] {msg}");
    let mut g = DIAG.lock().unwrap();
    g.push(msg);
    if g.len() > 30 {
        let n = g.len() - 30;
        g.drain(..n);
    }
}

/// Ground-truth check: does this HW encoder actually work on this box?
/// An encoder can be compiled in (appears in `ffmpeg -encoders`) yet have no
/// usable device (no GPU, no /dev/dri access). We run a tiny real test encode
/// and keep only the ones that produce a non-empty file.
///
/// Two things that matter here (learned the hard way):
///  - NVENC rejects frames below its minimum dimensions, so 128x72 fails with
///    "Frame Dimension less than the minimum supported value". Use 320x240.
///  - VA-API (and other hw encoders) emit hw frames that can't go to a null
///    sink, so write to a real file (the muxer auto hwdownloads).
///
/// `extra_args` pins the device (e.g. -qsv_device/-vaapi_device for iGPU,
/// and CUDA_VISIBLE_DEVICES via `env` for a specific NVIDIA GPU).
/// `vf` is the pixel-format chain: NVENC/QSV take plain nv12, but VA-API
/// encoders need an explicit hwupload (auto_scale can't do it: "Function not
/// implemented" / ENOSYS on this ffmpeg build).
fn probe_hw_encoder(enc: &str, tag: &str, vf: &str, extra_args: &[String], env: &[(String, String)]) -> bool {
    let path = in_dir().join(format!(".probe_{enc}_{tag}.mp4"));
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "lavfi", "-i", "testsrc=duration=0.1:size=320x240:rate=10",
        // testsrc defaults to rgb24; from RGB, ffmpeg negotiates YUV444P for
        // encoders, but av1_nvenc (Blackwell, driver 595.x) rejects 4:4:4
        // ("YUV444P not supported" -> "No capable devices found"). Force NV12
        // so the probe reflects real 4:2:0 content.
        "-vf", vf,
        "-frames:v", "2", "-c:v", enc,
    ]);
    cmd.args(extra_args);
    cmd.arg(path.to_string_lossy().as_ref());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            dlog(format!("probe {enc}/{tag}: ffmpeg spawn failed: {e}"));
            return false;
        }
    };
    let good = out.status.success() && std::fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false);
    if !good {
        // Surface the real reason (missing driver libs, no device, workdir
        // not writable, ...) — dlog() ships it to the server so headless
        // (container) nodes are debuggable via GET /api/nodes.
        let code = out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
        let err: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join(" | ")
            .chars()
            .take(500)
            .collect();
        dlog(format!("probe {enc}/{tag} FAILED (ffmpeg exit {code}): {err}"));
    }
    let _ = std::fs::remove_file(&path);
    good
}

/// Selected bundled NVIDIA userland libdir (set by pick_nvidia_series).
static NVIDIA_LIBDIR: Mutex<Option<String>> = Mutex::new(None);

fn nvidia_libdir() -> Option<String> {
    NVIDIA_LIBDIR.lock().unwrap().clone()
}

/// Pick the bundled NVIDIA userland series that matches THIS host's kernel
/// driver. The NAS doesn't inject the host driver userland into containers
/// (verified on the user's QNAP: /usr/local/nvidia is absent), so the only
/// reliable way to find the match is to try: a version-skewed userland
/// fails cuInit with CUDA_ERROR_SYSTEM_DRIVER_MISMATCH. The host kernel
/// version IS readable from inside (sysfs/procfs are not namespaced), so
/// try that series first; if it isn't bundled (NAS firmware often ships a
/// driver newer than anything in the image), fetch it from the Ubuntu
/// repos at runtime, best effort. /opt/nvidia-userland/<ver>/lib holds one
/// series each; remember the winner — probe envs and per-job ffmpeg envs
/// then prepend it to LD_LIBRARY_PATH.
fn pick_nvidia_series() {
    let root = PathBuf::from("/opt/nvidia-userland");
    if !root.exists() {
        return; // no bundled series (bare host / non-NVIDIA box)
    }
    let host = host_nvidia_driver_version();
    let host_major = host.as_deref().and_then(nv_major);
    if let Some(h) = &host {
        dlog(format!("host NVIDIA kernel driver: {h}"));
    }
    if let Some(m) = host_major {
        if !root.join(m.to_string()).exists() {
            fetch_nvidia_series(m, host.as_deref().unwrap_or(""), &root);
        }
    }
    let mut bundled: Vec<u32> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.filter_map(|e| e.ok()) {
            if let Some(v) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) {
                bundled.push(v);
            }
        }
    }
    if bundled.is_empty() {
        return;
    }
    // host's own series first, then the rest newest-first
    let mut others = bundled;
    if let Some(m) = host_major {
        others.retain(|v| *v != m);
    }
    others.sort_by(|a, b| b.cmp(a));
    let mut series: Vec<u32> = host_major.into_iter().collect();
    series.append(&mut others);
    for v in series {
        let lib = root.join(v.to_string()).join("lib");
        if !lib.is_dir() {
            continue;
        }
        dlog(format!("nvidia userland series {v}: testing cuInit against host driver"));
        let ok = probe_hw_encoder(
            "h264_nvenc",
            &format!("series{v}"),
            "format=nv12",
            &[],
            &[("LD_LIBRARY_PATH".into(), lib.to_string_lossy().to_string())],
        );
        if ok {
            dlog(format!(
                "nvidia userland series {v} WORKS — all NVENC runs will use {}",
                lib.display()
            ));
            *NVIDIA_LIBDIR.lock().unwrap() = Some(lib.to_string_lossy().to_string());
            return;
        }
    }
    dlog(
        "no bundled nvidia series matched the host driver — NVENC falls back to system libs (expect DRIVER_MISMATCH if the host driver is outside the bundled series)"
            .to_string(),
    );
}

/// Read the host NVIDIA kernel driver version. /sys/module/nvidia/version
/// is just the version string; /proc/driver/nvidia/version is the NVRM
/// banner. Neither is namespaced — both report the HOST from a container.
fn host_nvidia_driver_version() -> Option<String> {
    for p in ["/sys/module/nvidia/version", "/proc/driver/nvidia/version"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Some(line) = s.lines().next() {
                let t = line.trim().to_string();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// Major series number (e.g. 575) out of a version string like
/// "575.64.03" or an NVRM banner line.
fn nv_major(line: &str) -> Option<u32> {
    for t in line.split_whitespace() {
        if let Some(m) = t.split('.').next() {
            if let Ok(n) = m.parse::<u32>() {
                if n >= 300 && n <= 999 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Fetch the driver userland matching the host kernel driver at runtime
/// when it isn't bundled (NAS firmware keeps shipping newer driver
/// series/point releases than the image). Sources, in order: NVIDIA's
/// official .run of the EXACT host version (the only source with every
/// point release — an older userland than the kernel fails with
/// SYSTEM_DRIVER_MISMATCH), then the NVIDIA CUDA repo debs (newest point
/// release of the major series), then Ubuntu apt. Surgical: only the
/// files NVENC + nvidia-smi need land in the container. Best effort — a
/// logged no-op without network or repo coverage.
fn fetch_nvidia_series(major: u32, ver: &str, root: &PathBuf) {
    dlog(format!(
        "bundled userland has no series {major} (host {ver}) — trying to fetch it (best effort)"
    ));
    let dest_lib = root.join(major.to_string()).join("lib");
    let dest_bin = root.join(major.to_string()).join("bin");
    let _ = std::fs::create_dir_all(&dest_lib);
    let _ = std::fs::create_dir_all(&dest_bin);
    let tmp = format!("/tmp/ct-nv-{major}");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::create_dir_all(&tmp);
    let script = format!(
        r#"set -e
cd {tmp}
U=https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/
fix_symlinks() {{
    for n in libcuda libnvidia-encode libnvidia-ml libnvidia-cfg; do
        t=$(ls {dl}/ 2>/dev/null | grep "^$n\.so\.[0-9]" | grep -v "^$n\.so\.1$" | head -1)
        if [ -n "$t" ]; then ln -sf "$t" {dl}/$n.so.1; fi
    done
    return 0
}}
# path 1: NVIDIA's official .run of the EXACT host version
fetch_run() {{
    case "{ver}" in *[!0-9.]*|'') return 9 ;; esac
    curl -sf -o nv.run "https://us.download.nvidia.com/XFree86/Linux-x86_64/{ver}/NVIDIA-Linux-x86_64-{ver}.run" || return 9
    sh nv.run --extract-only --target r || return 9
    for f in libcuda.so* libnvidia-encode.so* libnvidia-ml.so* libnvidia-cfg.so*; do
        cp -a r/$f {dl}/ 2>/dev/null || true
    done
    cp -a r/nvidia-smi {db}/ 2>/dev/null || true
    [ -n "$(ls {dl}/libcuda.so.[0-9]* 2>/dev/null)" ] || return 9
    fix_symlinks
}}
# path 2: NVIDIA CUDA repo debs (newest point release of the major series)
fetch_cuda_repo() {{
    curl -sf $U/ -o listing.html || return 9
    for base in libnvidia-compute-{m} libnvidia-encode-{m} libnvidia-cfg1-{m} nvidia-compute-utils-{m} nvidia-utils-{m}; do
        f=$(grep -oE ">$\{{base}}_[0-9][^>]*amd64\.deb" listing.html | sed 's/^>//' | sort -V | tail -1)
        [ -z "$f" ] && return 9
        curl -sf -O "$U/$f" || return 9
    done
}}
# path 3: Ubuntu repos (jammy, then noble — series that ARE real Ubuntu
# packages, e.g. 580/595)
fetch_apt() {{
    apt-get update -qq 2>/dev/null || true
    for p in libnvidia-compute-{m} libnvidia-encode-{m} nvidia-utils-{m}; do
        apt-get download $p 2>/dev/null || return 9
    done
}}
rm -rf r ex *.deb listing.html nv.run
fetch_run || {{
    rm -rf r nv.run
    fetch_cuda_repo || {{
        rm -f *.deb listing.html
        echo 'deb http://archive.ubuntu.com/ubuntu noble main universe' > /etc/apt/sources.list.d/ct-noble.list
        fetch_apt
    }}
    [ -n "$(ls *.deb 2>/dev/null)" ] || exit 9
    mkdir -p ex
    for d in *.deb; do dpkg -x "$d" ex/; done
    for f in libcuda.so* libnvidia-encode.so* libnvidia-ml.so* libnvidia-cfg.so*; do
        cp -a ex/usr/lib/x86_64-linux-gnu/$f {dl}/ 2>/dev/null || true
    done
    cp -a ex/usr/bin/nvidia-smi {db}/ 2>/dev/null || true
    fix_symlinks
}}
ls {dl}/
"#,
        tmp = tmp,
        m = major,
        ver = ver,
        dl = dest_lib.display(),
        db = dest_bin.display()
    );
    match std::process::Command::new("sh").args(["-c", &script]).output() {
        Ok(o) if o.status.success() => {
            dlog(format!(
                "runtime userland series {major} installed: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            ));
        }
        Ok(o) => dlog(format!(
            "runtime userland series {major} install failed (exit {}): {}",
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" | ")
                .trim()
        )),
        Err(e) => dlog(format!("runtime userland series {major}: cannot run sh: {e}")),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// One probe task: encoder + device selection + the Accel entry it feeds.
struct ProbeTask {
    enc: String,
    tag: String, // keeps parallel probe files unique per device/backend
    vf: String,  // pixel-format filter chain (see below)
    extra_args: Vec<String>,
    env: Vec<(String, String)>,
    accel: Accel,
}

/// Build per-(device, backend, encoder) probe tasks for every HW encoder
/// ffmpeg reports. NVENC: one task set per NVIDIA GPU (CUDA_VISIBLE_DEVICES=i
/// makes that GPU the only visible one; works on Linux and Windows).
/// QSV/VA-API on Linux: pinned per render node via -qsv_device / -vaapi_device
/// (both accept a DRM path). QSV on Windows / AMF / VideoToolbox: no device
/// pinning needed — the backend picks its own device.
fn build_probe_tasks(encs: &[String], devs: &[GpuDev]) -> Vec<ProbeTask> {
    let os = std::env::consts::OS;
    let mut tasks: Vec<ProbeTask> = Vec::new();
    for d in devs {
        let base = || Accel {
            kind: d.kind.clone(),
            device: d.index,
            name: d.name.clone(),
            backend: String::new(),
            encoders: Vec::new(),
        };
        match d.vendor.as_str() {
            "nvidia" => {
                for e in encs.iter().filter(|e| e.ends_with("_nvenc")) {
                    let mut a = base();
                    a.backend = "nvenc".into();
                    let mut env = vec![(
                        "CUDA_VISIBLE_DEVICES".to_string(),
                        d.index.to_string(),
                    )];
                    if let Some(lib) = nvidia_libdir() {
                        env.push(("LD_LIBRARY_PATH".to_string(), lib));
                    }
                    tasks.push(ProbeTask {
                        enc: e.clone(),
                        tag: format!("nvenc{}", d.index),
                        vf: "format=nv12".into(),
                        extra_args: vec![],
                        env,
                        accel: a,
                    });
                }
                if let Some(node) = &d.render_node {
                    for e in encs.iter().filter(|e| e.ends_with("_vaapi")) {
                        let mut a = base();
                        a.backend = "vaapi".into();
                        tasks.push(ProbeTask {
                            enc: e.clone(),
                            tag: format!("vaapi{}", d.index),
                            vf: "format=nv12,hwupload".into(),
                            extra_args: vec!["-vaapi_device".into(), node.clone()],
                            env: vec![],
                            accel: a,
                        });
                    }
                }
            }
            "intel" => {
                // Linux: pin the render node. Windows: QSV finds its own device.
                let on_windows = os == "windows";
                if d.render_node.is_some() || on_windows {
                    let node = d.render_node.clone();
                    for backend in ["qsv", "vaapi"] {
                        // VA-API only exists as a backend on Linux.
                        if backend == "vaapi" && node.is_none() {
                            continue;
                        }
                        for e in encs.iter().filter(|e| e.ends_with(&format!("_{backend}"))) {
                            let mut a = base();
                            a.backend = backend.into();
                            let mut extra = vec![];
                            if let Some(n) = &node {
                                let flag = if backend == "qsv" { "-qsv_device" } else { "-vaapi_device" };
                                extra = vec![flag.into(), n.clone()];
                            }
                            tasks.push(ProbeTask {
                                enc: e.clone(),
                                tag: format!("{backend}{}{}", d.index, if on_windows { "w" } else { "" }),
                                vf: if backend == "vaapi" { "format=nv12,hwupload".into() } else { "format=nv12".into() },
                                extra_args: extra,
                                env: vec![],
                                accel: a,
                            });
                        }
                    }
                }
            }
            "amd" => {
                // AMF (Windows) — the backend picks the default AMD GPU.
                for e in encs.iter().filter(|e| e.ends_with("_amf")) {
                    let mut a = base();
                    a.backend = "amf".into();
                    tasks.push(ProbeTask {
                        enc: e.clone(),
                        tag: format!("amf{}", d.index),
                        vf: "format=nv12".into(),
                        extra_args: vec![],
                        env: vec![],
                        accel: a,
                    });
                }
            }
            "apple" => {
                // VideoToolbox (macOS) — h264_vt / hevc_vt / mjpeg_vt (old
                // ffmpeg) or h264_videotoolbox / hevc_videotoolbox / ... (new
                // ffmpeg renamed them). No device pinning needed.
                for e in encs.iter().filter(|e| e.ends_with("_vt") || e.contains("videotoolbox")) {
                    let mut a = base();
                    a.backend = "videotoolbox".into();
                    tasks.push(ProbeTask {
                        enc: e.clone(),
                        tag: format!("vt{}", d.index),
                        vf: "format=nv12".into(),
                        extra_args: vec![],
                        env: vec![],
                        accel: a,
                    });
                }
            }
            _ => {}
        }
    }
    tasks
}

fn detect_ffmpeg() -> Caps {
    // version
    let version = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.to_string()))
        .unwrap_or_else(|| "ffmpeg not found".into());

    // decoding HW-accel methods (informational)
    let mut haccels = vec![];
    if let Ok(o) = std::process::Command::new("ffmpeg").args(["-hide_banner", "-hwaccels"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let t = line.trim();
            if t.is_empty() || t.to_lowercase().contains("hardware") {
                continue;
            }
            if !t.contains(' ') {
                haccels.push(t.to_string());
            }
        }
    }

    // candidate HW encoders (2nd field of each `-encoders` line)
    let mut candidates: Vec<String> = vec![];
    if let Ok(o) = std::process::Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let name = line.split_whitespace().nth(1).unwrap_or("");
            if !name.is_empty() && is_hw_encoder(name) {
                candidates.push(name.to_string());
            }
        }
    }

    // PROBE every (device, backend, encoder) combination, then group the
    // winners per physical device: GPU #0, CPU #0, ...
    //
    // VA-API/QSV probes share ONE DRM device and the VA driver doesn't like
    // parallel contexts (probes drop out flakily under contention), so probes
    // for one device+backend run SEQUENTIALLY while different devices
    // (NVIDIA dGPU vs Intel iGPU) still probe in parallel.
    let gpus = enumerate_gpus();
    // Containerized NAS nodes: the host NVIDIA driver version is invisible
    // from inside, so pick the bundled userland series that can actually
    // init CUDA before any NVENC probe (see pick_nvidia_series).
    pick_nvidia_series();
    dlog(format!("encoder candidates: {candidates:?}"));
    dlog(format!(
        "gpus found: {:?}",
        gpus.iter().map(|g| (g.kind.as_str(), g.index, g.name.as_str())).collect::<Vec<_>>()
    ));
    let all = build_probe_tasks(&candidates, &gpus);
    let mut groups: Vec<(String, Vec<ProbeTask>)> = Vec::new();
    for t in all {
        let key = format!("{}|{}|{}", t.accel.kind, t.accel.device, t.accel.backend);
        if let Some(g) = groups.iter_mut().find(|(k, _)| *k == key) {
            g.1.push(t);
        } else {
            groups.push((key, vec![t]));
        }
    }
    let results: Vec<(Accel, String, bool)> = std::thread::scope(|s| {
        let handles = groups
            .iter()
            .map(|(_k, ts)| {
                s.spawn(|| {
                    ts.iter()
                        .map(|t| {
                            (
                                t.accel.clone(),
                                t.enc.clone(),
                                probe_hw_encoder(&t.enc, &t.tag, &t.vf, &t.extra_args, &t.env),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect()
    });

    let mut accels: Vec<Accel> = Vec::new();
    for (a, enc, ok) in results.into_iter().filter(|(_, _, ok)| *ok) {
        let slot = accels
            .iter_mut()
            .find(|s| s.kind == a.kind && s.device == a.device && s.backend == a.backend);
        match slot {
            Some(s) => {
                if !s.encoders.contains(&enc) {
                    s.encoders.push(enc);
                }
            }
            None => {
                accels.push(Accel { encoders: vec![enc], ..a });
            }
        }
    }
    accels.sort_by(|a, b| {
        a.kind.cmp(&b.kind).then_with(|| a.device.cmp(&b.device)).then_with(|| a.backend.cmp(&b.backend))
    });
    for a in accels.iter_mut() {
        a.encoders.sort();
    }

    // Flat list for the server's dispatch logic (it ranks nvenc > vaapi > qsv).
    let mut hw: Vec<String> = accels.iter().flat_map(|a| a.encoders.iter().cloned()).collect();
    hw.sort();
    hw.dedup();

    Caps { ffmpeg_version: version, haccels, hw_encoders: hw, accels }
}

// ============================================================
//  job model (what the server sends us) + local tracking
// ============================================================

#[derive(serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RemoteJob {
    id: String,
    input_file: String,
    output_file: String,
    video_codec: String,
    effective_video_codec: String,
    audio_codec: String,
    container: String,
    #[serde(default)]
    crf: Option<u32>,
    /// Target video bitrate in kbps (server computed it from the source's
    /// bitrate and the user's "% of source" setting). Wins over crf.
    #[serde(default)]
    video_bitrate: Option<u32>,
    /// Multi-GPU: the physical device (0-based index within its kind, as
    /// reported in our `accels`) the server pinned this encode to.
    #[serde(default)]
    gpu_device: Option<u32>,
}

/// Multi-GPU device pinning: the server picked the concrete encoder AND a
/// physical device for this job. We re-derive how to pin ffmpeg to that
/// device:
///   nvenc  -> CUDA_VISIBLE_DEVICES=<nvidia-smi index>
///   vaapi  -> -vaapi_device <render node>
///   qsv    -> -qsv_device <render node> (Linux; Windows picks its own)
/// When several devices share the same index across kinds (a dGPU #0 and an
/// iGPU #0), the reported accel entry for (device, backend, encoder) breaks
/// the tie. Returns (extra env vars, extra args) — both empty when there is
/// nothing to pin (software, single-device box, unknown device).
fn pin_device(job: &RemoteJob, eff: &str, caps: &Caps) -> (Vec<(&'static str, String)>, Vec<String>) {
    let Some(dev) = job.gpu_device else { return (vec![], vec![]) };
    let backend = eff.rsplit('_').next().unwrap_or("").to_string();
    if !is_hw_encoder(eff) {
        return (vec![], vec![]);
    }
    let acc = caps
        .accels
        .iter()
        .find(|a| a.device == dev && a.backend == backend && a.encoders.iter().any(|e| e == eff));
    let kind = acc.map(|a| a.kind.clone()).unwrap_or_else(|| "gpu".to_string());
    let d = enumerate_gpus().into_iter().find(|d| d.index == dev && d.kind == kind);
    match backend.as_str() {
        "nvenc" => match d {
            Some(d) => {
                let mut e = vec![("CUDA_VISIBLE_DEVICES", d.index.to_string())];
                if let Some(lib) = nvidia_libdir() {
                    e.push(("LD_LIBRARY_PATH", lib));
                }
                (e, vec![])
            }
            None => (vec![], vec![]),
        },
        "vaapi" | "qsv" => {
            let node = d.and_then(|d| d.render_node);
            match node {
                Some(n) => {
                    let flag = if backend == "qsv" { "-qsv_device" } else { "-vaapi_device" };
                    (vec![], vec![flag.to_string(), n])
                }
                None => (vec![], vec![]),
            }
        }
        // videotoolbox / mft / amf: single-device or backend-managed.
        _ => (vec![], vec![]),
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalJob {
    id: String,
    input_file: String,
    output_file: String,
    video_codec: String,
    status: String,
    progress: f64,
    error: Option<String>,
    finished_at: Option<i64>,
}

#[derive(Default)]
struct NodeState {
    caps: Caps,
    current: Option<LocalJob>,
    jobs: Vec<LocalJob>,
}

/// Render node to pin `-vaapi_device` to for VA-API jobs: the first Intel
/// iGPU's node, else the first render node at all. Without an explicit
/// device, ffmpeg's `hwupload` filter has no device context and the encoder
/// fails ("A hardware device reference is required").
fn pick_vaapi_device() -> Option<String> {
    let devs = enumerate_gpus();
    devs.iter()
        .find(|d| d.vendor == "intel" && d.render_node.is_some())
        .or_else(|| devs.iter().find(|d| d.render_node.is_some()))
        .and_then(|d| d.render_node.clone())
}

type NState = Arc<Mutex<NodeState>>;

fn push_job(s: &mut NodeState, j: LocalJob) {
    s.current = Some(j.clone());
    s.jobs.insert(0, j);
    if s.jobs.len() > 100 {
        s.jobs.truncate(100);
    }
}
fn update_job(s: &mut NodeState, id: &str, f: impl FnOnce(&mut LocalJob)) {
    if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
        f(j);
        if j.status == "done" || j.status == "error" {
            if s.current.as_ref().map(|c| c.id.as_str()) == Some(id) {
                s.current = None;
            }
        }
    }
}

// ============================================================
//  ffmpeg helpers (same progress logic as the server)
// ============================================================

async fn probe_duration(path: &str) -> f64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1", path])
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    out
}

/// av1_nvenc on Blackwell (driver 595.x) only accepts 4:2:0 input. If the
/// source is 4:4:4/4:2:2/RGB, ffmpeg negotiates YUV444P and the encoder is
/// rejected with "YUV444P not supported" -> "No capable devices found".
/// Return a format filter that forces NV12 when needed.
async fn nvenc_av1_fmt_guard(encoder: &str, path: &str) -> Vec<String> {
    if encoder != "av1_nvenc" {
        return Vec::new();
    }
    let pf = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=pix_fmt",
            "-of", "default=nw=1:nk=1",
            path,
        ])
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().lines().next().unwrap_or("").to_string())
        .unwrap_or_default();
    let ok_420 = matches!(pf.as_str(), "yuv420p" | "nv12" | "p010le" | "yuv420p10le" | "" | "N/A");
    if ok_420 {
        Vec::new()
    } else {
        vec!["-vf".into(), "format=nv12".into()]
    }
}

fn parse_time(s: &str) -> Option<f64> {
    let key = "time=";
    let idx = s.find(key)? + key.len();
    let rest = &s[idx..];
    let rest = rest.split_whitespace().next()?;
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// Byte stream over a file on disk — feeds reqwest's multipart `Part::stream`
/// so a multi-GB finished output is uploaded from disk, never read whole
/// into RAM.
struct FileBytes {
    file: Option<tokio::fs::File>,
    buf: Vec<u8>,
}

impl FileBytes {
    fn new(path: &std::path::Path) -> Self {
        let file = std::fs::File::open(path)
            .ok()
            .map(tokio::fs::File::from_std);
        Self { file, buf: vec![0u8; 1 << 20] }
    }
}

impl futures_util::Stream for FileBytes {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use tokio::io::AsyncRead;
        let this = self.get_mut();
        match this.file.as_mut() {
            None => std::task::Poll::Ready(None),
            Some(f) => {
                this.buf.clear();
                // ReadBuf borrows this.buf — capture the read result and the
                // filled length inside one scope so the borrow ends before
                // we touch the buffer again.
                let (res, n) = {
                    let mut rb = tokio::io::ReadBuf::new(&mut this.buf);
                    let r = std::pin::Pin::new(f).poll_read(cx, &mut rb);
                    (r, rb.filled().len())
                };
                match res {
                    std::task::Poll::Ready(Ok(_)) => {
                        if n == 0 {
                            this.file = None;
                            std::task::Poll::Ready(None)
                        } else {
                            this.buf.truncate(n);
                            std::task::Poll::Ready(Some(Ok(this.buf.clone().into())))
                        }
                    }
                    std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Some(Err(e))),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            }
        }
    }
}

// ============================================================
//  run a job locally
// ============================================================

/// Report an error to the server and record it locally — pushing the job if
/// the failure happened before the local "running" push, updating it after.
async fn fail_job(
    client: &reqwest::Client,
    state: &NState,
    server: &str,
    node_id: &str,
    job: &RemoteJob,
    msg: String,
) {
    let _ = client
        .post(format!("{}/api/nodes/{}/report", server, node_id))
        .json(&json!({"jobId": job.id, "status": "error", "error": msg.clone()}))
        .send()
        .await;
    let mut st = state.lock().unwrap();
    if st.jobs.iter().any(|j| j.id == job.id) {
        update_job(&mut st, &job.id, |j| {
            j.status = "error".into();
            j.error = Some(msg);
            j.finished_at = Some(now_ms());
        });
    } else {
        push_job(
            &mut st,
            LocalJob {
                id: job.id.clone(),
                input_file: job.input_file.clone(),
                output_file: job.output_file.clone(),
                video_codec: job.effective_video_codec.clone(),
                status: "error".into(),
                progress: 0.0,
                error: Some(msg),
                finished_at: Some(now_ms()),
            },
        );
    }
}

async fn run_job(client: reqwest::Client, state: NState, server: String, node_id: String, job: RemoteJob) {
    let in_path = in_dir().join(&job.input_file);
    let out_path = out_dir().join(&job.output_file);

    // File transfers (multi-GB in both directions) get their own client with
    // a 30-minute timeout: the shared client's 30s cap is right for
    // control-plane calls but would kill a big transfer mid-stream.
    let xfer = reqwest::Client::builder()
        .default_headers(api_auth_headers())
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .unwrap();

    // 1. download input from server — streamed straight to disk (a 16GB
    // library source must never sit in RAM).
    let url = format!("{}/api/nodes/{}/file/{}", server, node_id, job.input_file);
    let dl = xfer.get(&url).send().await;
    let dl = match dl {
        Ok(r) if r.status().is_success() => r,
        _ => {
            fail_job(&client, &state, &server, &node_id, &job, "failed to download input from server".into()).await;
            return;
        }
    };
    // A failed body download (torn connection) or a size mismatch vs
    // Content-Length must abort the job — proceeding would encode a missing
    // or STALE input left in in/ by a previous run and upload a bogus
    // "done" result.
    let expected = dl.content_length();
    let mut dl_err: Option<String> = None;
    // input_file may be a library-relative path with subdirectories
    // (e.g. "subdir/movie.mp4") — make sure the parent exists first.
    if let Some(p) = in_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    match tokio::fs::File::create(&in_path).await {
        Ok(mut file) => {
            use futures_util::StreamExt;
            let mut stream = dl.bytes_stream();
            let mut written: u64 = 0;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(b) => {
                        if let Err(e) = file.write_all(&b).await {
                            dl_err = Some(format!("failed to write input to disk: {e}"));
                            break;
                        }
                        written += b.len() as u64;
                    }
                    Err(e) => {
                        dl_err = Some(format!("failed to download input body: {e}"));
                        break;
                    }
                }
            }
            if dl_err.is_none() {
                if let Err(e) = file.flush().await {
                    dl_err = Some(format!("failed to finish input download: {e}"));
                } else if let Some(cl) = expected {
                    if written != cl {
                        dl_err = Some(format!(
                            "input download truncated: wrote {written} bytes, server sent {cl}"
                        ));
                    }
                }
            }
        }
        Err(e) => dl_err = Some(format!("failed to create input file: {e}")),
    }
    if let Some(msg) = dl_err {
        let _ = std::fs::remove_file(&in_path);
        fail_job(&client, &state, &server, &node_id, &job, msg).await;
        return;
    }

    // track locally + report running
    {
        let mut st = state.lock().unwrap();
        push_job(&mut st, LocalJob {
            id: job.id.clone(),
            input_file: job.input_file.clone(),
            output_file: job.output_file.clone(),
            video_codec: job.effective_video_codec.clone(),
            status: "running".into(),
            progress: 0.0,
            error: None,
            finished_at: None,
        });
    }
    let _ = client
        .post(format!("{}/api/nodes/{}/report", server, node_id))
        .json(&json!({"jobId": job.id, "status": "running"}))
        .send().await;

    // 2. build ffmpeg args
    let eff = job.effective_video_codec.clone();
    // Multi-GPU: server pinned a physical device for this encode (nvenc ->
    // CUDA_VISIBLE_DEVICES, vaapi/qsv -> the matching render node).
    let (pin_env, pin_args) = {
        let caps = state.lock().unwrap().caps.clone();
        pin_device(&job, &eff, &caps)
    };
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-i".into(),
        in_path.to_string_lossy().to_string(),
    ];
    // av1_nvenc guard: force NV12 for 4:4:4/4:2:2/RGB sources (see fn docs).
    let guard = nvenc_av1_fmt_guard(&eff, &in_path.to_string_lossy()).await;
    args.extend(guard);
    // VA-API encoders need an explicit hwupload: ffmpeg's auto_scale can't
    // convert sw frames to the VA-API hw format on this build (ENOSYS) — and
    // hwupload in turn needs a device reference, so pin -vaapi_device too.
    if eff.ends_with("_vaapi") {
        args.push("-vf".into());
        args.push("format=nv12,hwupload".into());
        if pin_args.is_empty() {
            if let Some(dev) = pick_vaapi_device() {
                args.push("-vaapi_device".into());
                args.push(dev);
            }
        }
    }
    args.extend(pin_args);
    args.push("-c:v".into());
    args.push(eff.clone());
    if let Some(kbps) = job.video_bitrate {
        // Capped VBR ("% of source" mode): -b:v sets the target and
        // -maxrate/-bufsize hard-cap the output, so a misconfigured
        // percentage can't bloat the file past its target.
        args.push("-b:v".into());
        args.push(format!("{kbps}k"));
        args.push("-maxrate".into());
        args.push(format!("{}k", (kbps as u64) * 120 / 100)); // 1.2x target
        args.push("-bufsize".into());
        args.push(format!("{}k", (kbps as u64) * 2)); // 2x target
    } else {
        let crf_eligible = ["libx264", "libx265", "libvpx-vp9", "libvpx", "libsvtav1", "libaom-av1"].contains(&eff.as_str());
        if crf_eligible {
            if let Some(crf) = job.crf {
                args.push("-crf".into());
                args.push(crf.to_string());
            }
        }
    }
    // Per-encoder preset — each family accepts different names. libsvtav1
    // takes ints (0..=13), NVENC takes p1..p7, x264/x265/vpx take
    // "veryfast". Others (vaapi/amf/...) don't share a common preset, so
    // we leave them at their default.
    let preset: Option<&str> = if eff == "libsvtav1" {
        Some("4")
    } else if eff.ends_with("_nvenc") {
        Some("p5")
    } else if eff == "libx264" || eff == "libx265" || eff == "libvpx" || eff == "libvpx-vp9" {
        Some("veryfast")
    } else {
        None
    };
    if let Some(p) = preset {
        args.push("-preset".into());
        args.push(p.into());
    }
    args.push("-c:a".into());
    // The built-in `opus` encoder is flagged experimental in this ffmpeg
    // build (fails without -strict -2); libopus is the stable one.
    let acodec = if job.audio_codec == "opus" { "libopus" } else { job.audio_codec.as_str() };
    args.push(acodec.to_string());
    if job.audio_codec == "aac" {
        args.push("-b:a".into());
        args.push("128k".into());
    }
    if job.container == "webm" {
        args.push("-f".into());
        args.push("webm".into());
    }
    args.push("-y".into());
    args.push(out_path.to_string_lossy().to_string());

    let total = probe_duration(&in_path.to_string_lossy()).await;

    let mut child = match Command::new("ffmpeg").args(&args).envs(pin_env.into_iter()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("spawn ffmpeg failed: {e}");
            let _ = client.post(format!("{}/api/nodes/{}/report", server, node_id))
                .json(&json!({"jobId": job.id, "status": "error", "error": msg})).send().await;
            let mut st = state.lock().unwrap();
            update_job(&mut st, &job.id, |j| { j.status = "error".into(); j.error = Some(msg); j.finished_at = Some(now_ms()); });
            return;
        }
    };

    // 3. read stderr, update progress.
    // ffmpeg writes progress with '\r' (same-line overwrite), not '\n'.
    // Line-based reading lumps every update into one line at the end ->
    // progress stuck at 0. Read in chunks, split on both '\r' and '\n'.
    use tokio::io::AsyncReadExt;
    let mut last_err: Option<String> = None;
    if let Some(mut s) = child.stderr.take() {
        let mut buf = [0u8; 8192];
        let mut pending = String::new();
        let mut last_report = 0.0f64;
        loop {
            let n = match s.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            pending.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(pos) = pending.find(|c| c == '\r' || c == '\n') {
                let seg = pending[..pos].to_string();
                pending = pending[pos + 1..].to_string();
                if seg.is_empty() {
                    continue;
                }
                if let Some(t) = parse_time(&seg) {
                    let prog = if total > 0.0 {
                        (t / total).clamp(0.0, 0.999)
                    } else {
                        0.0
                    };
                    {
                        let mut st = state.lock().unwrap();
                        update_job(&mut st, &job.id, |j| j.progress = prog);
                    } // lock dropped before any await
                    if (prog - last_report) >= 0.05 {
                        last_report = prog;
                        let _ = client
                            .post(format!("{}/api/nodes/{}/report", server, node_id))
                            .json(&json!({"jobId": job.id, "status": "running", "progress": prog}))
                            .send()
                            .await;
                    }
                }
                if seg.contains("Error")
                    || seg.contains("Invalid")
                    || seg.contains("does not contain any stream")
                {
                    last_err = Some(seg.trim().to_string());
                }
            }
        }
        // flush trailing segment
        if !pending.is_empty() {
            if let Some(t) = parse_time(&pending) {
                let prog = if total > 0.0 {
                    (t / total).clamp(0.0, 0.999)
                } else {
                    0.0
                };
                {
                    let mut st = state.lock().unwrap();
                    update_job(&mut st, &job.id, |j| j.progress = prog);
                }
            }
            if pending.contains("Error")
                || pending.contains("Invalid")
                || pending.contains("does not contain any stream")
            {
                last_err = Some(pending.trim().to_string());
            }
        }
    }
    let ok = child.wait().await.map(|s| s.success()).unwrap_or(false);

    // Wait for the output file's size to stabilize before reading it:
    // some hosts report a lagging on-disk size right after ffmpeg closes
    // the file (write-size metadata lag) — reading too early would upload
    // a truncated artifact.
    if ok {
        let mut last = 0u64;
        let mut stable = 0u32;
        for _ in 0..40 {
            let s = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
            if s > 0 && s == last {
                stable += 1;
                if stable >= 2 {
                    break;
                }
            } else {
                stable = 0;
            }
            last = s;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // 4. upload result + report — the file part streams from disk (never
    // held in RAM), over the long-timeout transfer client.
    if ok {
        if let Ok(size) = std::fs::metadata(&out_path).map(|m| m.len()) {
            let form = reqwest::multipart::Form::new()
                .part("jobId", reqwest::multipart::Part::text(job.id.clone()))
                .part("outputFile", reqwest::multipart::Part::text(job.output_file.clone()))
                .part("file", reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(FileBytes::new(&out_path))).file_name(job.output_file.clone()));
            let up = xfer.post(format!("{}/api/nodes/{}/result", server, node_id)).multipart(form).send().await;
            match up {
                Ok(r) if r.status().is_success() => {
                    let _ = client.post(format!("{}/api/nodes/{}/report", server, node_id))
                        .json(&json!({"jobId": job.id, "status": "done", "progress": 1.0, "outputFile": job.output_file, "outputSize": size}))
                        .send().await;
                    let mut st = state.lock().unwrap();
                    update_job(&mut st, &job.id, |j| { j.status = "done".into(); j.progress = 1.0; j.finished_at = Some(now_ms()); });
                }
                Ok(r) => {
                    let msg = format!("result upload failed: HTTP {}", r.status());
                    let _ = client.post(format!("{}/api/nodes/{}/report", server, node_id))
                        .json(&json!({"jobId": job.id, "status": "error", "error": msg})).send().await;
                    let mut st = state.lock().unwrap();
                    update_job(&mut st, &job.id, |j| { j.status = "error".into(); j.error = Some(msg); j.finished_at = Some(now_ms()); });
                }
                Err(e) => {
                    let msg = format!("result upload failed: {e}");
                    let _ = client.post(format!("{}/api/nodes/{}/report", server, node_id))
                        .json(&json!({"jobId": job.id, "status": "error", "error": msg})).send().await;
                    let mut st = state.lock().unwrap();
                    update_job(&mut st, &job.id, |j| { j.status = "error".into(); j.error = Some(msg); j.finished_at = Some(now_ms()); });
                }
            }
        } else {
            let msg = "could not read output file".into();
            let _ = client.post(format!("{}/api/nodes/{}/report", server, node_id))
                .json(&json!({"jobId": job.id, "status": "error", "error": msg})).send().await;
            let mut st = state.lock().unwrap();
            update_job(&mut st, &job.id, |j| { j.status = "error".into(); j.error = Some(msg); j.finished_at = Some(now_ms()); });
        }
    } else {
        let msg = last_err.unwrap_or_else(|| "ffmpeg exited non-zero".into());
        let _ = client.post(format!("{}/api/nodes/{}/report", server, node_id))
            .json(&json!({"jobId": job.id, "status": "error", "error": msg})).send().await;
        let mut st = state.lock().unwrap();
        update_job(&mut st, &job.id, |j| { j.status = "error".into(); j.error = Some(msg); j.finished_at = Some(now_ms()); });
    }
}

// ============================================================
//  server loops: register / heartbeat / poll
// ============================================================

/// Shared, updatable node id (re-registered after a server restart).
type SharedId = Arc<Mutex<String>>;

/// Register (or re-register) with the server; stores the id into `id`.
/// Returns true on success.
async fn do_register(client: &reqwest::Client, server: &str, name: &str, caps: &Caps, id: &SharedId) -> bool {
    let body = json!({
        "name": name,
        "ffmpegVersion": caps.ffmpeg_version,
        "haccels": caps.haccels,
        "hwEncoders": caps.hw_encoders,
        "accels": caps.accels,
        "diag": DIAG.lock().unwrap().clone(),
    });
    let resp = client.post(format!("{}/api/nodes/register", server)).json(&body).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(nid) = v.get("nodeId").and_then(|x| x.as_str()) {
                    *id.lock().unwrap() = nid.to_string();
                }
            }
            true
        }
        Ok(r) => {
            let st = r.status();
            let txt = r.text().await.unwrap_or_default();
            println!("[node] register HTTP {}: {}", st, txt.chars().take(300).collect::<String>());
            false
        }
        Err(e) => {
            println!("[node] register transport error: {}", e);
            false
        }
    }
}

/// Browse the LAN for a CodecTrans server (mDNS) and return its base URL.
/// Reads the server's IP from its address records (IPv4 preferred) and port
/// from the SRV record. Returns None if nothing is found within the window.
async fn discover_server(daemon: &ServiceDaemon) -> Option<String> {
    let mut rx = daemon.browse("_codectrans-server._tcp.local.").ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let ev = tokio::time::timeout(remaining, rx.recv_async()).await;
        let ev = match ev {
            Ok(Ok(e)) => e,
            _ => return None,
        };
        if let ServiceEvent::ServiceResolved(svc) = ev {
            let svc: &ResolvedService = &svc;
            if !svc.is_valid() {
                continue;
            }
            let ip = svc
                .get_addresses_v4()
                .iter()
                .next()
                .map(|a| a.to_string())
                .or_else(|| txt_str(svc, "ip"));
            if let Some(ip) = ip {
                return Some(format!("http://{}:{}", ip, svc.get_port()));
            }
        }
    }
}

/// Read a TXT property value as a UTF-8 string (best-effort).
fn txt_str(svc: &ResolvedService, key: &str) -> Option<String> {
    svc.get_property_val(key)
        .flatten()
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

/// A fresh HTTP client (new connection pool). Used to recover after a
/// transport error: a pooled keep-alive connection left over from a
/// restarted server would otherwise be reused by every retry.
fn fresh_client() -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers(api_auth_headers())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Bearer token for the server API (CT_API_TOKEN). Empty map when unset, so
/// local/dev setups need no config; nodes MUST mirror the server's token or
/// every control call will 401.
fn api_auth_headers() -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Ok(tok) = std::env::var("CT_API_TOKEN") {
        if !tok.is_empty() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {tok}")) {
                h.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
    }
    h
}

// ============================================================
//  live stats (CPU / RAM / GPU) — sampled per heartbeat
// ============================================================

/// Sample host stats. Runs on a blocking thread (some samples spawn a short
/// external command); callers bound it with a timeout. Returns None fields
/// when a metric can't be read on this OS (e.g. GPU temp on macOS needs
/// root, so it's reported as absent).
fn sample_stats() -> Value {
    let mut v = json!({});
    #[cfg(target_os = "linux")]
    {
        if let Some(pct) = linux_cpu_pct() {
            v["cpuPct"] = json!(pct);
        }
        if let Some((used, total)) = linux_ram_mb() {
            v["ramUsedMb"] = json!(used);
            v["ramTotalMb"] = json!(total);
        }
        if let Some((temp, util)) = nvidia_gpu_stats() {
            v["gpuTemp"] = json!(temp);
            v["gpuUtil"] = json!(util);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(pct) = macos_cpu_pct() {
            v["cpuPct"] = json!(pct);
        }
        if let Some((used, total)) = macos_ram_mb() {
            v["ramUsedMb"] = json!(used);
            v["ramTotalMb"] = json!(total);
        }
    }
    v
}

/// CPU% since the previous sample (needs two /proc/stat reads ~3s apart).
#[cfg(target_os = "linux")]
fn linux_cpu_pct() -> Option<f64> {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static PREV: OnceLock<Mutex<(u64, u64)>> = OnceLock::new();
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().next()?;
    let mut it = line.split_whitespace();
    it.next()?; // "cpu"
    let mut total = 0u64;
    let mut idle = 0u64;
    for (i, f) in it.enumerate() {
        let v: u64 = f.parse().ok()?;
        total += v;
        if i == 3 || i == 4 {
            idle += v; // idle + iowait
        }
    }
    let prev = PREV.get_or_init(|| Mutex::new((0, 0)));
    let mut g = prev.lock().ok()?;
    let pct = if g.0 > 0 && total > g.0 {
        let dtotal = total - g.0;
        let didle = idle.saturating_sub(g.1);
        100.0 * (dtotal.saturating_sub(didle)) as f64 / dtotal as f64
    } else {
        0.0
    };
    *g = (total, idle);
    Some(pct)
}

#[cfg(target_os = "linux")]
fn linux_ram_mb() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let get = |key: &str| -> Option<u64> {
        s.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
    };
    let total_kb = get("MemTotal:")?;
    let avail_kb = get("MemAvailable:")?;
    Some((total_kb.saturating_sub(avail_kb) / 1024, total_kb / 1024))
}

/// NVIDIA GPU temp/util via nvidia-smi (first GPU). None when not NVIDIA.
#[cfg(target_os = "linux")]
fn nvidia_gpu_stats() -> Option<(u64, u64)> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?;
    let mut parts = line.split(',');
    let temp = parts.next()?.trim().parse::<u64>().ok()?;
    let util = parts.next()?.trim().parse::<u64>().ok()?;
    Some((temp, util))
}

#[cfg(target_os = "macos")]
fn macos_cpu_pct() -> Option<f64> {
    let out = std::process::Command::new("top").args(["-l", "1", "-n", "0"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().find(|l| l.starts_with("CPU usage"))?;
    // "CPU usage: 2.30% user, 1.00% sys, 96.69% idle"
    let nums: Vec<f64> = line
        .split('%')
        .filter_map(|p| p.trim().rsplit_once(' ').and_then(|(_, n)| n.parse().ok()))
        .collect();
    // first three numbers: user, sys, idle
    let idle = nums.get(2)?;
    Some((100.0 - idle).clamp(0.0, 100.0))
}

#[cfg(target_os = "macos")]
fn macos_ram_mb() -> Option<(u64, u64)> {
    let run = |cmd: &str, arg: &str| -> Option<String> {
        let o = std::process::Command::new(cmd).arg(arg).output().ok()?;
        if !o.status.success() {
            return None;
        }
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let total_bytes: u64 = run("sysctl", "-n hw.memsize")?.parse().ok()?;
    let page_bytes: u64 = run("sysctl", "-n hw.pagesize")?.parse().ok()?;
    let vm = run("vm_stat", "")?;
    let pages = |key: &str| -> u64 {
        vm.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|n| n.trim().trim_end_matches('.') .parse::<u64>().ok())
            .unwrap_or(0)
    };
    let free = pages("Pages free");
    let inactive = pages("Pages inactive");
    let spec = pages("Pages speculative");
    let total_pages = total_bytes / page_bytes;
    let used_pages = total_pages.saturating_sub(free + inactive + spec);
    Some((used_pages * page_bytes / 1024 / 1024, total_bytes / 1024 / 1024))
}

/// Heartbeat loop: liveness + live stats (CPU/RAM/GPU) every ~3s.
async fn heartbeat_loop(mut client: reqwest::Client, server: String, name: String, caps: Caps, id: SharedId) {
    loop {
        let nid = id.lock().unwrap().clone();
        // Stats can spawn short external commands (top -l 1, nvidia-smi) —
        // keep them off the async runtime and bounded by a timeout.
        let stats = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::task::spawn_blocking(|| sample_stats()),
        )
        .await
        {
            Ok(Ok(s)) => s,
            _ => json!({}),
        };
        let resp = client
            .post(format!("{}/api/nodes/{}/heartbeat", server, nid))
            .json(&stats)
            .send()
            .await;
        match resp {
            Ok(r) => {
                // 404 = unknown node (server restarted) → re-register
                if r.status() == reqwest::StatusCode::NOT_FOUND {
                    let _ = do_register(&client, &server, &name, &caps, &id).await;
                }
            }
            // Transport failure: a stale keep-alive connection from a
            // restarted server would be reused by every retry and keep
            // timing out. Swap in a fresh client (new pool).
            Err(_) => client = fresh_client(),
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Long-poll loop with a CONCURRENT worker pool: claims jobs while fewer
/// than `maxJobs` are running (the server pushes maxJobs in the poll
/// response; CT_MAX_JOBS seeds the default), spawns each claim as a task,
/// and waits for a slot to free up before polling again.
/// Re-registers automatically if the server no longer knows this node.
async fn poll_loop(
    mut client: reqwest::Client,
    server: String,
    name: String,
    caps: Caps,
    id: SharedId,
    state: NState,
) {
    let max_jobs: Arc<std::sync::atomic::AtomicU32> = Arc::new(std::sync::atomic::AtomicU32::new(
        std::env::var("CT_MAX_JOBS").ok().and_then(|s| s.parse().ok()).unwrap_or(1).clamp(1, 16),
    ));
    let active: Arc<std::sync::atomic::AtomicU32> = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let slot_free = Arc::new(tokio::sync::Notify::new());
    loop {
        // Claim while under capacity.
        loop {
            if active.load(std::sync::atomic::Ordering::Relaxed)
                >= max_jobs.load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }
            let nid = id.lock().unwrap().clone();
            let url = format!("{}/api/nodes/{}/poll", server, nid);
            let resp = client.get(&url).send().await;
            match resp {
                Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                    // unknown node (server restart) → re-register, then retry
                    println!("[node] not registered on server — re-registering");
                    let _ = do_register(&client, &server, &name, &caps, &id).await;
                    break;
                }
                Ok(r) => {
                    if let Ok(v) = r.json::<Value>().await {
                        // Server-pushed worker settings.
                        if let Some(cfg) = v.get("config") {
                            if let Some(m) = cfg.get("maxJobs").and_then(|x| x.as_u64()) {
                                max_jobs.store(m.clamp(1, 16) as u32, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        if let Some(jobv) = v.get("job").filter(|j| !j.is_null()) {
                            if let Ok(job) = serde_json::from_value::<RemoteJob>(jobv.clone()) {
                                println!("[node] claimed job {} ({} -> {})", job.id, job.input_file, job.effective_video_codec);
                                active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let client2 = client.clone();
                                let state2 = state.clone();
                                let server2 = server.clone();
                                let nid2 = id.lock().unwrap().clone();
                                let active2 = active.clone();
                                let free2 = slot_free.clone();
                                tokio::spawn(async move {
                                    run_job(client2, state2, server2, nid2, job).await;
                                    active2.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                    free2.notify_one();
                                });
                                continue; // claim the next one
                            }
                        }
                    }
                    break; // long-poll timed out with no job
                }
                Err(e) => {
                    let mut chain = String::new();
                    let mut src = std::error::Error::source(&e);
                    while let Some(s) = src {
                        chain.push_str(&format!(" <- {}", s));
                        src = s.source();
                    }
                    println!("[node] poll error: {} (retrying){}", e, chain);
                    // Swap in a fresh client (new pool) — a stale keep-alive /
                    // long-poll connection from a restarted server would
                    // otherwise be reused for every retry and keep timing out.
                    client = fresh_client();
                    break;
                }
            }
        }
        // Wait: a free slot (job finished) or a short idle tick.
        if active.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                slot_free.notified(),
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

// ============================================================
//  local inspection API
// ============================================================

async fn health(State(st): State<NState>) -> impl IntoResponse {
    let s = st.lock().unwrap();
    let cur = s.current.as_ref();
    Json(json!({
        "status": "ok",
        "service": "codectrans-node",
        "name": node_name(),
        "server": server_url(),
        "currentJob": cur.map(|c| c.id.clone()),
        "ffmpeg": s.caps.ffmpeg_version,
        "haccels": s.caps.haccels,
        "hwEncoderCount": s.caps.hw_encoders.len(),
        "time": now_ms(),
    }))
}

async fn capabilities(State(st): State<NState>) -> impl IntoResponse {
    let s = st.lock().unwrap();
    Json(json!({
        "ffmpegVersion": s.caps.ffmpeg_version,
        "haccels": s.caps.haccels,
        "hwEncoders": s.caps.hw_encoders,
        "accels": s.caps.accels,
    }))
}

async fn jobs(State(st): State<NState>) -> impl IntoResponse {
    let s = st.lock().unwrap();
    Json(Value::Array(s.jobs.iter().map(serde_json::to_value).collect::<Result<Vec<_>, _>>().unwrap_or_default()))
}

// ============================================================
//  main
// ============================================================

async fn node_stats() -> impl IntoResponse {
    // Sample on demand (bounded; spawns short external commands).
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::task::spawn_blocking(|| sample_stats()),
    )
    .await
    {
        Ok(Ok(s)) => Json(s),
        _ => Json(json!({})),
    }
}

#[tokio::main]
async fn main() {
    let name = node_name();
    let caps = detect_ffmpeg();
    let port = local_port();

    // mDNS: advertise this node on the LAN so a server can find it.
    let daemon = match ServiceDaemon::new() {
        Ok(d) => Some(d),
        Err(e) => {
            println!("[node] mDNS unavailable ({e}) — LAN discovery disabled");
            None
        }
    };
    if let Some(d) = &daemon {
        let ip = lan_ip();
        let mut props: HashMap<String, String> = HashMap::new();
        props.insert("ip".into(), ip.clone());
        props.insert("name".into(), name.clone());
        props.insert("hw".into(), caps.hw_encoders.join(","));
        props.insert("ffmpeg".into(), caps.ffmpeg_version.clone());
        match ServiceInfo::new(
            "_codectrans._tcp.local.",
            &name,
            &mdns_hostname(),
            ip.as_str(),
            port,
            props,
        ) {
            Ok(mut info) => {
                // Advertise only on the LAN interface — avoid docker/bridge/veth
                // and other non-LAN interfaces that can confuse mDNS resolution.
                if let Ok(ip_addr) = ip.parse::<IpAddr>() {
                    info.set_interfaces(vec![IfKind::Addr(ip_addr)]);
                }
                let info = info.enable_addr_auto();
                match d.register(info) {
                    Ok(()) => println!("[node] advertised on LAN (mDNS) as '{name}' @ {ip}:{port}"),
                    Err(e) => println!("[node] mDNS register failed: {e}"),
                }
            }
            Err(e) => println!("[node] mDNS ServiceInfo failed: {e}"),
        }
    }

    // Determine the server: an explicit CT_SERVER, or auto-discover on the LAN.
    let mut server = server_url();
    if server.is_empty() || server.eq_ignore_ascii_case("auto") {
        println!("[node] CT_SERVER=auto — discovering a server on the LAN (mDNS)");
        loop {
            let found = match &daemon {
                Some(d) => discover_server(d).await,
                None => None,
            };
            match found {
                Some(s) => {
                    println!("[node] discovered server at {s}");
                    server = s;
                    break;
                }
                None => {
                    eprintln!("[node] no server found yet — retrying in 3s (set CT_SERVER=<url> to skip)");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }
    }

    println!("[node] {} | server={} | ffmpeg={}", name, server, caps.ffmpeg_version);
    println!("[node] haccels={:?}", caps.haccels);
    println!("[node] hw_encoders={:?}", caps.hw_encoders);
    // GPU/driver diagnostics (docker logs + shipped to the server):
    match std::process::Command::new("nvidia-smi").args(["-L"]).output() {
        Ok(o) if o.status.success() => {
            dlog(format!("nvidia-smi: {}", String::from_utf8_lossy(&o.stdout).trim()))
        }
        Ok(o) => {
            dlog(format!(
                "nvidia-smi FAILED: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
        Err(e) => dlog(format!("nvidia-smi not runnable: {e}")),
    }
    // Workdir must be writable (probe files + job scratch land there):
    match std::fs::write(in_dir().join(".wtest"), b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(in_dir().join(".wtest"));
        }
        Err(e) => dlog(format!("WORKDIR NOT WRITABLE: {} ({e})", in_dir().display())),
    }
    // Container NVIDIA introspection (QNAP-style host-userland injection):
    // where are PATH/LD pointing, is the host's /usr/local/nvidia mounted,
    // what driver does the host's own nvidia-smi report, which CUDA libs
    // does the dynamic linker actually resolve?
    {
        let envdump = |k: &str| std::env::var(k).unwrap_or_else(|_| "<unset>".into());
        dlog(format!("env PATH={}", envdump("PATH")));
        dlog(format!("env LD_LIBRARY_PATH={}", envdump("LD_LIBRARY_PATH")));
        let mut parts = Vec::new();
        for d in ["/usr/local/nvidia/bin", "/usr/local/nvidia/lib64"] {
            parts.push(match std::fs::read_dir(d) {
                Ok(rd) => {
                    let names: Vec<String> = rd
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .take(10)
                        .collect();
                    format!("{d}=[{}]", names.join(","))
                }
                Err(_) => format!("{d}=<absent>"),
            });
        }
        dlog(parts.join(" "));
        for p in ["/usr/local/nvidia/bin/nvidia-smi", "nvidia-smi"] {
            if let Ok(o) = std::process::Command::new(p)
                .args(["--query-gpu=driver_version,name", "--format=csv,noheader"])
                .output()
            {
                if o.status.success() {
                    dlog(format!(
                        "nvidia-smi({p}): {}",
                        String::from_utf8_lossy(&o.stdout).trim()
                    ));
                }
            }
        }
        // The selected bundled series' nvidia-smi speaks the host driver's
        // version (it only works when the series matches the kernel driver).
        if let Some(lib) = nvidia_libdir() {
            let bin = std::path::Path::new(&lib).join("../bin/nvidia-smi");
            if let Ok(o) = std::process::Command::new(&bin)
                .env("LD_LIBRARY_PATH", &lib)
                .args(["--query-gpu=driver_version,name", "--format=csv,noheader"])
                .output()
            {
                if o.status.success() {
                    dlog(format!(
                        "host driver (via {} libs): {}",
                        std::path::Path::new(&lib)
                            .parent()
                            .and_then(|s| s.file_name())
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        String::from_utf8_lossy(&o.stdout).trim()
                    ));
                }
            }
        }
        if let Ok(o) = std::process::Command::new("sh")
            .args([
                "-c",
                "ldconfig -p 2>/dev/null | grep -E 'libcuda|libnvidia-encode' | head -8",
            ])
            .output()
        {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !t.is_empty() {
                dlog(format!("ldconfig: {}", t.replace('\n', " | ")));
            }
        }
        // Host-major series nvidia-smi — ALWAYS (even when the cuInit probe
        // failed): tells us whether the kernel RM is reachable from userland
        // at all, independently of CUDA context creation.
        if let Some(ver) = host_nvidia_driver_version() {
            if let Some(major) = nv_major(&ver) {
                let sdir = PathBuf::from(format!("/opt/nvidia-userland/{major}"));
                let bin = sdir.join("bin/nvidia-smi");
                if bin.exists() {
                    if let Ok(o) = std::process::Command::new(&bin)
                        .env("LD_LIBRARY_PATH", sdir.join("lib").to_string_lossy().as_ref())
                        .arg("-L")
                        .output()
                    {
                        let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        dlog(format!(
                            "series{major} nvidia-smi: exit={} out=[{}] err=[{}]",
                            o.status.code().unwrap_or(-1),
                            out.replace('\n', " | "),
                            err.replace('\n', " | ")
                        ));
                    }
                } else {
                    dlog(format!(
                        "series{major} nvidia-smi not available (host driver {ver} — bundled series: none)"
                    ));
                }
            }
        }
        // GPU device nodes inside the container:
        let devout = std::process::Command::new("sh")
            .args(["-c", "ls -l /dev/nvidia* 2>&1 | head -8"])
            .output();
        if let Ok(o) = devout {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !t.is_empty() {
                dlog(format!("dev nodes: {}", t.replace('\n', " | ")));
            }
        }
    }
    let accel_desc: String = if caps.hw_encoders.is_empty() {
        "CPU only".to_string()
    } else {
        caps.hw_encoders.join(", ")
    };

    let client = reqwest::Client::builder()
        .default_headers(api_auth_headers())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();

    // register (retry until the server is reachable); id is shared/updatable
    let id: SharedId = Arc::new(Mutex::new(String::new()));
    let caps0 = caps.clone();
    loop {
        if do_register(&client, &server, &name, &caps0, &id).await {
            break;
        }
        println!("[node] server not reachable at {} — retrying in 3s", server);
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    println!("[node] registered as {}", id.lock().unwrap());

    let caps_hb = caps.clone();
    let caps_poll = caps.clone();
    let state: NState = Arc::new(Mutex::new(NodeState { caps, current: None, jobs: vec![] }));

    // spawn loops (they re-register automatically if the server restarts)
    tokio::spawn(heartbeat_loop(client.clone(), server.clone(), name.clone(), caps_hb, id.clone()));
    tokio::spawn(poll_loop(client.clone(), server.clone(), name.clone(), caps_poll, id.clone(), state.clone()));

    // local API
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/capabilities", get(capabilities))
        .route("/api/jobs", get(jobs))
        .route("/api/stats", get(node_stats))
        .with_state(state);
    let bind = bind_addr();
    let port = local_port();
    let lan = lan_ip();
    println!();
    println!("[node] ── node '{name}' is up ─────────────────────────────");
    if bind == "0.0.0.0" {
        println!("[node]   bind : 0.0.0.0 (all interfaces — reachable on the LAN)");
        println!("[node]   LAN  : http://{lan}:{port}   <- give this IP:port to the server");
    } else {
        println!("[node]   bind : 127.0.0.1 (local only — not reachable from the LAN)");
        println!("[node]   local: http://127.0.0.1:{port}");
    }
    println!("[node]   server: {server}");
    println!("[node]   accel: {accel_desc}");
    println!("[node] ─────────────────────────────────────────────────────");
    println!();
    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app).await.unwrap();
}
