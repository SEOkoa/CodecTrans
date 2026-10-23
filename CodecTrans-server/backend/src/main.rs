//! CodecTrans — video transcoding service (Rust/axum + ffmpeg).
//!
//! MVP (increment 1):
//!   - upload an input media file
//!   - start a transcode job (target video/audio codec + container)
//!   - poll job progress (parsed from ffmpeg stderr)
//!   - list jobs, download the produced file
//!   - serve the React frontend from /
//!
//! Layout (relative to CWD):
//!   uploads/  input files
//!   outputs/  transcoded files
//!   frontend/dist  built React app (served at /)

#![allow(non_snake_case, unused)]

use axum::{
    body::Bytes,
    extract::{
        multipart::Multipart,
        path::Path as UrlPath,
        Query,
        DefaultBodyLimit,
        State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json,
    Router,
};
use futures_util::StreamExt;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, ResolvedService, IfKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tower_http::services::ServeDir;
use tower_http::cors::CorsLayer;

// ============================================================
//  Paths / config
// ============================================================

fn base_dir() -> PathBuf {
    std::env::var("CT_BASE").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap_or(PathBuf::from(".").as_path()).to_path_buf()
    })
}

fn uploads_dir() -> PathBuf {
    let d = base_dir().join("uploads");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn outputs_dir() -> PathBuf {
    let d = base_dir().join("outputs");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn frontend_dir() -> PathBuf {
    base_dir().join("frontend").join("dist")
}

/// Max upload size in bytes. `Multipart` defaults to 2MB, which silently breaks
/// real video uploads ("Failed to fetch" in the browser). Override with
/// CT_MAX_UPLOAD_MB (default 8192 MB = 8GB).
fn max_upload_bytes() -> usize {
    let mb: usize = std::env::var("CT_MAX_UPLOAD_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192);
    mb.saturating_mul(1024 * 1024)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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

// ============================================================
//  Job model + store
// ============================================================

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub input_file: String,
    pub output_file: String,
    pub video_codec: String,
    /// Encoder the node actually runs — may be an HW-accelerated variant of
    /// `video_codec` chosen at dispatch time (e.g. h264 -> h264_nvenc).
    pub effective_video_codec: String,
    pub audio_codec: String,
    pub container: String,
    pub crf: Option<u32>,
    /// Target video bitrate in kbps (set when the user picks "% of source"
    /// mode). Wins over `crf` when present: `-b:v` + `-maxrate` + `-bufsize`.
    pub video_bitrate: Option<u32>,
    /// The source-video percentage the user requested (100 = same bitrate).
    pub bitrate_pct: Option<u32>,
    /// Source video bitrate in bits/s (from ffprobe) — for UI display.
    pub source_bit_rate: Option<u64>,
    pub status: String,          // queued | dispatched | running | done | error
    pub progress: f64,           // 0.0 .. 1.0
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub input_size: Option<u64>,
    pub output_size: Option<u64>,
    /// Which node this job is (or was) dispatched to. None = run on the server.
    pub node_id: Option<String>,
    /// "server" or the node name that executes/ran the job.
    pub executed_by: String,
    /// true when effective_video_codec was auto-rewritten to an HW encoder.
    pub accelerated: bool,
}

/// One accelerator as reported by a node: a physical device (GPU or iGPU)
/// + backend + the encoders that actually work on it. The UI groups these as
/// "GPU #0", "CPU #0", ...
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Accel {
    pub kind: String,     // "gpu" (discrete) | "cpu" (iGPU)
    pub device: u32,      // 0-based index within its kind
    pub name: String,
    pub backend: String,  // "nvenc" | "qsv" | "vaapi"
    pub encoders: Vec<String>,
}

/// A registered transcoding node and its ffmpeg acceleration capabilities.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    pub name: String,
    pub ffmpeg_version: String,
    /// HW acceleration methods from `ffmpeg -haccels` (cuda, vaapi, qsv, ...).
    pub haccels: Vec<String>,
    /// Encoders that are HW-accelerated (h264_nvenc, hevc_vaapi, ...).
    pub hw_encoders: Vec<String>,
    /// Grouped per physical device (GPU #0 nvenc, CPU #0 qsv/vaapi, ...).
    pub accels: Vec<Accel>,
    pub status: String,          // online | offline
    pub last_heartbeat: i64,
    pub current_job: Option<String>,
}

/// A node seen via mDNS LAN discovery (may not have registered yet).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovered {
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub hw_encoders: Vec<String>,
    pub ffmpeg_version: String,
    pub last_seen: i64,
}

/// One video in the storage library (scanned by the server, read-only).
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub path: String,          // relative to storage root
    pub name: String,          // base filename
    pub size: u64,
    pub duration: Option<f64>, // seconds
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub container: Option<String>,
    // source analysis (task: show what's in each file before re-encoding)
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_bit_rate: Option<u64>, // bits/s
    pub pix_fmt: Option<String>,
    pub status: String,        // pending | queued | done | skipped | error
    pub reencoded: Option<String>, // target codec actually applied (e.g. "hevc")
    pub new_size: Option<u64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct JobStore {
    jobs: HashMap<String, Job>,
    nodes: HashMap<String, Node>,
    discovered: HashMap<String, Discovered>,
    library: HashMap<String, LibraryEntry>, // keyed by relative path
    storage_root: String,
}

impl JobStore {
    pub fn insert(&mut self, j: Job) {
        self.jobs.insert(j.id.clone(), j);
    }
    pub fn update<F: FnOnce(&mut Job)>(&mut self, id: &str, f: F) -> Option<Job> {
        if let Some(j) = self.jobs.get_mut(id) {
            f(j);
            self.jobs.get(id).cloned()
        } else {
            None
        }
    }
    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.get(id).cloned()
    }
    pub fn list(&self) -> Vec<Job> {
        let mut v: Vec<Job> = self.jobs.values().cloned().collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }

    // ---- nodes ----
    fn online(n: &Node) -> bool {
        now_ms() - n.last_heartbeat <= 10_000
    }
    pub fn upsert_node(&mut self, n: Node) {
        self.nodes.insert(n.id.clone(), n);
    }
    pub fn get_node(&self, id: &str) -> Option<Node> {
        self.nodes.get(id).cloned()
    }
    pub fn find_node_by_name(&self, name: &str) -> Option<Node> {
        self.nodes.values().find(|n| n.name == name).cloned()
    }
    pub fn list_nodes(&mut self) -> Vec<Node> {
        let mut v: Vec<Node> = self.nodes.values().cloned().collect();
        for n in v.iter_mut() {
            if !Self::online(n) {
                n.status = "offline".into();
            }
        }
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
    pub fn heartbeat(&mut self, id: &str) -> Option<Node> {
        if let Some(n) = self.nodes.get_mut(id) {
            n.last_heartbeat = now_ms();
            n.status = "online".into();
        }
        self.nodes.get(id).cloned()
    }
    /// Atomically claim the oldest queued job destined for `node_id`.
    pub fn claim_for_node(&mut self, node_id: &str) -> Option<Job> {
        let best: Option<Job> = self
            .jobs
            .values()
            .filter(|j| j.status == "queued" && j.node_id.as_deref() == Some(node_id))
            .cloned()
            .min_by_key(|j| j.created_at);
        if let Some(mut j) = best {
            j.status = "dispatched".into();
            self.jobs.insert(j.id.clone(), j.clone());
            if let Some(n) = self.nodes.get_mut(node_id) {
                n.current_job = Some(j.id.clone());
            }
            Some(j)
        } else {
            None
        }
    }
    /// Auto-pick a node: prefer an online node that has an HW encoder for the
    /// requested codec family, else any online node.
    pub fn pick_node(&self, family: &str) -> Option<Node> {
        let online: Vec<Node> = self
            .nodes
            .values()
            .filter(|n| Self::online(n))
            .cloned()
            .collect();
        for n in &online {
            if n.hw_encoders.iter().any(|h| codec_family(h) == family) {
                return Some(n.clone());
            }
        }
        online.first().cloned()
    }

    // ---- discovered (mDNS LAN discovery) ----
    /// Record / refresh a node seen on the LAN. Keyed by name.
    pub fn upsert_discovered(&mut self, d: Discovered) {
        self.discovered.insert(d.name.clone(), d);
    }
    /// Drop discovered entries not seen within the last `ttl_ms`.
    pub fn prune_discovered(&mut self, ttl_ms: i64) {
        let cutoff = now_ms() - ttl_ms;
        self.discovered
            .retain(|_, d| d.last_seen >= cutoff);
    }
    pub fn list_discovered(&self) -> Vec<Discovered> {
        let mut v: Vec<Discovered> = self.discovered.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    // ---- library (storage catalog, server-side, read-only scan) ----
    pub fn storage_root(&self) -> &str {
        &self.storage_root
    }
    pub fn set_storage_root(&mut self, root: String) {
        self.storage_root = root;
    }
    pub fn list_library(&self) -> Vec<LibraryEntry> {
        let mut v: Vec<LibraryEntry> = self.library.values().cloned().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }
    /// Replace the whole catalog (after a scan).
    pub fn set_library(&mut self, entries: Vec<LibraryEntry>) {
        self.library.clear();
        for e in entries {
            self.library.insert(e.path.clone(), e);
        }
    }
    pub fn get_library_entry(&self, path: &str) -> Option<LibraryEntry> {
        self.library.get(path).cloned()
    }
    pub fn update_library_entry<F: FnOnce(&mut LibraryEntry)>(
        &mut self,
        path: &str,
        f: F,
    ) -> Option<LibraryEntry> {
        if let Some(e) = self.library.get_mut(path) {
            f(e);
            self.library.get(path).cloned()
        } else {
            None
        }
    }
}

type Store = Arc<Mutex<JobStore>>;

// ============================================================
//  ffmpeg helpers
// ============================================================

/// Duration of a media file in seconds (ffprobe), or 0.0 on failure.
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

/// ffprobe a file -> (video_codec, audio_codec, duration_seconds).
async fn probe_media(path: &str) -> (Option<String>, Option<String>, f64) {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_type,codec_name",
            "-show_entries", "format=duration",
            "-of", "json",
            path,
        ])
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());
    let Some(s) = out else { return (None, None, 0.0) };
    let Ok(v) = serde_json::from_str::<Value>(&s) else { return (None, None, 0.0) };
    let mut vc: Option<String> = None;
    let mut ac: Option<String> = None;
    if let Some(streams) = v.get("streams").and_then(|s| s.as_array()) {
        for st in streams {
            let ct = st.get("codec_type").and_then(|x| x.as_str()).unwrap_or("");
            let cn = st.get("codec_name").and_then(|x| x.as_str()).map(|s| s.to_string());
            match ct {
                "video" if vc.is_none() => vc = cn,
                "audio" if ac.is_none() => ac = cn,
                _ => {}
            }
        }
    }
    let dur = v
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    (vc, ac, dur)
}

/// Full media analysis (ffprobe): what the user asked for in the UI —
/// bitrate, codecs, resolution, fps, duration, audio layout.
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub duration: Option<f64>,         // seconds
    pub size: Option<u64>,             // bytes
    pub container: Option<String>,     // mp4, mov, avi, ...
    pub bit_rate: Option<u64>,         // overall, bits/s
    pub video_codec: Option<String>,   // h264, hevc, av1, ...
    pub video_profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pix_fmt: Option<String>,       // yuv420p, p010le, ...
    pub fps: Option<f64>,
    pub video_bit_rate: Option<u64>,   // bits/s
    pub audio_codec: Option<String>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<u32>,
    pub audio_bit_rate: Option<u64>,
}

/// ffprobe a file into full MediaInfo (best-effort: fields are None on failure).
async fn probe_full(path: &str) -> MediaInfo {
    let mut info = MediaInfo::default();
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries",
            "stream=codec_type,codec_name,profile,width,height,pix_fmt,avg_frame_rate,sample_rate,channels,bit_rate",
            "-show_entries", "format=duration,size,bit_rate,format_name",
            "-of", "json",
            path,
        ])
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());
    let Some(s) = out else { return info };
    let Ok(v) = serde_json::from_str::<Value>(&s) else { return info };

    let u32f = |st: &Value, k: &str| st.get(k).and_then(|x| x.as_u64()).map(|n| n as u32);
    let f64f = |st: &Value, k: &str| st.get(k).and_then(|x| x.as_f64());
    if let Some(streams) = v.get("streams").and_then(|s| s.as_array()) {
        for st in streams {
            let ct = st.get("codec_type").and_then(|x| x.as_str()).unwrap_or("");
            let cn = st.get("codec_name").and_then(|x| x.as_str()).map(|s| s.to_string());
            let br = st.get("bit_rate").and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok());
            match ct {
                "video" if info.video_codec.is_none() => {
                    info.video_codec = cn;
                    info.video_profile = st.get("profile").and_then(|x| x.as_str()).map(|s| s.to_string());
                    info.width = u32f(st, "width");
                    info.height = u32f(st, "height");
                    info.pix_fmt = st.get("pix_fmt").and_then(|x| x.as_str()).map(|s| s.to_string());
                    info.video_bit_rate = br;
                    // avg_frame_rate is "num/den"
                    if let Some(rate) = st.get("avg_frame_rate").and_then(|x| x.as_str()) {
                        if let Some((n, d)) = rate.split_once('/') {
                            if let (Ok(n), Ok(d)) = (n.parse::<f64>(), d.parse::<f64>()) {
                                if d != 0.0 {
                                    info.fps = Some(n / d);
                                }
                            }
                        }
                    }
                }
                "audio" if info.audio_codec.is_none() => {
                    info.audio_codec = cn;
                    info.audio_sample_rate = u32f(st, "sample_rate");
                    info.audio_channels = u32f(st, "channels");
                    info.audio_bit_rate = br;
                }
                _ => {}
            }
            let _ = f64f; // (kept for future numeric stream fields)
        }
    }
    if let Some(f) = v.get("format") {
        info.duration = f.get("duration").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
        info.size = f.get("size").and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok());
        info.bit_rate = f.get("bit_rate").and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok());
        info.container = f.get("format_name").and_then(|x| x.as_str()).map(|s| s.to_string());
    }
    info
}

/// ffprobe the first video stream's pix_fmt (None on failure / no video).
async fn probe_video_pix_fmt(path: &str) -> Option<String> {
    let out = Command::new("ffprobe")
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
        .and_then(|o| String::from_utf8(o.stdout).ok())?;
    out.trim().lines().next().filter(|s| !s.is_empty() && *s != "N/A").map(|s| s.to_string())
}

/// First /dev/dri/renderD* node, lowest number first (VA-API device pinning).
fn first_render_node() -> Option<String> {
    let dir = std::fs::read_dir("/dev/dri").ok()?;
    let mut nums: Vec<(u32, String)> = Vec::new();
    for e in dir.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if let Some(s) = n.strip_prefix("renderD") {
            if let Ok(i) = s.parse::<u32>() {
                nums.push((i, format!("/dev/dri/{}", n)));
            }
        }
    }
    nums.sort_by_key(|(i, _)| *i);
    nums.into_iter().next().map(|(_, p)| p)
}

/// av1_nvenc on Blackwell (driver 595.x) only accepts 4:2:0 input: the driver
/// reports NV_ENC_CAPS_SUPPORT_YUV444_ENCODE=0 for AV1. If the source is
/// 4:4:4/4:2:2/RGB, ffmpeg negotiates YUV444P and the encoder is rejected with
/// "YUV444P not supported" -> "No capable devices found". Return a format
/// filter that forces NV12 when needed (empty for normal 4:2:0 sources).
async fn nvenc_av1_fmt_guard(encoder: &str, path: &str) -> Vec<String> {
    if encoder != "av1_nvenc" {
        return Vec::new();
    }
    let pf = probe_video_pix_fmt(path).await.unwrap_or_default();
    let ok_420 = matches!(pf.as_str(), "yuv420p" | "nv12" | "p010le" | "yuv420p10le" | "");
    if ok_420 {
        Vec::new()
    } else {
        vec!["-vf".into(), "format=nv12".into()]
    }
}

/// Is this file extension a video container we should scan?
fn is_video_ext(ext: Option<&str>) -> bool {
    match ext {
        Some(e) => [
            "mp4", "mkv", "avi", "mov", "ts", "webm", "m4v", "mpg", "mpeg", "wmv", "flv",
            "3gp", "mts", "m2ts", "ogv", "divx",
        ]
        .contains(&e.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Recursively collect video files under `cur` (relative to `root`), up to `limit`.
fn walk_videos(cur: &std::path::Path, root: &std::path::Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(rd) = std::fs::read_dir(cur) else { return };
    for e in rd.flatten() {
        if out.len() >= limit {
            break;
        }
        let p = e.path();
        if p.is_dir() {
            walk_videos(&p, root, out, limit);
        } else if p.is_file() && is_video_ext(p.extension().and_then(|x| x.to_str())) {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

/// Parse "time=HH:MM:SS.xx" from an ffmpeg stderr line -> seconds.
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

/// Validate a requested video encoder exists in this ffmpeg build.
fn encoder_exists(name: &str) -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        })
        .unwrap_or(false)
}

// ============================================================
//  Static codec / container catalog
// ============================================================

fn codec_catalog() -> Value {
    json!({
        "videoCodecs": [
            {"id": "libx264",     "label": "H.264 (AVC)",     "note": "best compatibility"},
            {"id": "libx265",     "label": "H.265 (HEVC)",    "note": "smaller files, slower"},
            {"id": "libvpx-vp9",  "label": "VP9",             "note": "web-friendly"},
            {"id": "libvpx",      "label": "VP8",             "note": "webm legacy"},
            {"id": "libsvtav1",   "label": "AV1 (SVT)",       "note": "next-gen, fast"},
            {"id": "libaom-av1",  "label": "AV1 (AOM)",       "note": "next-gen, best quality, slow"},
            {"id": "mpeg4",       "label": "MPEG-4 Part 2",   "note": "legacy"}
        ],
        "audioCodecs": [
            {"id": "aac",         "label": "AAC",             "note": "default"},
            {"id": "libmp3lame",  "label": "MP3",             "note": ""},
            {"id": "libopus",     "label": "Opus",            "note": "webm/opus"},
            {"id": "copy",        "label": "Copy (no re-encode)", "note": "stream copy"}
        ],
        "containers": [
            {"id": "mp4",  "label": "MP4"},
            {"id": "mkv",  "label": "MKV"},
            {"id": "webm", "label": "WebM"},
            {"id": "mov",  "label": "QuickTime MOV"},
            {"id": "avi",  "label": "AVI"}
        ]
    })
}

// ---- codec family + HW-acceleration helpers (for node auto-selection) ----

/// Target video bitrate (kbps) = source video bitrate x pct/100.
/// None when the source bitrate is unknown or pct is 0/absent.
/// Floored at 100 kbps so a tiny percentage can't produce an unplayable file.
fn target_kbps(src_bps: Option<u64>, pct: Option<u32>) -> Option<u32> {
    let (bps, p) = (src_bps?, pct?);
    if p == 0 {
        return None;
    }
    // bps -> kbps (the ffmpeg args are built as "{kbps}k").
    let t = bps as f64 * (p as f64) / 100.0 / 1000.0;
    Some((t as u32).max(100))
}

/// Map any encoder name to a logical codec family so HW variants can be matched
/// to the codec the user asked for (libx264 and h264_nvenc are both "h264").
fn codec_family(enc: &str) -> String {
    let e = enc.to_lowercase();
    if e.contains("h264") || e.contains("x264") || e.contains("avc") {
        "h264".into()
    } else if e.contains("hevc") || e.contains("h265") || e.contains("x265") {
        "h265".into()
    } else if e.contains("vp9") {
        "vp9".into()
    } else if e.contains("vp8") {
        "vp8".into()
    } else if e.contains("av1") {
        "av1".into()
    } else if e.contains("mpeg4") {
        "mpeg4".into()
    } else {
        "other".into()
    }
}

/// True if the encoder name is a known HW-accelerated variant.
#[allow(dead_code)]
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

/// Rank a HW encoder by backend preference (lower = faster on typical media):
/// nvenc > vaapi > qsv > amf > mft > videotoolbox. Suffix-based so names like
/// `hevc_vt` / `hevc_videotoolbox` map to the videotoolbox rank.
fn backend_rank(enc: &str) -> usize {
    let e = enc.to_lowercase();
    let pref = ["nvenc", "vaapi", "qsv", "amf", "mft", "vt"];
    if e.contains("videotoolbox") {
        return 5;
    }
    pref.iter()
        .position(|p| e.ends_with(&format!("_{p}")))
        .unwrap_or(99)
}

/// Pick the best accelerated encoder for a family from a node's HW encoders.
fn best_hw_encoder(hw: &[String], family: &str) -> Option<String> {
    let mut cands: Vec<&String> = hw.iter().filter(|h| codec_family(h) == family).collect();
    cands.sort_by(|a, b| {
        backend_rank(a)
            .cmp(&backend_rank(b))
            .then_with(|| a.cmp(b))
    });
    cands.first().map(|s| s.to_string())
}

/// Canonical software encoder for a logical family (used when a node has no
/// working HW encoder for that family).
fn family_default_encoder(family: &str) -> Option<&'static str> {
    match family {
        "h264" => Some("libx264"),
        "h265" => Some("libx265"),
        "vp9" => Some("libvpx-vp9"),
        "vp8" => Some("libvpx"),
        "av1" => Some("libsvtav1"),
        "mpeg4" => Some("mpeg4"),
        _ => None,
    }
}

/// True for short logical codec names that need mapping to a concrete encoder.
fn is_logical_name(enc: &str) -> bool {
    matches!(
        enc.to_lowercase().as_str(),
        "h264" | "h265" | "hevc" | "avc" | "vp8" | "vp9" | "av1" | "mpeg4" | "mpeg2" | "h263"
    )
}

/// Choose the encoder a node should run: the best working HW encoder for the
/// requested family, else the requested name, else (for logical names) the
/// family's canonical software encoder. Returns (encoder, accelerated).
fn resolve_video_codec(requested: &str, hw: &[String]) -> (String, bool) {
    let family = codec_family(requested);
    if let Some(h) = best_hw_encoder(hw, &family) {
        return (h, true);
    }
    if is_logical_name(requested) {
        if let Some(def) = family_default_encoder(&family) {
            return (def.to_string(), false);
        }
    }
    (requested.to_string(), false)
}

// ============================================================
//  Transcode worker
// ============================================================

/// Encoders in our catalog that accept `-crf`. HW NVENC encoders use
/// `-qp`/`-cq` instead — sending them `-crf` fails, so we skip it.
const CRF_ENCODERS: [&str; 6] = [
    "libx264",
    "libx265",
    "libvpx",
    "libvpx-vp9",
    "libsvtav1",
    "libaom-av1",
];

async fn run_transcode(store: Store, job: Job) {
    // mark running
    {
        let mut s = store.lock().unwrap();
        let _ = s.update(&job.id, |j| {
            j.status = "running".into();
            j.started_at = Some(now_ms());
        });
    }

    let input_path = uploads_dir().join(&job.input_file);
    let output_path = outputs_dir().join(&job.output_file);

    // build ffmpeg args
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-i".into(),
        input_path.to_string_lossy().to_string(),
    ];
    // av1_nvenc guard: force NV12 for 4:4:4/4:2:2/RGB sources (see fn docs).
    let guard = nvenc_av1_fmt_guard(&job.video_codec, &input_path.to_string_lossy()).await;
    args.extend(guard);
    // VA-API encoders need an explicit hwupload: ffmpeg's auto_scale can't
    // convert sw frames to the VA-API hw format on this build (ENOSYS), and
    // hwupload needs a device reference, so pin -vaapi_device to the first
    // /dev/dri render node.
    if job.video_codec.ends_with("_vaapi") {
        args.push("-vf".into());
        args.push("format=nv12,hwupload".into());
        if let Some(dev) = first_render_node() {
            args.push("-vaapi_device".into());
            args.push(dev);
        }
    }
    // video codec
    args.push("-c:v".into());
    args.push(job.video_codec.clone());
    if job.video_codec != "copy" {
        if let Some(kbps) = job.video_bitrate {
            // Capped VBR ("% of source" mode): -b:v sets the target and
            // -maxrate/-bufsize hard-cap the output. That cap is what keeps a
            // misconfigured percentage from bloating the file past its target
            // (the failure mode of bare CRF on hard-to-compress content).
            args.push("-b:v".into());
            args.push(format!("{kbps}k"));
            args.push("-maxrate".into());
            args.push(format!("{}k", (kbps as u64) * 120 / 100)); // 1.2x target
            args.push("-bufsize".into());
            args.push(format!("{}k", (kbps as u64) * 2)); // 2x target
        } else if let Some(crf) = job.crf {
            if CRF_ENCODERS.contains(&job.video_codec.as_str()) {
                args.push("-crf".into());
                args.push(crf.to_string());
            }
        }
        // Per-encoder preset — each family accepts different names:
        // libsvtav1 takes ints (0..=13), NVENC takes p1..p7, the rest accept
        // "veryfast". (Passing "veryfast" to nvenc/svtav1 fails at open.)
        let preset = if job.video_codec == "libsvtav1" {
            "4"
        } else if job.video_codec.ends_with("_nvenc") {
            "p5"
        } else {
            "veryfast"
        };
        args.push("-preset".into());
        args.push(preset.into());
    }
    // audio codec — the built-in `opus` is experimental in this ffmpeg build;
    // libopus is the stable encoder.
    let acodec = if job.audio_codec == "opus" { "libopus" } else { job.audio_codec.as_str() };
    args.push("-c:a".into());
    args.push(acodec.to_string());
    if job.audio_codec == "aac" {
        args.push("-b:a".into());
        args.push("128k".into());
    }
    // container is inferred from output extension; force with -f for webm/mov
    if job.container == "webm" {
        args.push("-f".into());
        args.push("webm".into());
    }
    args.push("-y".into());
    args.push(output_path.to_string_lossy().to_string());

    // total duration for progress
    let total = probe_duration(&input_path.to_string_lossy()).await;

    let mut child = match Command::new("ffmpeg").args(&args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = store.lock().unwrap().update(&job.id, |j| { j.status = "error".into(); j.error = Some(format!("spawn ffmpeg failed: {e}")); j.finished_at = Some(now_ms()); });
            return;
        }
    };

    // read stderr, update progress.
    // ffmpeg writes progress updates with '\r' (carriage return, same-line
    // overwrite), NOT '\n'. Line-based reading (BufReader::lines) lumps every
    // update into ONE line delivered only when ffmpeg finally emits '\n' at the
    // end -> progress stuck at 0 until done. Read in chunks and split on BOTH
    // '\r' and '\n' so each update is processed live.
    use tokio::io::AsyncReadExt;
    let mut stderr = match child.stderr.take() {
        Some(s) => s,
        None => {
            let _ = child.wait().await;
            return;
        }
    };
    let mut buf = [0u8; 8192];
    let mut pending = String::new();
    let mut last_err_line: Option<String> = None;
    loop {
        let n = match stderr.read(&mut buf).await {
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
                let _ = store
                    .lock()
                    .unwrap()
                    .update(&job.id, |j| {
                        j.progress = prog;
                    });
            }
            if seg.contains("Error")
                || seg.contains("Invalid")
                || seg.contains("does not contain any stream")
            {
                last_err_line = Some(seg.trim().to_string());
            }
        }
    }
    // flush any trailing segment without a terminator
    if !pending.is_empty() {
        if let Some(t) = parse_time(&pending) {
            let prog = if total > 0.0 {
                (t / total).clamp(0.0, 0.999)
            } else {
                0.0
            };
            let _ = store
                .lock()
                .unwrap()
                .update(&job.id, |j| {
                    j.progress = prog;
                });
        }
        if pending.contains("Error")
            || pending.contains("Invalid")
            || pending.contains("does not contain any stream")
        {
            last_err_line = Some(pending.trim().to_string());
        }
    }

    let status = child.wait().await.map(|s| s.success()).unwrap_or(false);

    // finalise
    let out_size = std::fs::metadata(&output_path).map(|m| m.len()).ok();
    let in_size = std::fs::metadata(&input_path).map(|m| m.len()).ok();
    {
        let mut s = store.lock().unwrap();
        let _ = s.update(&job.id, |j| {
            j.finished_at = Some(now_ms());
            j.input_size = in_size;
            j.output_size = out_size;
            if status {
                j.status = "done".into();
                j.progress = 1.0;
            } else {
                j.status = "error".into();
                j.error = last_err_line.or_else(|| Some("ffmpeg exited with non-zero status".into()));
            }
        });
    }
}

// ============================================================
//  Handlers
// ============================================================

pub async fn health() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "codectrans",
        "ffmpeg": std::process::Command::new("ffmpeg").arg("-version").output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.lines().next().map(|l| l.to_string()))
            .unwrap_or_else(|| "ffmpeg not found".into()),
        "time": now_ms()
    }))
}

pub async fn codecs() -> impl IntoResponse {
    // filter video codecs to those actually present in this ffmpeg build
    let catalog = codec_catalog();
    let mut vc = vec![];
    if let Some(arr) = catalog.get("videoCodecs").and_then(|v| v.as_array()) {
        for c in arr {
            let id = c.get("id").and_then(|x| x.as_str()).unwrap_or("");
            if encoder_exists(id) {
                vc.push(c.clone());
            }
        }
    }
    let mut out = catalog.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("videoCodecs".into(), Value::Array(vc));
    }
    Json(out)
}

/// POST /api/upload  (multipart field "file") -> {file, size}
/// Streams the field to disk in chunks — never buffers the whole file in RAM,
/// so multi-GB uploads don't OOM the server.
pub async fn upload(mut mp: Multipart) -> Response {
    let mut saved: Option<(String, u64)> = None;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.file_name().unwrap_or("upload.bin").to_string();
        // sanitise: strip any path, keep base name
        let safe = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_string();
        let path = uploads_dir().join(&safe);
        let mut file = match tokio::fs::File::create(&path).await {
            Ok(f) => f,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("create: {e}")}))).into_response(),
        };
        let mut size: u64 = 0;
        let mut ok = true;
        // `Field` is a Stream<Result<Bytes>> — pump it to disk chunk by chunk.
        let mut stream = field;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    size += bytes.len() as u64;
                    if let Err(e) = file.write_all(&bytes).await {
                        eprintln!("[upload] write {safe}: {e}");
                        ok = false;
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("[upload] read field {safe}: {e}");
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            let _ = std::fs::remove_file(&path);
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "failed to store upload"}))).into_response();
        }
        saved = Some((safe, size));
    }
    match saved {
        Some((f, size)) => {
            // Analyze the saved file so the UI can show codec/bitrate/etc.
            // immediately — no separate round-trip needed.
            let analysis = probe_full(&uploads_dir().join(&f).to_string_lossy()).await;
            Json(json!({"file": f, "size": size, "analysis": analysis})).into_response()
        }
        None => (StatusCode::BAD_REQUEST, Json(json!({"error": "no file field"}))).into_response(),
    }
}

/// GET /api/media/analyze?file=<name> — full ffprobe analysis of an uploaded
/// file (bitrate, codecs, resolution, fps, audio layout). Filename is
/// base-name only; anything with a path is rejected.
pub async fn media_analyze(Query(query): Query<HashMap<String, String>>) -> Response {
    let Some(name) = query.get("file").cloned() else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "missing ?file="}))).into_response();
    };
    let safe = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_string();
    if safe.contains("..") || safe.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad file name"}))).into_response();
    }
    let path = uploads_dir().join(&safe);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "file not found"}))).into_response();
    }
    let info = probe_full(&path.to_string_lossy()).await;
    Json(json!({"file": safe, "analysis": info})).into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanReq {
    /// Storage root to scan. Optional — falls back to the previously set root,
    /// then the CT_STORAGE env var.
    root: Option<String>,
    /// Max files to scan (default 500). Keeps large libraries responsive.
    limit: Option<usize>,
}

/// GET /api/library -> {root, count, entries}
pub async fn library_get(State(store): State<Store>) -> Response {
    let s = store.lock().unwrap();
    let root = s.storage_root().to_string();
    let entries = s.list_library();
    Json(json!({"root": root, "count": entries.len(), "entries": entries})).into_response()
}

/// POST /api/library/scan — walk the storage root, ffprobe each video, build
/// the in-memory catalog. Read-only: never touches the source files.
pub async fn library_scan(State(store): State<Store>, Json(req): Json<ScanReq>) -> Response {
    // Resolve the root: request > existing > CT_STORAGE.
    let root = match req.root.filter(|r| !r.trim().is_empty()) {
        Some(r) => r,
        None => {
            let existing = store.lock().unwrap().storage_root().to_string();
            if !existing.trim().is_empty() {
                existing
            } else {
                std::env::var("CT_STORAGE").unwrap_or_default()
            }
        }
    };
    if root.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no storage root — pass {\"root\":\"/path/to/videos\"} or set CT_STORAGE"})),
        )
            .into_response();
    }
    let rootp = PathBuf::from(root.trim());
    if !rootp.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("not a directory: {}", rootp.display())})),
        )
            .into_response();
    }
    let limit = req.limit.unwrap_or(500).max(1).min(5000);

    // Walk (blocking, but cheap) — collect relative video paths.
    let rels = std::thread::scope(|sc| {
        sc.spawn(|| {
            let mut v: Vec<PathBuf> = Vec::new();
            walk_videos(&rootp, &rootp, &mut v, limit);
            v
        })
        .join()
        .unwrap_or_default()
    });

    // ffprobe each (sequential; bounded by `limit`). MVP — parallelize later.
    let mut entries: Vec<LibraryEntry> = Vec::with_capacity(rels.len());
    for rel in &rels {
        let full = rootp.join(rel);
        let pstr = full.to_string_lossy().to_string();
        let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
        let ext = full.extension().and_then(|x| x.to_str()).unwrap_or("").to_string();
        let info = probe_full(&pstr).await;
        entries.push(LibraryEntry {
            path: rel.to_string_lossy().to_string(),
            name: full.file_name().and_then(|x| x.to_str()).unwrap_or("").to_string(),
            size,
            duration: info.duration,
            video_codec: info.video_codec,
            audio_codec: info.audio_codec,
            container: info.container.clone().or_else(|| if ext.is_empty() { None } else { Some(ext) }),
            width: info.width,
            height: info.height,
            fps: info.fps,
            video_bit_rate: info.video_bit_rate,
            pix_fmt: info.pix_fmt,
            status: "pending".into(),
            reencoded: None,
            new_size: None,
            error: None,
        });
    }
    {
        let mut s = store.lock().unwrap();
        s.set_storage_root(rootp.to_string_lossy().to_string());
        s.set_library(entries.clone());
    }
    Json(json!({
        "root": rootp.to_string_lossy(),
        "count": entries.len(),
        "entries": entries,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscodeReq {
    file: String,
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    crf: Option<u32>,
    /// Percentage of the source video bitrate (1..=500). When set, overrides
    /// `crf` — the job runs capped VBR instead of CRF.
    #[serde(default)]
    bitrate_pct: Option<u32>,
}

fn safe_base(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rfind('.') {
        Some(i) if i > 0 => base[..i].to_string(),
        _ => base.to_string(),
    }
}

/// POST /api/transcode -> {id}
pub async fn transcode(State(store): State<Store>, Json(req): Json<TranscodeReq>) -> Response {
    // input must exist
    if !uploads_dir().join(&req.file).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("input file not found: {}", req.file)}))).into_response();
    }
    // Resolve a logical name (h264/h265/vp9/...) to a concrete encoder. On the
    // server path there's no HW list, so this maps logical -> canonical software
    // encoder (h265 -> libx265); concrete names (libx264, copy) pass through.
    let requested_vcodec = req.video_codec.unwrap_or_else(|| "libx264".into());
    let (vcodec, _hw) = resolve_video_codec(&requested_vcodec, &[]);
    let acodec = req.audio_codec.unwrap_or_else(|| "aac".into());
    let container = req.container.unwrap_or_else(|| "mp4".into());

    let out_name = format!("{}_{}_{}.{}", safe_base(&req.file), vcodec.replace("lib", "").replace('-', "_"), container, container);
    let id = uuid::Uuid::new_v4().to_string();
    let input_size = std::fs::metadata(uploads_dir().join(&req.file)).map(|m| m.len()).ok();
    // "% of source" bitrate mode: analyze the source once, compute the target.
    let analysis = probe_full(&uploads_dir().join(&req.file).to_string_lossy()).await;
    let video_bitrate = target_kbps(analysis.video_bit_rate, req.bitrate_pct);
    let job = Job {
        id: id.clone(),
        input_file: req.file.clone(),
        output_file: out_name.clone(),
        video_codec: vcodec.clone(),
        effective_video_codec: vcodec.clone(),
        audio_codec: acodec.clone(),
        container: container.clone(),
        crf: req.crf,
        video_bitrate,
        bitrate_pct: req.bitrate_pct,
        source_bit_rate: analysis.video_bit_rate,
        status: "queued".into(),
        progress: 0.0,
        created_at: now_ms(),
        started_at: None,
        finished_at: None,
        error: None,
        input_size,
        output_size: None,
        node_id: None,
        executed_by: "server".into(),
        accelerated: false,
    };
    let job_for_worker = job.clone();
    {
        let mut s = store.lock().unwrap();
        s.insert(job);
    }
    let store2 = store.clone();
    tokio::spawn(async move { run_transcode(store2, job_for_worker).await; });
    Json(json!({"id": id})).into_response()
}

pub async fn list_jobs(State(store): State<Store>) -> impl IntoResponse {
    let v: Vec<Value> = store
        .lock()
        .unwrap()
        .list()
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    Json(Value::Array(v))
}

pub async fn get_job(State(store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    match store.lock().unwrap().get(&id) {
        Some(j) => match serde_json::to_value(j) {
            Ok(v) => Json(v).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "job not found"}))).into_response(),
    }
}

pub async fn download(UrlPath(name): UrlPath<String>) -> Response {
    // only serve from outputs/, block path traversal
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad filename"}))).into_response();
    }
    let path = outputs_dir().join(&name);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "file not found"}))).into_response();
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    };
    let headers = [
        ("content-type".to_string(), "application/octet-stream".to_string()),
        ("content-disposition".to_string(), format!("attachment; filename={name}")),
    ];
    (StatusCode::OK, headers, bytes).into_response()
}

// ============================================================
//  Node management (distributed transcoding)
// ============================================================

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterReq {
    name: String,
    #[serde(default)]
    ffmpeg_version: Option<String>,
    #[serde(default)]
    haccels: Option<Vec<String>>,
    #[serde(default)]
    hw_encoders: Option<Vec<String>>,
    #[serde(default)]
    accels: Option<Vec<Accel>>,
}

/// POST /api/nodes/register — a node announces itself with its ffmpeg capabilities.
/// Idempotent by name: if a node with this name already exists, its capabilities
/// and heartbeat are refreshed and the SAME id is returned (safe to re-register,
/// e.g. after a server restart).
pub async fn node_register(State(store): State<Store>, Json(req): Json<RegisterReq>) -> Response {
    let (ffmpeg_version, haccels, hw_encoders, accels) = (
        req.ffmpeg_version.unwrap_or_default(),
        req.haccels.unwrap_or_default(),
        req.hw_encoders.unwrap_or_default(),
        req.accels.unwrap_or_default(),
    );
    // idempotent re-register by name
    {
        let mut s = store.lock().unwrap();
        if let Some(existing) = s.find_node_by_name(&req.name) {
            if let Some(n) = s.nodes.get_mut(&existing.id) {
                n.ffmpeg_version = ffmpeg_version;
                n.haccels = haccels;
                n.hw_encoders = hw_encoders;
                n.accels = accels;
                n.last_heartbeat = now_ms();
                n.status = "online".into();
            }
            return Json(json!({"nodeId": existing.id})).into_response();
        }
    }
    // new node
    let id = uuid::Uuid::new_v4().to_string();
    let node = Node {
        id: id.clone(),
        name: req.name,
        ffmpeg_version,
        haccels,
        hw_encoders,
        accels,
        status: "online".into(),
        last_heartbeat: now_ms(),
        current_job: None,
    };
    store.lock().unwrap().upsert_node(node);
    Json(json!({"nodeId": id})).into_response()
}

/// POST /api/nodes/:id/heartbeat — liveness.
pub async fn node_heartbeat(State(store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    match store.lock().unwrap().heartbeat(&id) {
        Some(n) => match serde_json::to_value(n) {
            Ok(v) => Json(json!({"ok": true, "node": v})).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "unknown node"}))).into_response(),
    }
}

/// GET /api/nodes — all nodes with live status + acceleration capabilities.
pub async fn list_nodes(State(store): State<Store>) -> impl IntoResponse {
    let v: Vec<Value> = store
        .lock()
        .unwrap()
        .list_nodes()
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    Json(Value::Array(v))
}

/// GET /api/nodes/discovered — nodes seen on the LAN via mDNS (may not be
/// registered yet). Each carries its name, ip, port and HW encoders.
pub async fn list_discovered_nodes(State(store): State<Store>) -> impl IntoResponse {
    let v: Vec<Value> = store
        .lock()
        .unwrap()
        .list_discovered()
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    Json(Value::Array(v))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchReq {
    ip: String,
    #[serde(default)]
    port: Option<u16>,
}

/// POST /api/nodes/search — "server finds a node": probe a node's local API
/// (ip:port) and, if it answers, verify its capabilities and register/refresh
/// it in the registry. The user copies the IP:port the node printed in its
/// terminal and hands it to the server. Idempotent by name, so it merges with
/// the node's own registration once the node connects back to us.
pub async fn search_node(State(store): State<Store>, Json(req): Json<SearchReq>) -> Response {
    let port = req.port.unwrap_or(8401);
    let base = format!("http://{}:{}", req.ip, port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    // 1. Probe /api/health for the node's name.
    let health = match client.get(format!("{base}/api/health")).send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => v,
            Err(_) => {
                return (StatusCode::NOT_FOUND, Json(json!({ "error": "node responded but /api/health was unreadable" }))).into_response();
            }
        },
        Ok(r) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no node at {base} (HTTP {})", r.status()) }))).into_response();
        }
        Err(e) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no node at {base} ({e})") }))).into_response();
        }
    };
    let name = health.get("name").and_then(|x| x.as_str()).unwrap_or("node").to_string();

    // 2. Probe /api/capabilities for acceleration capabilities.
    let (ffmpeg_version, haccels, hw_encoders, accels) = match client
        .get(format!("{base}/api/capabilities"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            let c: Value = r.json().await.unwrap_or(Value::Null);
            let strs = |k: &str| -> Vec<String> {
                c.get(k)
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            };
            (
                c.get("ffmpegVersion").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_default(),
                strs("haccels"),
                strs("hwEncoders"),
                serde_json::from_value::<Vec<Accel>>(c.get("accels").cloned().unwrap_or(Value::Null))
                    .unwrap_or_default(),
            )
        }
        _ => (String::new(), vec![], vec![], vec![]),
    };

    // 3. Upsert into the registry (idempotent by name).
    let node = {
        let mut st = store.lock().unwrap();
        let id = match st.find_node_by_name(&name) {
            Some(n) => n.id,
            None => uuid::Uuid::new_v4().to_string(),
        };
        let node = Node {
            id,
            name: name.clone(),
            ffmpeg_version,
            haccels,
            hw_encoders,
            accels,
            status: "online".into(),
            last_heartbeat: now_ms(),
            current_job: None,
        };
        st.upsert_node(node.clone());
        node
    };

    Json(serde_json::to_value(node).unwrap_or(Value::Null)).into_response()
}

/// GET /api/nodes/:id — one node.
pub async fn get_node(State(store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    match store.lock().unwrap().get_node(&id) {
        Some(n) => match serde_json::to_value(n) {
            Ok(v) => Json(v).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "node not found"}))).into_response(),
    }
}

/// GET /api/nodes/:id/poll — long-poll (up to 20s) for the next dispatched job.
pub async fn node_poll(State(store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    if store.lock().unwrap().get_node(&id).is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown node"}))).into_response();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let claimed = store.lock().unwrap().claim_for_node(&id);
        if let Some(job) = claimed {
            return match serde_json::to_value(job) {
                Ok(v) => Json(json!({"job": v})).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
            };
        }
        if std::time::Instant::now() >= deadline {
            return Json(json!({"job": null})).into_response();
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportReq {
    job_id: String,
    status: String,
    #[serde(default)]
    progress: Option<f64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    output_file: Option<String>,
    #[serde(default)]
    output_size: Option<u64>,
}

/// POST /api/nodes/:id/report — a node reports job progress / completion.
pub async fn node_report(State(store): State<Store>, UrlPath(id): UrlPath<String>, Json(req): Json<ReportReq>) -> Response {
    {
        let mut s = store.lock().unwrap();
        if let Some(j) = s.jobs.get_mut(&req.job_id) {
            j.status = req.status.clone();
            if let Some(p) = req.progress {
                j.progress = p.clamp(0.0, 1.0);
            }
            if let Some(e) = req.error {
                j.error = Some(e);
            }
            if let Some(o) = req.output_file {
                j.output_file = o;
            }
            if let Some(sz) = req.output_size {
                j.output_size = Some(sz);
            }
            if req.status == "done" || req.status == "error" {
                j.finished_at = Some(now_ms());
            }
            if req.status == "running" && j.started_at.is_none() {
                j.started_at = Some(now_ms());
            }
        }
        if req.status == "done" || req.status == "error" {
            if let Some(n) = s.nodes.get_mut(&id) {
                n.current_job = None;
            }
        }
    }
    Json(json!({"ok": true})).into_response()
}

/// GET /api/nodes/:id/file/:name — a node fetches an input file from the server.
pub async fn node_fetch_file(UrlPath((_id, name)): UrlPath<(String, String)>) -> Response {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad filename"}))).into_response();
    }
    let path = uploads_dir().join(&name);
    if !path.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    match std::fs::read(&path) {
        Ok(b) => (StatusCode::OK, [("content-type", "application/octet-stream")], b).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// POST /api/nodes/:id/result — a node uploads a finished output (multipart:
/// fields `file`, `jobId`, `outputFile`). Stored in the server's outputs/.
/// The `file` field is streamed to a temp file on disk (not held in RAM), then
/// renamed to the declared `outputFile` — safe for multi-GB outputs.
pub async fn node_result(State(store): State<Store>, UrlPath(id): UrlPath<String>, mut mp: Multipart) -> Response {
    let mut job_id: Option<String> = None;
    let mut out_name: Option<String> = None;
    // Temp destination for the large `file` field (renamed once we know outputFile).
    let tmp_path = outputs_dir().join(format!(".inprogress_{}_{}", id, uuid::Uuid::new_v4()));
    let mut tmp_size: u64 = 0;
    let mut got_file = false;
    while let Ok(Some(field)) = mp.next_field().await {
        // dispatch on the FORM FIELD name ("file"/"jobId"/"outputFile"),
        // not the uploaded file's base name.
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "file" => {
                let mut file = match tokio::fs::File::create(&tmp_path).await {
                    Ok(f) => f,
                    Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("create temp: {e}")}))).into_response(),
                };
                let mut ok = true;
                let mut stream = field;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            tmp_size += bytes.len() as u64;
                            if let Err(e) = file.write_all(&bytes).await {
                                eprintln!("[node_result] write {id}: {e}");
                                ok = false;
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("[node_result] read {id}: {e}");
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    let _ = std::fs::remove_file(&tmp_path);
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "failed to store file"}))).into_response();
                }
                got_file = true;
            }
            "jobId" => {
                if let Ok(b) = field.bytes().await {
                    job_id = Some(String::from_utf8_lossy(&b).to_string());
                }
            }
            "outputFile" => {
                if let Ok(b) = field.bytes().await {
                    out_name = Some(String::from_utf8_lossy(&b).to_string());
                }
            }
            _ => {}
        }
    }
    let job_id = match job_id {
        Some(x) => x,
        None => { let _ = std::fs::remove_file(&tmp_path); return (StatusCode::BAD_REQUEST, Json(json!({"error": "missing jobId"}))).into_response(); }
    };
    let out_name = match out_name {
        Some(x) => x,
        None => { let _ = std::fs::remove_file(&tmp_path); return (StatusCode::BAD_REQUEST, Json(json!({"error": "missing outputFile"}))).into_response(); }
    };
    if out_name.contains("..") || out_name.contains('/') || out_name.contains('\\') {
        let _ = std::fs::remove_file(&tmp_path);
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad filename"}))).into_response();
    }
    if !got_file {
        let _ = std::fs::remove_file(&tmp_path);
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "missing file"}))).into_response();
    }
    let path = outputs_dir().join(&out_name);
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("write {e}")}))).into_response();
    }
    let size = tmp_size;
    {
        let mut s = store.lock().unwrap();
        if let Some(j) = s.jobs.get_mut(&job_id) {
            j.output_file = out_name.clone();
            j.output_size = Some(size);
        }
    }
    Json(json!({"ok": true, "file": out_name, "size": size})).into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DispatchReq {
    file: String,
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    crf: Option<u32>,
    /// Percentage of the source video bitrate (1..=500). Overrides `crf`.
    #[serde(default)]
    bitrate_pct: Option<u32>,
    #[serde(default)]
    node_id: Option<String>,
    /// If true, use the requested video_codec verbatim (no HW auto-rewrite).
    #[serde(default)]
    force_codec: Option<bool>,
}

/// POST /api/dispatch — create a transcode job targeted at a node. The target
/// node's best HW encoder for the requested codec family is auto-selected.
pub async fn dispatch(State(store): State<Store>, Json(req): Json<DispatchReq>) -> Response {
    if !uploads_dir().join(&req.file).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("input not found: {}", req.file)}))).into_response();
    }
    let vcodec = req.video_codec.unwrap_or_else(|| "h264".into());
    let acodec = req.audio_codec.unwrap_or_else(|| "aac".into());
    let container = req.container.unwrap_or_else(|| "mp4".into());

    let (node_id, node_name, effective, accelerated) = {
        let s = store.lock().unwrap();
        let node = if let Some(nid) = &req.node_id {
            s.get_node(nid)
        } else {
            s.pick_node(&codec_family(&vcodec))
        };
        let node = match node {
            Some(n) => n,
            None => return (StatusCode::BAD_REQUEST, Json(json!({"error": "no available node"}))).into_response(),
        };
        let (effective, accelerated) = if req.force_codec.unwrap_or(false) {
            (vcodec.clone(), false)
        } else {
            resolve_video_codec(&vcodec, &node.hw_encoders)
        };
        (node.id.clone(), node.name.clone(), effective, accelerated)
    };
    // lock dropped here

    let out_name = format!(
        "{}_{}_{}.{}",
        safe_base(&req.file),
        effective.replace("lib", "").replace('-', "_"),
        container,
        container
    );
    let id = uuid::Uuid::new_v4().to_string();
    let input_size = std::fs::metadata(uploads_dir().join(&req.file)).map(|m| m.len()).ok();
    // "% of source" bitrate mode: analyze the source once, compute the target.
    let analysis = probe_full(&uploads_dir().join(&req.file).to_string_lossy()).await;
    let video_bitrate = target_kbps(analysis.video_bit_rate, req.bitrate_pct);
    let job = Job {
        id: id.clone(),
        input_file: req.file.clone(),
        output_file: out_name,
        video_codec: vcodec.clone(),
        effective_video_codec: effective.clone(),
        audio_codec: acodec,
        container,
        crf: req.crf,
        video_bitrate,
        bitrate_pct: req.bitrate_pct,
        source_bit_rate: analysis.video_bit_rate,
        status: "queued".into(),
        progress: 0.0,
        created_at: now_ms(),
        started_at: None,
        finished_at: None,
        error: None,
        input_size,
        output_size: None,
        node_id: Some(node_id.clone()),
        executed_by: node_name.clone(),
        accelerated,
    };
    store.lock().unwrap().insert(job);
    Json(json!({
        "id": id,
        "nodeId": node_id,
        "node": node_name,
        "effectiveVideoCodec": effective,
        "accelerated": accelerated
    }))
    .into_response()
}

// ============================================================
//  main
// ============================================================

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("CT_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8400);
    let store: Store = Arc::new(Mutex::new(JobStore::default()));

    // mDNS: advertise this server on the LAN + browse for nodes.
    spawn_mdns(&store, port).await;

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/codecs", get(codecs))
        .route("/api/library", get(library_get))
        .route("/api/library/scan", post(library_scan))
        .route("/api/upload", post(upload))
        .route("/api/media/analyze", get(media_analyze))
        .route("/api/transcode", post(transcode))
        .route("/api/dispatch", post(dispatch))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/:id", get(get_job))
        .route("/api/download/:name", get(download))
        // node management (distributed transcoding)
        .route("/api/nodes", get(list_nodes))
        .route("/api/nodes/discovered", get(list_discovered_nodes))
        .route("/api/nodes/search", post(search_node))
        .route("/api/nodes/register", post(node_register))
        .route("/api/nodes/:id", get(get_node))
        .route("/api/nodes/:id/heartbeat", post(node_heartbeat))
        .route("/api/nodes/:id/poll", get(node_poll))
        .route("/api/nodes/:id/report", post(node_report))
        .route("/api/nodes/:id/file/:name", get(node_fetch_file))
        .route("/api/nodes/:id/result", post(node_result))
        .with_state(store)
        // Lift Multipart's 2MB default body limit (it silently breaks real
        // video uploads). Configurable via CT_MAX_UPLOAD_MB.
        .layer(DefaultBodyLimit::max(max_upload_bytes()))
        .layer(CorsLayer::permissive())
        // Static frontend: ServeDir serves / (index.html) and /assets/*.
        // No client-side routing, so unknown paths fall through to ServeDir's
        // default clean 404.
        .fallback_service(ServeDir::new(frontend_dir()));

    let addr = format!("0.0.0.0:{port}");
    println!("[CodecTrans] backend on {addr} | base={}", base_dir().display());
    println!("[CodecTrans] uploads={} outputs={}", uploads_dir().display(), outputs_dir().display());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app).await.unwrap();
}

/// Read a TXT property value as a UTF-8 string (best-effort).
fn txt_str(svc: &ResolvedService, key: &str) -> Option<String> {
    svc.get_property_val(key)
        .flatten()
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

/// Advertise this server on the LAN (mDNS) so nodes with CT_SERVER=auto can
/// find it, and continuously browse for nodes so `GET /api/nodes/discovered`
/// reflects what's on the LAN. Discovered nodes are recorded in the store.
async fn spawn_mdns(store: &Store, port: u16) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            println!("[CodecTrans] mDNS unavailable ({e}) — LAN discovery disabled");
            return;
        }
    };

    // 1. Advertise this server.
    let ip = lan_ip();
    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("ip".into(), ip.clone());
    props.insert("service".into(), "codectrans-server".into());
    if let Ok(mut info) = ServiceInfo::new(
        "_codectrans-server._tcp.local.",
        "codectrans-server",
        &mdns_hostname(),
        ip.as_str(),
        port,
        props,
    ) {
        // Advertise only on the LAN interface (skip docker/bridge/veth).
        if let Ok(ip_addr) = ip.parse::<IpAddr>() {
            info.set_interfaces(vec![IfKind::Addr(ip_addr)]);
        }
        let info = info.enable_addr_auto();
        match daemon.register(info) {
            Ok(()) => println!("[CodecTrans] advertised server on LAN (mDNS) @ {ip}:{port}"),
            Err(e) => println!("[CodecTrans] mDNS server register failed: {e}"),
        }
    }

    // 2. Browse for nodes and record what we find (the task keeps the daemon
    //    handle alive, so the advertisement persists for the process lifetime).
    match daemon.browse("_codectrans._tcp.local.") {
        Ok(mut rx) => {
            let store = store.clone();
            let keepalive = daemon;
            tokio::spawn(async move {
                let _keepalive = keepalive;
                loop {
                    match rx.recv_async().await {
                        Ok(ServiceEvent::ServiceResolved(svc)) => {
                            let svc: &ResolvedService = &svc;
                            if !svc.is_valid() {
                                continue;
                            }
                            let name = txt_str(&svc, "name")
                                .or_else(|| svc
                                    .fullname
                                    .strip_suffix(svc.ty_domain.as_str())
                                    .map(|s| s.to_string()))
                                .unwrap_or_else(|| "node".into());
                            let ip = svc
                                .get_addresses_v4()
                                .iter()
                                .next()
                                .map(|a| a.to_string())
                                .or_else(|| txt_str(&svc, "ip"))
                                .unwrap_or_default();
                            let hw = txt_str(&svc, "hw")
                                .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
                                .unwrap_or_default();
                            let ff = txt_str(&svc, "ffmpeg").unwrap_or_default();
                            let d = Discovered {
                                name,
                                ip,
                                port: svc.get_port(),
                                hw_encoders: hw,
                                ffmpeg_version: ff,
                                last_seen: now_ms(),
                            };
                            let mut st = store.lock().unwrap();
                            st.upsert_discovered(d);
                        }
                        Ok(_) => {}
                        Err(_) => break, // receiver closed
                    }
                }
            });
        }
        Err(e) => println!("[CodecTrans] mDNS browse failed: {e}"),
    }

    // 3. Periodically prune stale discovered entries.
    {
        let store = store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                store.lock().unwrap().prune_discovered(15_000);
            }
        });
    }
}
