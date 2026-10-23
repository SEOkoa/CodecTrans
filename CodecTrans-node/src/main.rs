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
    // the node's own folder (CodecTrans-node/) — keep in/out local to the node
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
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
    let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
    let good = ok && std::fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    good
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
                    tasks.push(ProbeTask {
                        enc: e.clone(),
                        tag: format!("nvenc{}", d.index),
                        vf: "format=nv12".into(),
                        extra_args: vec![],
                        env: vec![("CUDA_VISIBLE_DEVICES".into(), d.index.to_string())],
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
    let all = build_probe_tasks(&candidates, &enumerate_gpus());
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

// ============================================================
//  run a job locally
// ============================================================

async fn run_job(client: &reqwest::Client, state: &NState, server: &str, node_id: &str, job: RemoteJob) {
    let in_path = in_dir().join(&job.input_file);
    let out_path = out_dir().join(&job.output_file);

    // 1. download input from server
    let url = format!("{}/api/nodes/{}/file/{}", server, node_id, job.input_file);
    let dl = client.get(&url).send().await;
    let dl = match dl {
        Ok(r) if r.status().is_success() => r,
        _ => {
            let msg: String = "failed to download input from server".into();
            let _ = client
                .post(format!("{}/api/nodes/{}/report", server, node_id))
                .json(&json!({"jobId": job.id, "status": "error", "error": msg}))
                .send().await;
            let mut st = state.lock().unwrap();
            push_job(&mut st, LocalJob {
                id: job.id.clone(), input_file: job.input_file.clone(), output_file: job.output_file.clone(),
                video_codec: job.effective_video_codec.clone(), status: "error".into(), progress: 0.0,
                error: Some(msg.clone()), finished_at: Some(now_ms()),
            });
            return;
        }
    };
    if let Ok(bytes) = dl.bytes().await {
        let _ = std::fs::write(&in_path, &bytes);
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
        if let Some(dev) = pick_vaapi_device() {
            args.push("-vaapi_device".into());
            args.push(dev);
        }
    }
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

    let mut child = match Command::new("ffmpeg").args(&args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() {
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

    // 4. upload result + report
    if ok {
        if let Ok(bytes) = std::fs::read(&out_path) {
            let size = bytes.len() as u64;
            let form = reqwest::multipart::Form::new()
                .part("jobId", reqwest::multipart::Part::text(job.id.clone()))
                .part("outputFile", reqwest::multipart::Part::text(job.output_file.clone()))
                .part("file", reqwest::multipart::Part::bytes(bytes).file_name(job.output_file.clone()));
            let up = client.post(format!("{}/api/nodes/{}/result", server, node_id)).multipart(form).send().await;
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
        _ => false,
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
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

async fn heartbeat_loop(mut client: reqwest::Client, server: String, name: String, caps: Caps, id: SharedId) {
    loop {
        let nid = id.lock().unwrap().clone();
        let resp = client.post(format!("{}/api/nodes/{}/heartbeat", server, nid)).send().await;
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

/// Long-poll loop: blocks on /poll (server holds up to 20s), runs any job, repeats.
/// Re-registers automatically if the server no longer knows this node.
async fn poll_loop(mut client: reqwest::Client, server: String, name: String, caps: Caps, id: SharedId, state: NState) {
    loop {
        let nid = id.lock().unwrap().clone();
        let url = format!("{}/api/nodes/{}/poll", server, nid);
        let resp = client.get(&url).send().await;
        match resp {
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                // unknown node (server restart) → re-register, then retry immediately
                println!("[node] not registered on server — re-registering");
                let _ = do_register(&client, &server, &name, &caps, &id).await;
                continue;
            }
            Ok(r) => {
                if let Ok(v) = r.json::<Value>().await {
                    if let Some(jobv) = v.get("job").filter(|j| !j.is_null()) {
                        if let Ok(job) = serde_json::from_value::<RemoteJob>(jobv.clone()) {
                            println!("[node] claimed job {} ({} -> {})", job.id, job.input_file, job.effective_video_codec);
                            let nid2 = id.lock().unwrap().clone();
                            run_job(&client, &state, &server, &nid2, job).await;
                            continue; // immediately poll for the next
                        }
                    }
                }
            }
            Err(e) => {
                println!("[node] poll error: {e} (retrying)");
                // Swap in a fresh client (new pool) — a stale keep-alive /
                // long-poll connection from a restarted server would
                // otherwise be reused by every retry and keep timing out.
                client = fresh_client();
            }
        }
        // poll returned empty or errored; small pause to avoid hammering
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
    let accel_desc: String = if caps.hw_encoders.is_empty() {
        "CPU only".to_string()
    } else {
        caps.hw_encoders.join(", ")
    };

    let client = reqwest::Client::builder()
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
