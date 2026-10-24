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
    /// Set for library-automation jobs: path relative to the storage root, so
    /// the node can pull the source and the result lands next to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lib_path: Option<String>,
    /// Multi-GPU: the physical device index (0-based within its kind, as
    /// reported in `accels`) the node must pin this encode to
    /// (CUDA_VISIBLE_DEVICES / -vaapi_device / -qsv_device). None = let the
    /// backend pick (single-device machines, software).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_device: Option<u32>,
    /// Post-encode plan — the ops chained on the encode node's output
    /// port (verify/replace/saveHere/requeue), executed in order when the
    /// result arrives. Empty = plain encode (output lands next to source).
    #[serde(default)]
    pub post: Vec<PostOp>,
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
    /// UI display-name override (rename from the dashboard gear). None = use
    /// the registration name (which the node itself keeps sending).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub ffmpeg_version: String,
    /// HW acceleration methods from `ffmpeg -haccels` (cuda, vaapi, qsv, ...).
    pub haccels: Vec<String>,
    /// Encoders that are HW-accelerated (h264_nvenc, hevc_vaapi, ...).
    pub hw_encoders: Vec<String>,
    /// Grouped per physical device (GPU #0 nvenc, CPU #0 qsv/vaapi, ...).
    pub accels: Vec<Accel>,
    /// Worker-resource selection (dashboard gear): which accel backends this
    /// node may be used for (e.g. disable the Intel iGPU because its encodes
    /// contend with the dGPU for CPU). `None` (key absent = never configured)
    /// = all backends; `Some(list)` = exactly that list — an empty list
    /// disables ALL HW workers (software only if `allow_software`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_backends: Option<Vec<String>>,
    /// When false, the node is never given a software-encoder fallback job.
    #[serde(default = "default_true")]
    pub allow_software: bool,
    /// How many jobs the node may run at once (e.g. a 2-slice NVENC GPU can
    /// do 2-3 encodes without losing throughput). Default 1.
    #[serde(default = "default_one")]
    pub max_jobs: u32,
    pub status: String,          // online | offline
    pub last_heartbeat: i64,
    /// When this node was first seen in the registry (epoch ms; persisted in Redis).
    pub first_seen: i64,
    pub current_job: Option<String>,
    // ── live stats (latest heartbeat) ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_used_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_total_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_temp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_util: Option<u64>,
}

/// The name shown in the UI: display override, else the registration name.
impl Node {
    pub fn display(&self) -> &str {
        self.display_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.name)
    }
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
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub path: String,          // relative to the folder (or storage root for ad-hoc)
    pub name: String,          // base filename
    /// Registered folder id this entry belongs to (None = ad-hoc root scan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
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

impl LibraryEntry {
    /// Catalog key: unique across folders ("{folder_id}/{rel}", or the
    /// relative path itself for ad-hoc root scans). Also stored on the job
    /// as `lib_path` so the node can fetch the source and the result is
    /// placed next to it.
    pub fn key(&self) -> String {
        match &self.folder {
            Some(f) => format!("{f}/{}", self.path),
            None => self.path.clone(),
        }
    }
}

pub struct JobStore {
    jobs: HashMap<String, Job>,
    discovered: HashMap<String, Discovered>,
    library: HashMap<String, LibraryEntry>, // keyed by relative path
    storage_root: String,
    /// Node registry lives in Redis (persistent identity + liveness); this is
    /// the shared connection handle.
    redis: RConn,
}

impl JobStore {
    pub fn new(redis: RConn) -> Self {
        JobStore {
            jobs: HashMap::new(),
            discovered: HashMap::new(),
            library: HashMap::new(),
            storage_root: String::new(),
            redis,
        }
    }
    /// Clone the Redis handle out (drop any store guard before awaiting on it).
    pub fn redis(&self) -> RConn {
        self.redis.clone()
    }
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

    // ---- nodes: see the Redis node registry section below ----
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
            Some(j)
        } else {
            None
        }
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
            self.library.insert(e.key(), e);
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
//  Node registry — Redis (persistent identity + liveness)
//
//  The node map used to live in memory: a server restart wiped it (fresh
//  UUIDs → orphaned queued jobs, offline history lost, no filtering). The
//  registry now lives in Redis:
//
//    ct:node:{id}          HASH  name, ffmpegVersion, haccels, hwEncoders,
//                                accels (JSON), firstSeen, lastHeartbeat,
//                                currentJob
//    ct:node:name:{name}   STRING -> id     (idempotent re-register by name)
//    ct:node:ids           SET of all node ids
//    ct:online:{id}        STRING "1" + TTL (refreshed by register/heartbeat)
//
//  Jobs stay in memory (ephemeral); the registry is the only persisted state.
// ============================================================

const ONLINE_TTL_SECS: u64 = 20;
const RK_IDS: &str = "ct:node:ids";

fn rk_node(id: &str) -> String {
    format!("ct:node:{}", id)
}
fn rk_online(id: &str) -> String {
    format!("ct:online:{}", id)
}
fn rk_name(name: &str) -> String {
    format!("ct:node:name:{}", name)
}

type RConn = redis::aio::MultiplexedConnection;

type NodeHash = HashMap<String, String>;

fn hash_get<'a>(h: &'a NodeHash, k: &str) -> Option<&'a str> {
    h.get(k).map(|s| s.as_str())
}

fn parse_node(id: &str, h: &NodeHash, online: bool) -> Option<Node> {
    let name = hash_get(h, "name")?.to_string();
    let mut accels: Vec<Accel> = serde_json::from_str(hash_get(h, "accels").unwrap_or("[]")).unwrap_or_default();
    let hw_encoders: Vec<String> = serde_json::from_str(hash_get(h, "hwEncoders").unwrap_or("[]")).unwrap_or_default();
    let haccels: Vec<String> = serde_json::from_str(hash_get(h, "haccels").unwrap_or("[]")).unwrap_or_default();
    let enabled_backends: Option<Vec<String>> = hash_get(h, "enabledBackends")
        .map(|s| serde_json::from_str(s).unwrap_or_default());
    let first_seen = hash_get(h, "firstSeen").and_then(|s| s.parse::<i64>().ok()).unwrap_or_else(now_ms);
    let last_heartbeat = hash_get(h, "lastHeartbeat").and_then(|s| s.parse::<i64>().ok()).unwrap_or_else(now_ms);
    let cur = hash_get(h, "currentJob").unwrap_or("");
    let display_name = hash_get(h, "displayName").map(|s| s.to_string()).filter(|s| !s.is_empty());
    let allow_software = hash_get(h, "allowSoftware").map(|s| s == "true").unwrap_or(true);
    let max_jobs = hash_get(h, "maxJobs").and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).clamp(1, 16);
    let stat_u64 = |k: &str| -> Option<u64> { hash_get(h, k).and_then(|s| s.parse::<u64>().ok()) };
    let stat_f64 = |k: &str| -> Option<f64> { hash_get(h, k).and_then(|s| s.parse::<f64>().ok()) };
    accels.sort_by(|a, b| a.backend.cmp(&b.backend));
    Some(Node {
        id: id.to_string(),
        name,
        display_name,
        ffmpeg_version: hash_get(h, "ffmpegVersion").unwrap_or("").to_string(),
        hw_encoders,
        haccels,
        accels,
        enabled_backends,
        allow_software,
        max_jobs,
        status: if online { "online".into() } else { "offline".into() },
        last_heartbeat,
        first_seen,
        current_job: if cur.is_empty() { None } else { Some(cur.to_string()) },
        cpu_pct: stat_f64("cpuPct"),
        ram_used_mb: stat_u64("ramUsedMb"),
        ram_total_mb: stat_u64("ramTotalMb"),
        gpu_temp: stat_u64("gpuTemp"),
        gpu_util: stat_u64("gpuUtil"),
    })
}

async fn hgetall_node(c: RConn, id: &str) -> Option<Node> {
    let mut c = c;
    let h: NodeHash = redis::cmd("HGETALL")
        .arg(rk_node(id))
        .query_async(&mut c)
        .await
        .ok()?;
    let ttl: i64 = redis::cmd("TTL")
        .arg(rk_online(id))
        .query_async(&mut c)
        .await
        .ok()
        .flatten()
        .unwrap_or(-2);
    parse_node(id, &h, ttl > 0)
}

/// Register (or re-register) a node by name. A node that registered under the
/// same name before keeps its id across restarts — that is what lets queued
/// jobs survive a server restart instead of becoming orphans.
async fn reg_node(
    c: RConn,
    name: &str,
    ffmpeg_version: &str,
    haccels: &[String],
    hw_encoders: &[String],
    accels: &[Accel],
) -> Node {
    let mut c = c;
    let now = now_ms();
    let existing: Option<String> = redis::cmd("GET")
        .arg(rk_name(name))
        .query_async(&mut c)
        .await
        .ok()
        .flatten();
    let (id, is_new) = match existing {
        Some(i) if !i.is_empty() => (i, false),
        _ => (uuid::Uuid::new_v4().to_string(), true),
    };
    let first_seen = if is_new {
        now
    } else {
        hgetall_node(c.clone(), &id).await.map(|n| n.first_seen).unwrap_or(now)
    };
    let mut pipe = redis::pipe();
    pipe.atomic();
    if is_new {
        pipe.cmd("SET").arg(rk_name(name)).arg(&id);
        pipe.cmd("SADD").arg(RK_IDS).arg(&id);
    }
    pipe.cmd("HSET")
        .arg(rk_node(&id))
        .arg("name").arg(name)
        .arg("ffmpegVersion").arg(ffmpeg_version)
        .arg("haccels").arg(serde_json::to_string(haccels).unwrap_or_else(|_| "[]".into()))
        .arg("hwEncoders").arg(serde_json::to_string(hw_encoders).unwrap_or_else(|_| "[]".into()))
        .arg("accels").arg(serde_json::to_string(accels).unwrap_or_else(|_| "[]".into()))
        .arg("firstSeen").arg(first_seen)
        .arg("lastHeartbeat").arg(now);
    if is_new {
        pipe.cmd("HSET").arg(rk_node(&id)).arg("currentJob").arg("");
    }
    pipe.cmd("SETEX").arg(rk_online(&id)).arg(ONLINE_TTL_SECS).arg("1");
    let _: () = pipe.query_async(&mut c).await.expect("redis register");
    hgetall_node(c.clone(), &id).await.unwrap_or(Node {
        id,
        name: name.to_string(),
        display_name: None,
        ffmpeg_version: ffmpeg_version.to_string(),
        hw_encoders: hw_encoders.to_vec(),
        haccels: haccels.to_vec(),
        accels: accels.to_vec(),
        enabled_backends: None,
        allow_software: true,
        max_jobs: 1,
        status: "online".into(),
        last_heartbeat: now,
        first_seen,
        current_job: None,
        cpu_pct: None,
        ram_used_mb: None,
        ram_total_mb: None,
        gpu_temp: None,
        gpu_util: None,
    })
}

/// Heartbeat: refresh liveness + last_heartbeat + optional live stats
/// (CPU/RAM/GPU sampled by the node). None if the node is unknown.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatStats {
    pub cpu_pct: Option<f64>,
    pub ram_used_mb: Option<u64>,
    pub ram_total_mb: Option<u64>,
    pub gpu_temp: Option<u64>,
    pub gpu_util: Option<u64>,
}

async fn beat_node(c: RConn, id: &str, stats: &HeartbeatStats) -> Option<Node> {
    let mut pipe = redis::pipe();
    pipe.cmd("HSET").arg(rk_node(id)).arg("lastHeartbeat").arg(now_ms());
    // Stats: report when present, clear when the node stopped reporting
    // (older node binary) so the dashboard doesn't show stale numbers.
    pipe.cmd("HSET")
        .arg(rk_node(id))
        .arg("cpuPct")
        .arg(stats.cpu_pct.map(|x| format!("{x:.1}")).unwrap_or_default());
    pipe.cmd("HSET")
        .arg(rk_node(id))
        .arg("ramUsedMb")
        .arg(stats.ram_used_mb.map(|x| x.to_string()).unwrap_or_default());
    pipe.cmd("HSET")
        .arg(rk_node(id))
        .arg("ramTotalMb")
        .arg(stats.ram_total_mb.map(|x| x.to_string()).unwrap_or_default());
    pipe.cmd("HSET")
        .arg(rk_node(id))
        .arg("gpuTemp")
        .arg(stats.gpu_temp.map(|x| x.to_string()).unwrap_or_default());
    pipe.cmd("HSET")
        .arg(rk_node(id))
        .arg("gpuUtil")
        .arg(stats.gpu_util.map(|x| x.to_string()).unwrap_or_default());
    pipe.cmd("SETEX").arg(rk_online(id)).arg(ONLINE_TTL_SECS).arg("1");
    let mut c = c;
    let _: () = pipe.query_async(&mut c).await.ok()?;
    hgetall_node(c.clone(), id).await
}

async fn set_current_job(c: RConn, id: &str, job: Option<&str>) {
    let v = job.unwrap_or("");
    let mut c = c;
    let _ = redis::cmd("HSET")
        .arg(rk_node(id))
        .arg("currentJob")
        .arg(v)
        .query_async::<i32>(&mut c)
        .await;
}

/// All nodes, filtered. status: "online" (default) | "offline" | "all";
/// backend: only nodes exposing that HW acceleration (nvenc/qsv/…);
/// family: only nodes with an HW encoder for that codec family.
async fn list_nodes_redis(c: RConn, status: &str, backend: Option<&str>, family: Option<&str>) -> Vec<Node> {
    let mut c = c;
    let ids: Vec<String> = redis::cmd("SMEMBERS")
        .arg(RK_IDS)
        .query_async(&mut c)
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for id in ids {
        if let Some(n) = hgetall_node(c.clone(), &id).await {
            let on = n.status == "online";
            match status {
                "online" if !on => continue,
                "offline" if on => continue,
                _ => {}
            }
            if let Some(b) = backend {
                if !n.accels.iter().any(|a| a.backend == b) {
                    continue;
                }
            }
            if let Some(f) = family {
                if !n.hw_encoders.iter().any(|h| codec_family(h) == f) {
                    continue;
                }
            }
            out.push(n);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Auto-pick a node: prefer an online node with an HW encoder for the
/// requested codec family, else any online node.
/// Pick a node index from the online list: a node with FREE capacity (its
/// overall active jobs < its maxJobs) that has an HW encoder for the family
/// (respecting the per-node worker-resource selection) is preferred; else
/// any free node.
fn pick_node_idx(online: &[Node], counts: &std::collections::HashMap<String, u32>, family: &str) -> Option<usize> {
    let free = |n: &Node| counts.get(&n.id).copied().unwrap_or(0) < n.max_jobs;
    for (i, n) in online.iter().enumerate() {
        if free(n) && usable_hw_encoders(n).iter().any(|h| codec_family(h) == family) {
            return Some(i);
        }
    }
    online.iter().position(|n| free(n))
}

/// Forced codec (forceCodec=true): pick a node that actually has this exact
/// encoder within its enabled backends (e.g. forced hevc_vaapi must not go to
/// a node with VA-API disabled or without it).
fn pick_node_encoder_idx(online: &[Node], counts: &std::collections::HashMap<String, u32>, encoder: &str) -> Option<usize> {
    let free = |n: &Node| counts.get(&n.id).copied().unwrap_or(0) < n.max_jobs;
    for (i, n) in online.iter().enumerate() {
        if free(n) && usable_hw_encoders(n).iter().any(|h| h == encoder) {
            return Some(i);
        }
    }
    online.iter().position(|n| free(n))
}

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
//  Dashboard server stats (CPU/RAM of the server host)
// ============================================================

fn read_cpu_times() -> Option<(u64, u64)> {
    // (idle, total) jiffies from /proc/stat's aggregate "cpu" line
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let f: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let total = f.iter().sum::<u64>();
    let idle = f.get(3).copied().unwrap_or(0) + f.get(4).copied().unwrap_or(0); // idle + iowait
    Some((idle, total))
}

fn read_mem() -> Option<(u64, u64)> {
    // (total_bytes, available_bytes) from /proc/meminfo
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let get = |k: &str| -> Option<u64> {
        s.lines().find_map(|l| {
            let (a, b) = l.split_once(':')?;
            (a.trim() == k)
                .then(|| b.trim().trim_end_matches(" kB").parse::<u64>().ok())
                .flatten()
        })
        .map(|kb| kb * 1024)
    };
    Some((get("MemTotal")?, get("MemAvailable").unwrap_or(0)))
}

/// GET /api/server-stats — CPU% (sampled over ~250 ms) + RAM totals for the
/// dashboard. Non-Linux hosts report nulls rather than failing.
pub async fn server_stats() -> impl IntoResponse {
    let (cpu_pct, cores) = {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let a = read_cpu_times();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let b = read_cpu_times();
        let pct = match (a, b) {
            (Some((ia, ta)), Some((ib, tb))) if tb > ta => {
                (100.0 * (tb - ta - ib.saturating_sub(ia)) as f64 / (tb - ta) as f64).min(100.0)
            }
            _ => 0.0,
        };
        (pct, cores)
    };
    let (ram_total, ram_used, ram_pct) = match read_mem() {
        Some((t, a)) => {
            let u = t.saturating_sub(a);
            (
                Some(t),
                Some(u),
                Some(if t > 0 { 100.0 * u as f64 / t as f64 } else { 0.0 }),
            )
        }
        None => (None, None, None),
    };
    Json(json!({
        "cpuPct": cpu_pct,
        "cores": cores,
        "ramTotal": ram_total,
        "ramUsed": ram_used,
        "ramPct": ram_pct,
    }))
}

// ============================================================
//  Library automation — saved settings + auto-queue on scan
// ============================================================

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Logical video codec (h264/h265/av1/...); rewritten per-node by the
    /// usual HW-encoder resolution.
    #[serde(default)]
    pub video_codec: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    /// "crf" | "pct" (percent of source bitrate).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub crf: Option<u32>,
    #[serde(default)]
    pub bitrate_pct: Option<u32>,
}

fn auto_cfg_path() -> PathBuf {
    base_dir().join("auto.json")
}

fn load_auto() -> AutoConfig {
    std::fs::read_to_string(auto_cfg_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_auto(c: &AutoConfig) -> std::io::Result<()> {
    std::fs::write(auto_cfg_path(), serde_json::to_string_pretty(c)?)
}

// ── folders + logic blocks (persisted in library.json) ──────────

/// One entry of an ordered encoder-preference list (`TargetCfg.choices`).
/// The server walks the list in order and uses the FIRST choice the selected
/// node can actually run (HW), so a folder can say "av1 on the GPU, else
/// hevc" or "hevc on the nvenc box, h264 on the Mac" without per-node graphs.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TargetChoice {
    /// Logical video codec (h264/h265/av1/...).
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Accelerator constraint for this choice:
    ///  - a specific backend: "nvenc" | "vaapi" | "qsv" | "amf" | "mft" |
    ///    "videotoolbox" (alias "vt")
    ///  - a device class: "gpu" | "cpu"
    ///  - None = any HW accelerator that can do the codec.
    #[serde(default)]
    pub accel: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    /// "crf" | "pct" (percent of source bitrate).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub crf: Option<u32>,
    #[serde(default)]
    pub bitrate_pct: Option<u32>,
}

/// Encoding target for a folder or a logic block.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TargetCfg {
    /// Logical video codec (h264/h265/av1/...); rewritten per-node to the
    /// actual HW encoder at dispatch time. Ignored when `choices` is set.
    #[serde(default)]
    pub video_codec: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub container: Option<String>,
    /// "crf" | "pct" (percent of source bitrate).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub crf: Option<u32>,
    #[serde(default)]
    pub bitrate_pct: Option<u32>,
    /// Ordered encoder preferences. When non-empty this takes precedence over
    /// the single `video_codec`: the server picks a node that can run at least
    /// one choice, then resolves the first choice that node can run.
    #[serde(default)]
    pub choices: Vec<TargetChoice>,
    /// Post-encode health check: ffprobe the finished file (video stream
    /// present, duration within 50% of the source). On failure the output is
    /// deleted and the entry marked error (requeue retries it).
    #[serde(default)]
    pub verify_output: bool,
    /// Replace the original: the finished file takes the source's path and
    /// the original is kept next to it as `name.orig.ext`. Library jobs only.
    /// (A second replace overwrites the previous `.orig` backup.)
    #[serde(default)]
    pub replace_original: bool,
}

impl From<AutoConfig> for TargetCfg {
    fn from(a: AutoConfig) -> Self {
        TargetCfg {
            video_codec: a.video_codec,
            audio_codec: a.audio_codec,
            container: a.container,
            mode: a.mode,
            crf: a.crf,
            bitrate_pct: a.bitrate_pct,
            choices: Vec::new(),
            verify_output: false,
            replace_original: false,
        }
    }
}

/// A watched library folder + its default encoding target.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRule {
    pub id: String,
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub target: TargetCfg,
    /// Named-logic reference (dashboard "로직" selector): when set, this
    /// folder's new files are processed by that saved logic graph. None =
    /// bulk-apply the folder's default target below.
    #[serde(default)]
    pub logic_id: Option<String>,
    /// Per-folder node graph — takes precedence over the global library
    /// graph when it has condition/action nodes (empty = inherit global).
    /// Legacy: superseded by `logic_id` + the named-logic store.
    #[serde(default)]
    pub graph: WorkflowGraph,
}

fn default_true() -> bool {
    true
}
fn default_one() -> u32 {
    1
}

/// Block condition — which newly found files the block applies to.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    /// "always" | "srcCodec" | "sizeGt" | "sizeLt" | "bitrateGt" |
    /// "bitrateLt" | "nodeHasHw" | "nameContains"
    #[serde(default)]
    pub kind: String,
    /// srcCodec: source codec name to match (h264, hevc, av1, ...)
    #[serde(default)]
    pub codec: Option<String>,
    /// sizeGt/sizeLt: threshold in MB; bitrateGt/bitrateLt: Mbps
    #[serde(default)]
    pub value: Option<f64>,
    /// nameContains: substring the file name must contain (case-insensitive)
    #[serde(default)]
    pub pattern: Option<String>,
    /// nodeHasHw: codec family that must have an HW encoder on some online
    /// node (h264, h265, av1, vp9)
    #[serde(default)]
    pub hw_family: Option<String>,
    /// nodeHasHw: device class filter — "gpu" | "cpu" (None = either),
    /// e.g. "av1 must be HW-encoded on a GPU (nvenc/videotoolbox), not
    /// on an iGPU path (qsv/vaapi)".
    #[serde(default)]
    pub accel_class: Option<String>,
}

/// Block action — what to do with matching files.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    /// "encode" | "skip" | "healthcheck"
    /// post-encode (chain on the encode output port):
    /// "verify" | "replace" | "saveHere" | "requeue"
    #[serde(default)]
    pub kind: String,
    /// "encode": the target to use (None = folder default)
    #[serde(default)]
    pub target: Option<TargetCfg>,
    /// "encode": never send to a node without a working HW encoder for
    /// this target's codec family — no software fallback, no other-node
    /// fallback. When nothing online can do it, the entry errors out
    /// (a later rescan retries it).
    #[serde(default)]
    pub hw_only: bool,
    /// "verify": which subset to health-check — "all" (default) | "pct"
    /// (random `verify_value`%) | "every" (every `verify_value`th file).
    #[serde(default)]
    pub verify_mode: Option<String>,
    /// "verify": the number for pct/every.
    #[serde(default)]
    pub verify_value: Option<u32>,
    /// "verify": what a failed check does — "error" (default) |
    /// "requeue" (entry goes back to the queue for another attempt).
    #[serde(default)]
    pub verify_on_failure: Option<String>,
}

/// One automation block: condition → action. Blocks are evaluated in order,
/// top to bottom; the first match wins.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogicBlock {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub condition: Condition,
    #[serde(default)]
    pub action: Action,
    #[serde(default)]
    pub label: Option<String>,
}

/// One saved (named) logic graph, picked from the library tab per folder.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NamedLogic {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub graph: WorkflowGraph,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct LibraryConfig {
    #[serde(default)]
    pub folders: Vec<FolderRule>,
    #[serde(default)]
    pub blocks: Vec<LogicBlock>,
    /// ComfyUI-style node graph (nodes + directed edges). When it has
    /// nodes, evaluation uses the graph; the legacy `blocks` list is a
    /// linear-chain fallback (auto-migrated for display).
    #[serde(default)]
    pub graph: WorkflowGraph,
    /// Saved named logics — the library tab assigns one (or none) per folder.
    #[serde(default)]
    pub logics: Vec<NamedLogic>,
    /// Basenames of re-encoded outputs ever produced (tombstone). Skipped by
    /// rescans so a folder removed+re-added (or a server restart) never
    /// picks up our own outputs and re-encodes them.
    #[serde(default)]
    pub tombstone: Vec<String>,
}

// ── node-graph workflow (ComfyUI style) ─────────────────────────
/// One node on the automation canvas.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub id: String,
    /// "start" | "condition" | "action"
    #[serde(default)]
    pub kind: String,
    /// condition nodes: which files pass
    #[serde(default)]
    pub condition: Condition,
    /// action nodes: what to do (encode/skip/healthcheck)
    #[serde(default)]
    pub action: Action,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional user-set header title (falls back to an auto-generated
    /// description of the condition/action).
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

/// A directed edge. `from_port` is "true"/"false" on condition nodes
/// (which branch the file takes); absent for the single start output.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    pub from: String,
    #[serde(default)]
    pub from_port: Option<String>,
    pub to: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGraph {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
}

/// A post-encode operation — chained on the encode action's OUTPUT port.
/// The server executes the whole plan (in chain order) when the finished
/// file arrives in `node_result`. The node never sees it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum PostOp {
    /// ffprobe the output: video stream present, positive duration, at
    /// least half the source's length. `mode`: "all" (default) | "pct"
    /// (random `value`%) | "every" (every Nth file by key hash — cheap
    /// sampling so a big folder isn't probed file by file).
    /// `on_failure`: "error" (default) | "requeue" (back to the queue).
    Verify {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        value: Option<u32>,
        #[serde(default)]
        on_failure: Option<String>,
    },
    /// The finished file takes the SOURCE path; the original is kept as
    /// `stem.orig.ext` (tombstoned so rescans ignore it).
    Replace,
    /// Explicit "keep the output next to the source" step (already true
    /// for library jobs — makes it a visible node on the graph).
    SaveHere,
    /// Put the entry back in the queue (status pending) after the rest of
    /// the plan ran. A rescan re-dispatches it — combine with a different
    /// folder logic to reprocess, or expect a re-encode loop if the same
    /// logic applies.
    Requeue,
}

/// FNV-1a — stable across restarts (unlike a counter) so "every Nth"
/// sampling picks the SAME files on every scan.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Does this entry get a health check, given the sampling mode?
fn verify_selected(key: &str, mode: Option<&str>, value: Option<u32>) -> bool {
    let h = fnv1a(key);
    match mode {
        Some("pct") => {
            let p = value.unwrap_or(100).clamp(1, 100);
            h % 100u64 < p as u64
        }
        Some("every") => {
            let n = value.unwrap_or(1).max(1);
            h % (n as u64) == 0
        }
        _ => true, // "all" / unset
    }
}

/// Legacy `blocks` → linear graph: start → c1 →a1; c1(false) → c2 →a2; …
/// so an old block list and its migrated graph mean exactly the same thing.
fn migrate_blocks_to_graph(blocks: &[LogicBlock]) -> WorkflowGraph {
    let live: Vec<&LogicBlock> = blocks.iter().filter(|b| b.enabled).collect();
    let mut nodes = vec![GraphNode {
        id: "start".into(),
        kind: "start".into(),
        enabled: true,
        x: 40.0,
        y: 160.0,
        ..Default::default()
    }];
    let mut edges = Vec::new();
    let mut prev_false: Option<String> = None;
    for (i, b) in live.iter().enumerate() {
        let cid = format!("c{}", i + 1);
        let aid = format!("a{}", i + 1);
        nodes.push(GraphNode {
            id: cid.clone(),
            kind: "condition".into(),
            condition: b.condition.clone(),
            enabled: true,
            x: 340.0,
            y: 30.0 + (i as f64) * 200.0,
            ..Default::default()
        });
        nodes.push(GraphNode {
            id: aid.clone(),
            kind: "action".into(),
            action: b.action.clone(),
            enabled: true,
            x: 660.0,
            y: 30.0 + (i as f64) * 200.0,
            ..Default::default()
        });
        let from = prev_false.clone().unwrap_or_else(|| "start".into());
        // start→c1 is portless; cN(false)→cN+1 rides the ✗ branch
        let port = if prev_false.is_some() { Some("false".into()) } else { None };
        edges.push(GraphEdge { from, from_port: port, to: cid.clone() });
        edges.push(GraphEdge { from: cid.clone(), from_port: Some("true".into()), to: aid });
        prev_false = Some(cid);
    }
    WorkflowGraph { nodes, edges }
}

fn lib_cfg_path() -> PathBuf {
    base_dir().join("library.json")
}

fn load_lib_cfg() -> LibraryConfig {
    std::fs::read_to_string(lib_cfg_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Process-wide lock for library.json read-modify-write cycles. The file is
/// written whole (folders + logics + blocks + tombstones) by several handlers,
/// so without this a concurrent scan's `persist_catalog` can commit a stale
/// snapshot and silently delete logics/folders added in the meantime.
static LIB_CFG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn save_lib_cfg(c: &LibraryConfig) -> std::io::Result<()> {
    std::fs::write(lib_cfg_path(), serde_json::to_string_pretty(c)?)
}

fn catalog_path() -> PathBuf {
    base_dir().join("catalog.json")
}

fn load_catalog() -> Vec<LibraryEntry> {
    std::fs::read_to_string(catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the in-memory catalog to disk and grow the re-encoded-name
/// tombstone (see LibraryConfig::tombstone).
fn persist_catalog(store: &Store) {
    let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    persist_catalog_locked(store);
}
fn persist_catalog_locked(store: &Store) {
    let entries = store.lock().unwrap().list_library();
    let _ = std::fs::write(
        catalog_path(),
        serde_json::to_string(&entries).unwrap_or_default(),
    );
    let mut c = load_lib_cfg();
    let mut seen: std::collections::HashSet<String> =
        c.tombstone.iter().cloned().collect();
    for e in &entries {
        if let Some(r) = &e.reencoded {
            seen.insert(r.clone());
        }
    }
    c.tombstone = seen.into_iter().collect();
    c.tombstone.sort();
    let _ = save_lib_cfg(&c);
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoldersBody {
    folders: Vec<FolderRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlocksBody {
    blocks: Option<Vec<LogicBlock>>,
    #[serde(default)]
    graph: Option<WorkflowGraph>,
}

/// GET /api/library/blocks — returns the legacy list AND the node graph
/// (auto-migrated from the legacy list when no graph is stored yet).
pub async fn blocks_get(State(_store): State<Store>) -> impl IntoResponse {
    let c = load_lib_cfg();
    let graph = if c.graph.nodes.is_empty() && !c.blocks.is_empty() {
        migrate_blocks_to_graph(&c.blocks)
    } else {
        c.graph
    };
    Json(json!({ "blocks": c.blocks, "graph": graph }))
}

/// POST /api/library/blocks — save the block list and/or the node graph.
/// A saved non-empty graph takes precedence at evaluation time.
pub async fn blocks_post(State(_store): State<Store>, Json(b): Json<BlocksBody>) -> Response {
    let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = load_lib_cfg();
    if let Some(blocks) = b.blocks.clone() {
        c.blocks = blocks;
    }
    if let Some(graph) = b.graph.clone() {
        c.graph = graph;
    }
    match save_lib_cfg(&c) {
        Ok(_) => Json(json!({ "ok": true, "blocks": c.blocks, "graph": c.graph })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// GET /api/library/folders/:id/blocks — the per-folder node graph (empty =
/// inherit the global library graph).
pub async fn folder_blocks_get(
    State(_store): State<Store>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let c = load_lib_cfg();
    match c.folders.iter().find(|f| f.id == id) {
        Some(f) => Json(json!({ "blocks": Vec::<LogicBlock>::new(), "graph": f.graph })).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "folder not found" })))
            .into_response(),
    }
}

/// POST /api/library/folders/:id/blocks — save a folder's own node graph.
/// An empty graph clears the override (the folder inherits the global graph).
pub async fn folder_blocks_post(
    State(_store): State<Store>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<BlocksBody>,
) -> Response {
    let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = load_lib_cfg();
    match c.folders.iter_mut().find(|f| f.id == id) {
        Some(f) => {
            if let Some(graph) = b.graph {
                f.graph = graph;
            }
            let g_saved = f.graph.clone();
            match save_lib_cfg(&c) {
                Ok(_) => Json(json!({ "ok": true, "graph": g_saved })).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response(),
            }
        }
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "folder not found" })))
            .into_response(),
    }
}

/// GET /api/library/folders
pub async fn folders_get(State(_store): State<Store>) -> impl IntoResponse {
    Json(json!({ "folders": load_lib_cfg().folders }))
}

// ── saved named logics (assigned per folder from the library tab) ──

/// GET /api/logic — the saved logic list.
pub async fn logics_get(State(_store): State<Store>) -> impl IntoResponse {
    Json(json!({ "logics": load_lib_cfg().logics }))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogicBody {
    /// Existing id when updating (absent = create new).
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub graph: WorkflowGraph,
}

/// POST /api/logic — create (no id) or update (with id) a named logic.
pub async fn logics_post(State(_store): State<Store>, Json(b): Json<LogicBody>) -> Response {
    let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = load_lib_cfg();
    let name = b.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "name required" }))).into_response();
    }
    match b.id.as_deref().and_then(|id| c.logics.iter_mut().find(|l| l.id == id)) {
        Some(l) => {
            l.name = name.to_string();
            l.graph = b.graph;
        }
        None => {
            c.logics.push(NamedLogic {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                graph: b.graph,
            });
        }
    }
    let logic = c.logics.last().unwrap().clone();
    match save_lib_cfg(&c) {
        Ok(_) => Json(json!({ "ok": true, "logic": logic, "logics": c.logics })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// DELETE /api/logic/:id — remove a named logic. Folders still referencing it
/// fall back to their bulk target (a dangling id is harmless at scan time).
pub async fn logic_delete(State(_store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = load_lib_cfg();
    let before = c.logics.len();
    c.logics.retain(|l| l.id != id);
    if c.logics.len() == before {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "logic not found" }))).into_response();
    }
    match save_lib_cfg(&c) {
        Ok(_) => Json(json!({ "ok": true, "logics": c.logics })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// POST /api/library/folders — save the folder list. Catalog entries of
/// removed folders are KEPT (marked "제거됨" in the UI) so re-adding the
/// folder restores their done state instead of re-queueing everything.
pub async fn folders_post(State(store): State<Store>, Json(mut b): Json<FoldersBody>) -> Response {
    let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = load_lib_cfg();
    // Preserve per-folder graphs: the frontend's folder list is loaded at tab
    // open and may predate a later graph edit, so an empty incoming graph
    // must not wipe a stored one (graphs are managed via the
    // /folders/:id/blocks endpoint).
    for f in b.folders.iter_mut() {
        if f.graph.nodes.is_empty() {
            if let Some(existing) = c.folders.iter().find(|e| e.id == f.id) {
                if !existing.graph.nodes.is_empty() {
                    f.graph = existing.graph.clone();
                }
            }
        }
    }
    c.folders = b.folders.clone();
    if let Err(e) = save_lib_cfg(&c) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }
    persist_catalog_locked(&store);
    Json(json!({ "ok": true, "folders": b.folders })).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequeueReq {
    key: String,
}

/// POST /api/library/requeue — reset a catalog entry to "pending" so the
/// next scan re-queues it (the per-file ⟳ re-encode button).
pub async fn library_requeue(State(store): State<Store>, Json(req): Json<RequeueReq>) -> Response {
    let found = {
        let mut s = store.lock().unwrap();
        s.update_library_entry(&req.key, |e| {
            e.status = "pending".into();
            e.error = None;
            e.reencoded = None;
            e.new_size = None;
        })
        .is_some()
    };
    if !found {
        return (
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("entry not found: {}", req.key) })),
            )
                .into_response()
        );
    }
    persist_catalog(&store);
    Json(json!({ "ok": true })).into_response()
}

pub async fn auto_get(State(_store): State<Store>) -> impl IntoResponse {
    Json(load_auto())
}

pub async fn auto_post(State(_store): State<Store>, Json(c): Json<AutoConfig>) -> Response {
    match save_auto(&c) {
        Ok(_) => Json(json!({"ok": true, "auto": c})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Resolve the on-disk source for a catalog key ("{folder_id}/{rel}", or a
/// legacy storage-root-relative path).
fn resolve_lib_source(lp: &str, store: &Store) -> Option<PathBuf> {
    if let Some((fid, rel)) = lp.split_once('/') {
        let cfg = load_lib_cfg();
        if let Some(f) = cfg.folders.iter().find(|f| f.id == fid) {
            let p = PathBuf::from(f.path.clone()).join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let root = store.lock().unwrap().storage_root().to_string();
    let p = PathBuf::from(root).join(lp);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Concrete HW encoder for `family` on `node` satisfying the `accel`
/// constraint — a specific backend ("nvenc", "videotoolbox", "vaapi", …) or
/// a device class ("gpu"/"cpu"). `None` constraint = best HW encoder for the
/// family overall. Returns `None` when no HW encoder fits.
/// Accel devices the node is ALLOWED to use as a worker (dashboard gear
/// setting `enabledBackends`; `None` = never configured = everything enabled,
/// `Some([])` = no HW workers at all).
fn usable_accels(node: &Node) -> Vec<Accel> {
    match &node.enabled_backends {
        None => node.accels.clone(),
        Some(list) => node
            .accels
            .iter()
            .filter(|a| list.iter().any(|b| *b == a.backend))
            .cloned()
            .collect(),
    }
}

/// Flat HW-encoder list restricted to the node's enabled backends.
fn usable_hw_encoders(node: &Node) -> Vec<String> {
    let mut v = Vec::new();
    for a in usable_accels(node) {
        for e in a.encoders {
            if !v.contains(&e) {
                v.push(e);
            }
        }
    }
    v
}

fn node_accel_encoder(node: &Node, family: &str, accel: Option<&str>) -> Option<String> {
    let accel = accel.map(|a| a.to_lowercase());
    if let Some(a) = accel {
        let is_class = a == "gpu" || a == "cpu";
        for acc in &usable_accels(node) {
            let ok = if is_class {
                acc.kind == a
            } else if a == "vt" {
                acc.backend.contains("video") || acc.backend == "vt"
            } else {
                acc.backend == a
            };
            if ok {
                if let Some(enc) = best_hw_encoder(&acc.encoders, family) {
                    return Some(enc);
                }
            }
        }
        return None;
    }
    best_hw_encoder(&usable_hw_encoders(node), family)
}

/// Match one choice against a node's accels and return the concrete encoder
/// AND the physical device it lives on. When the same backend exists on
/// several devices (a two-GPU box: nvenc on GPU #0 AND GPU #1), the device
/// with the FEWEST active jobs wins — `dev_load` maps device index -> count
/// of this node's queued/dispatched/running jobs already pinned there.
fn node_accel_encoder_dev(
    node: &Node,
    family: &str,
    accel: Option<&str>,
    dev_load: &std::collections::HashMap<u32, u32>,
) -> Option<(String, u32)> {
    let accel = accel.map(|a| a.to_lowercase());
    if let Some(a) = accel {
        let is_class = a == "gpu" || a == "cpu";
        let mut best: Option<(u32, (u32, String))> = None; // (load, (device, enc))
        for acc in &usable_accels(node) {
            let ok = if is_class {
                acc.kind == a
            } else if a == "vt" {
                acc.backend.contains("video") || acc.backend == "vt"
            } else {
                acc.backend == a
            };
            if !ok {
                continue;
            }
            if let Some(enc) = best_hw_encoder(&acc.encoders, family) {
                let load = dev_load.get(&acc.device).copied().unwrap_or(0);
                match &best {
                    None => best = Some((load, (acc.device, enc))),
                    Some((bl, b)) => {
                        if load < *bl || (load == *bl && acc.device < b.0) {
                            best = Some((load, (acc.device, enc)));
                        }
                    }
                }
            }
        }
        return best.map(|(_, (d, e))| (e, d));
    }
    // No accel constraint: any device with a working encoder for the family.
    let mut best: Option<(u32, (u32, String))> = None;
    for acc in &usable_accels(node) {
        if let Some(enc) = best_hw_encoder(&acc.encoders, family) {
            let load = dev_load.get(&acc.device).copied().unwrap_or(0);
            match &best {
                None => best = Some((load, (acc.device, enc))),
                Some((bl, b)) => {
                    if load < *bl || (load == *bl && acc.device < b.0) {
                        best = Some((load, (acc.device, enc)));
                    }
                }
            }
        }
    }
    best.map(|(_, (d, e))| (e, d))
}

/// For a known concrete encoder (legacy single-codec dispatch), pick the
/// least-loaded device that can run it. None when the encoder is software.
fn pick_device_for_encoder(
    node: &Node,
    encoder: &str,
    dev_load: &std::collections::HashMap<u32, u32>,
) -> Option<u32> {
    if !is_hw_encoder(encoder) {
        return None;
    }
    let mut best: Option<(u32, u32)> = None; // (load, device)
    for acc in &usable_accels(node) {
        if acc.encoders.iter().any(|e| e == encoder) {
            let load = dev_load.get(&acc.device).copied().unwrap_or(0);
            match &best {
                None => best = Some((load, acc.device)),
                Some((bl, bd)) => {
                    if load < *bl || (load == *bl && acc.device < *bd) {
                        best = Some((load, acc.device));
                    }
                }
            }
        }
    }
    best.map(|(_, d)| d)
}

/// Active jobs on ONE node, grouped by the pinned GPU device — the load map
/// for multi-GPU spreading on that box.
fn active_jobs_by_node_dev(
    store: &Store,
    prefix: Option<&str>,
    node_id: &str,
) -> std::collections::HashMap<u32, u32> {
    let s = store.lock().unwrap();
    let mut m: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for j in s.jobs.values() {
        if matches!(j.status.as_str(), "queued" | "dispatched" | "running")
            && j.node_id.as_deref() == Some(node_id)
        {
            if let Some(p) = prefix {
                match &j.lib_path {
                    Some(lp) if lp.starts_with(p) => {}
                    _ => continue,
                }
            }
            if let Some(d) = j.gpu_device {
                *m.entry(d).or_insert(0) += 1;
            }
        }
    }
    m
}

/// Walk an encoder-preference list on ONE node: the first choice the node can
/// run (HW) wins. Returns (choice index, concrete encoder, accelerated,
/// pinned GPU device — None for software). When no choice is HW-satisfiable
/// the first choice runs on a software encoder — unless this node has
/// software fallback disabled, in which case None (the node is not eligible
/// for this job at all).
fn resolve_choices_on_node(
    node: &Node,
    choices: &[TargetChoice],
    dev_load: &std::collections::HashMap<u32, u32>,
) -> Option<(usize, String, bool, Option<u32>)> {
    if choices.is_empty() {
        return (if node.allow_software {
            Some((0, "libx264".into(), false, None))
        } else {
            None
        });
    }
    for (i, ch) in choices.iter().enumerate() {
        let fam = codec_family(ch.video_codec.as_deref().unwrap_or("h264"));
        if let Some((enc, dev)) = node_accel_encoder_dev(node, &fam, ch.accel.as_deref(), dev_load) {
            return Some((i, enc, true, Some(dev)));
        }
    }
    if !node.allow_software {
        return None;
    }
    let fam = codec_family(choices[0].video_codec.as_deref().unwrap_or("h264"));
    let sw = family_default_encoder(&fam)
        .map(|s| s.to_string())
        .unwrap_or_else(|| choices[0].video_codec.clone().unwrap_or_else(|| "libx264".into()));
    Some((0, sw, false, None))
}

/// Active (queued/dispatched/running) jobs per node id — used to spread a
/// batch of files over the capable nodes. When `prefix` is given (a folder
/// key), only that folder's jobs count — one folder's batch must not crowd
/// the others off a node.
fn active_jobs_by_node(store: &Store, prefix: Option<&str>) -> std::collections::HashMap<String, u32> {
    let s = store.lock().unwrap();
    let mut m: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for j in s.jobs.values() {
        if matches!(j.status.as_str(), "queued" | "dispatched" | "running") {
            if let Some(nid) = &j.node_id {
                if let Some(p) = prefix {
                    match &j.lib_path {
                        Some(lp) if lp.starts_with(p) => {}
                        _ => continue,
                    }
                }
                *m.entry(nid.clone()).or_insert(0) += 1;
            }
        }
    }
    m
}

/// Resolve node + concrete encoder + effective target params for a target
/// carrying an encoder-preference list. Among ONLINE nodes able to run at
/// least one choice (HW), the LEAST BUSY one wins (load counts only THIS
/// folder's jobs), ties going to the node that fits the EARLIEST choice —
/// so a batch spreads over every capable node while the primary codec stays
/// primary. When no node can run any choice in HW, the least-busy online
/// node takes the first choice on software.
async fn resolve_choice_dispatch(
    store: &Store,
    t: &TargetCfg,
    folder_key: Option<&str>,
    hw_only: bool,
) -> Option<(String, String, String, bool, Option<u32>, TargetCfg)> {
    let c = store.lock().unwrap().redis();
    let online = list_nodes_redis(c, "online", None, None).await;
    if online.is_empty() {
        return None;
    }
    // cap[i] = index of the earliest choice node i can run (HW).
    let caps: Vec<Option<usize>> = online
        .iter()
        .map(|n| {
            t.choices.iter().position(|ch| {
                let fam = codec_family(ch.video_codec.as_deref().unwrap_or("h264"));
                node_accel_encoder(n, &fam, ch.accel.as_deref()).is_some()
            })
        })
        .collect();
    let prefix = folder_key.map(|k| format!("{k}/"));
    let counts = active_jobs_by_node(store, prefix.as_deref());
    // Node CAPACITY counts ALL folders (a node running at its max is full,
    // whatever the folder is); the balance load (counts) is folder-scoped.
    let overall = active_jobs_by_node(store, None);
    let free = |n: &Node| overall.get(&n.id).copied().unwrap_or(0) < n.max_jobs;
    let hw_idx: Box<[usize]> = (0..online.len())
        .filter(|i| caps[*i].is_some() && free(&online[*i]))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let sw_idx: Box<[usize]> = (0..online.len())
        .filter(|i| online[*i].allow_software && free(&online[*i]))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    // No HW fit on a free node: software fallback on a free node that
    // allows it — unless this encode is HW-only, in which case the file
    // is never sent to a node that can't do it in hardware (it waits for
    // a capable node; the dispatch layer turns that into a visible error
    // rather than a silent SW encode).
    let pool: &[usize] = if !hw_idx.is_empty() {
        &hw_idx
    } else if hw_only {
        return None;
    } else if !sw_idx.is_empty() {
        &sw_idx
    } else {
        return None;
    };
    // (load, earliest-choice-index) — lowest wins; list order breaks the rest.
    let mut best = pool[0];
    for &i in &pool[1..] {
        let cur = (
            counts.get(&online[i].id).copied().unwrap_or(0),
            caps[i].unwrap_or(usize::MAX),
        );
        let bst = (
            counts.get(&online[best].id).copied().unwrap_or(0),
            caps[best].unwrap_or(usize::MAX),
        );
        if cur < bst {
            best = i;
        }
    }
    let node = online[best].clone();
    // Multi-GPU: pin the encode to the LEAST LOADED physical device on this
    // node (two nvenc GPUs -> job 1 on #0, job 2 on #1, job 3 back on #0…).
    let prefix_dev = folder_key.map(|k| format!("{k}/"));
    let dev_load = active_jobs_by_node_dev(store, prefix_dev.as_deref(), &node.id);
    let (ci, effective, accelerated, gpu_device) =
        match resolve_choices_on_node(&node, &t.choices, &dev_load) {
            Some(r) => r,
            None => return None,
        };
    let ch = &t.choices[ci];
    // Choice fields override target-level defaults (container/mode/crf are
    // usually set once at the target; per-choice codec+accel differ).
    let eff = TargetCfg {
        video_codec: ch.video_codec.clone().or_else(|| t.video_codec.clone()),
        audio_codec: ch.audio_codec.clone().or_else(|| t.audio_codec.clone()),
        container: ch.container.clone().or_else(|| t.container.clone()),
        mode: ch.mode.clone().or_else(|| t.mode.clone()),
        crf: ch.crf.or(t.crf),
        bitrate_pct: ch.bitrate_pct.or(t.bitrate_pct),
        choices: Vec::new(),
        verify_output: t.verify_output,
        replace_original: t.replace_original,
    };
    let node_name = node.display().to_string();
    Some((node.id, node_name, effective, accelerated, gpu_device, eff))
}

/// Queue one library entry for re-encoding on a node with the given target
/// (folder default or a logic block's). The node pulls the source via
/// node_fetch_file; the finished file is moved back next to the source
/// (node_result), then the post-encode plan runs (verify/replace/…).
///
/// Ok(true)  = job queued
/// Ok(false) = nothing can take it right now (no free node / all busy) —
///             the entry stays pending, a rescan retries it
/// Err(msg)  = a semantic problem the user should see (e.g. hwOnly encode
///             but no online node has a hardware encoder for it)
async fn dispatch_lib_job(
    store: &Store,
    e: &LibraryEntry,
    t: &TargetCfg,
    post: &[PostOp],
    hw_only: bool,
) -> Result<bool, String> {
    let key = e.key();
    if resolve_lib_source(&key, store).is_none() {
        return Ok(false);
    }
    // Resolve (node, concrete encoder, effective target params).
    // Encoder-preference list -> capability-based least-busy node; single
    // codec -> legacy first-node-with-that-family.
    let eff_owned: Option<TargetCfg>;
    let (node_id, node_name, effective, accelerated, gpu_device, eff): (String, String, String, bool, Option<u32>, &TargetCfg) =
        if t.choices.is_empty() {
            let vcodec = t
                .video_codec
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "h264".into());
            let family = codec_family(&vcodec);
            let node = {
                let c = store.lock().unwrap().redis();
                let online = list_nodes_redis(c, "online", None, None).await;
                let counts = active_jobs_by_node(store, None);
                let idx = if hw_only {
                    // HW-only: pick a FREE node that has the family in
                    // hardware. Nothing capable online at all -> error;
                    // capable but busy -> pending.
                    let any_capable = online
                        .iter()
                        .any(|n| usable_hw_encoders(n).iter().any(|h| codec_family(h) == family));
                    if !any_capable {
                        return Err("HW only: no online node has a hardware encoder for this codec".into());
                    }
                    online.iter().position(|n| {
                        counts.get(&n.id).copied().unwrap_or(0) < n.max_jobs
                            && usable_hw_encoders(n).iter().any(|h| codec_family(h) == family)
                    })
                } else {
                    pick_node_idx(&online, &counts, &family)
                };
                match idx {
                    Some(i) => online[i].clone(),
                    None => return Ok(false),
                }
            };
            let (effective, accelerated) = resolve_video_codec(&vcodec, &usable_hw_encoders(&node));
            if hw_only && !accelerated {
                return Err("HW only: no hardware encoder available for this codec".into());
            }
            let fk = e.folder.as_deref().map(|k| format!("{k}/"));
            let dev_load = active_jobs_by_node_dev(store, fk.as_deref(), &node.id);
            let gpu_device = pick_device_for_encoder(&node, &effective, &dev_load);
            eff_owned = None;
            let node_name = node.display().to_string();
            (node.id, node_name, effective, accelerated, gpu_device, t)
        } else {
            // hwOnly + no online node with HW for ANY choice -> a visible
            // error (a capable-but-busy pool still waits: Ok(false)).
            if hw_only {
                let c = store.lock().unwrap().redis();
                let online = list_nodes_redis(c, "online", None, None).await;
                let any_hw = online.iter().any(|n| {
                    t.choices.iter().any(|ch| {
                        let fam = codec_family(ch.video_codec.as_deref().unwrap_or("h264"));
                        node_accel_encoder(n, &fam, ch.accel.as_deref()).is_some()
                    })
                });
                if !any_hw {
                    return Err("HW only: no online node has a hardware encoder for this target".into());
                }
            }
            let (node_id, node_name, effective, accelerated, gpu_device, eff) =
                match resolve_choice_dispatch(store, t, e.folder.as_deref(), hw_only).await {
                    Some(r) => r,
                    None => return Ok(false),
                };
            eff_owned = Some(eff);
            (
                node_id,
                node_name,
                effective,
                accelerated,
                gpu_device,
                eff_owned.as_ref().unwrap(),
            )
        };
    let vcodec = eff
        .video_codec
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "h264".into());
    let acodec = eff
        .audio_codec
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "aac".into());
    let container = eff
        .container
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mp4".into());
    let out_name = format!(
        "{}_{}_{}.{}",
        safe_base(&e.path),
        effective.replace("lib", "").replace('-', "_"),
        container,
        container
    );
    let pct_mode = eff.mode.as_deref().unwrap_or("pct") == "pct";
    let (crf, video_bitrate, bitrate_pct) = if pct_mode {
        let pct = eff.bitrate_pct.unwrap_or(70).clamp(10, 100);
        (None, target_kbps(e.video_bit_rate, Some(pct)), Some(pct))
    } else {
        (eff.crf, None, None)
    };
    let job = Job {
        id: uuid::Uuid::new_v4().to_string(),
        input_file: key.clone(),
        output_file: out_name,
        video_codec: vcodec,
        effective_video_codec: effective,
        audio_codec: acodec,
        container,
        crf,
        video_bitrate,
        bitrate_pct,
        source_bit_rate: e.video_bit_rate,
        status: "queued".into(),
        progress: 0.0,
        created_at: now_ms(),
        started_at: None,
        finished_at: None,
        error: None,
        input_size: Some(e.size),
        output_size: None,
        node_id: Some(node_id),
        executed_by: node_name,
        accelerated,
        lib_path: Some(key),
        gpu_device,
        post: post.to_vec(),
    };
    store.lock().unwrap().insert(job);
    Ok(true)
}

/// Does a block condition match a newly found library entry?
fn cond_matches(c: &Condition, e: &LibraryEntry, online: &[Node]) -> bool {
    let size_mb = e.size as f64 / 1_048_576.0;
    let br_mbps = e.video_bit_rate.map(|b| b as f64 / 1_000_000.0);
    match c.kind.as_str() {
        "always" => true,
        "srcCodec" => e.video_codec.as_deref() == c.codec.as_deref(),
        "sizeGt" => size_mb > c.value.unwrap_or(f64::INFINITY),
        "sizeLt" => size_mb < c.value.unwrap_or(f64::INFINITY),
        "bitrateGt" => br_mbps.unwrap_or(f64::NEG_INFINITY) > c.value.unwrap_or(f64::INFINITY),
        "bitrateLt" => br_mbps.unwrap_or(f64::INFINITY) < c.value.unwrap_or(f64::INFINITY),
        "nodeHasHw" => online.iter().any(|n| {
            let fam = c.hw_family.as_deref().unwrap_or("");
            let cls = c.accel_class.as_deref();
            usable_accels(n).iter().any(|a| {
                (cls.is_none() || a.kind == cls.unwrap_or(""))
                    && a.encoders.iter().any(|e| codec_family(e) == fam)
            })
        }),
        "nameContains" => {
            let pat = c.pattern.as_deref().unwrap_or("").to_lowercase();
            !pat.is_empty() && e.name.to_lowercase().contains(&pat)
        }
        _ => false,
    }
}

/// Evaluate the node graph for one file: from every `start` node follow the
/// edges — a condition's ✓ port is taken when the condition matches (a
/// disabled condition acts as an open gate), ✗ when it doesn't — and the
/// FIRST `action` node reached wins. The encode action's OUTPUT port is
/// then walked for the post-encode chain (verify → replace → … → requeue).
/// Returns (target, do_encode, post_plan, hw_only).
/// None = the file reached no action (caller falls back to the folder
/// default).
fn eval_graph(
    g: &WorkflowGraph,
    e: &LibraryEntry,
    online: &[Node],
    folder_target: &TargetCfg,
) -> Option<(TargetCfg, bool, Vec<PostOp>, bool)> {
    use std::collections::HashSet;
    if g.nodes.is_empty() {
        return None;
    }
    let idx: std::collections::HashMap<&str, usize> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    // out[i] = (child, port) — deduped; children sorted by node order so
    // the DFS (and therefore "first action wins") is deterministic.
    let mut out: Vec<Vec<(usize, Option<String>)>> = vec![Vec::new(); g.nodes.len()];
    for ed in &g.edges {
        if let (Some(&a), Some(&b)) = (idx.get(ed.from.as_str()), idx.get(ed.to.as_str())) {
            if a == b {
                continue;
            }
            let p = ed.from_port.clone();
            if !out[a].iter().any(|(c, q)| *c == b && q == &p) {
                out[a].push((b, p));
            }
        }
    }
    for v in out.iter_mut() {
        v.sort_by_key(|&(c, _)| c);
    }
    let mut visited: HashSet<usize> = HashSet::new();

    // Post-encode chain: BFS over the encode's output port. Only enabled
    // action nodes of the post kinds join the plan (in chain order);
    // anything else (another primary action, a condition, the start) ends
    // that branch — the graph stays linear after the encode.
    fn post_chain(
        i: usize,
        g: &WorkflowGraph,
        out: &[Vec<(usize, Option<String>)>],
        visited: &mut HashSet<usize>,
        plan: &mut Vec<PostOp>,
    ) {
        for (j, port) in out[i].iter().cloned() {
            // branch ports don't exist on action nodes — ignore them
            if matches!(port.as_deref(), Some("true") | Some("false")) {
                continue;
            }
            if !visited.insert(j) {
                continue;
            }
            let n = &g.nodes[j];
            if n.kind != "action" || !n.enabled {
                continue;
            }
            let op = match n.action.kind.as_str() {
                "verify" => Some(PostOp::Verify {
                    mode: n.action.verify_mode.clone(),
                    value: n.action.verify_value,
                    on_failure: n.action.verify_on_failure.clone(),
                }),
                "replace" => Some(PostOp::Replace),
                "saveHere" => Some(PostOp::SaveHere),
                "requeue" => Some(PostOp::Requeue),
                _ => None, // another primary action — chain ends here
            };
            if let Some(o) = op {
                plan.push(o);
                post_chain(j, g, out, visited, plan);
            }
        }
    }

    fn rec(
        i: usize,
        g: &WorkflowGraph,
        out: &[Vec<(usize, Option<String>)>],
        visited: &mut HashSet<usize>,
        e: &LibraryEntry,
        online: &[Node],
        folder_target: &TargetCfg,
    ) -> Option<(TargetCfg, bool, Vec<PostOp>, bool)> {
        if !visited.insert(i) {
            return None;
        }
        let n = &g.nodes[i];
        let result: bool = match n.kind.as_str() {
            "start" => true,
            "condition" => !n.enabled || cond_matches(&n.condition, e, online),
            "action" => {
                if !n.enabled {
                    return None;
                }
                return match n.action.kind.as_str() {
                    "encode" => {
                        let t = n.action
                            .target
                            .clone()
                            .unwrap_or_else(|| folder_target.clone());
                        let hw_only = n.action.hw_only;
                        let mut plan = Vec::new();
                        post_chain(i, g, out, visited, &mut plan);
                        Some((t, true, plan, hw_only))
                    }
                    "healthcheck" => {
                        if online.iter().any(|n| n.status == "online") {
                            Some((
                                folder_target.clone(),
                                folder_target.video_codec.is_some(),
                                Vec::new(),
                                false,
                            ))
                        } else {
                            Some((TargetCfg::default(), false, Vec::new(), false))
                        }
                    }
                    _ => Some((TargetCfg::default(), false, Vec::new(), false)), // skip
                };
            }
            _ => false,
        };
        for (j, port) in out[i].iter().cloned() {
            let taken = match port.as_deref() {
                None => true,
                Some("true") => result,
                Some("false") => !result,
                _ => false,
            };
            if taken {
                if let Some(d) = rec(j, g, out, visited, e, online, folder_target) {
                    return Some(d);
                }
            }
        }
        None
    }

    for (i, n) in g.nodes.iter().enumerate() {
        if n.kind == "start" {
            if let Some(d) = rec(i, g, &out, &mut visited, e, online, folder_target) {
                return Some(d);
            }
        }
    }
    None
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

#[derive(Deserialize)]
struct FsQuery {
    path: Option<String>,
}

/// GET /api/fs/children?path=/media — one directory level for the library
/// tree browser. Absolute paths only; `..` segments are rejected; the path
/// must exist and be a directory. Returns parent (for the breadcrumb/up
/// button) + entries sorted dirs-first (files: video flag + size for the
/// tree's "this looks like a video" hint).
pub async fn fs_children(Query(q): Query<FsQuery>) -> Response {
    let raw = q.path.as_deref().unwrap_or("/");
    let p = PathBuf::from(raw);
    if !p.is_absolute() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "path must be absolute"}))).into_response();
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "path contains .."}))).into_response();
    }
    if !p.is_dir() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not a directory"}))).into_response();
    }
    let read = match std::fs::read_dir(&p) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": format!("unreadable: {e}")}))
            )
                .into_response()
        }
    };
    let mut dirs: Vec<serde_json::Value> = Vec::new();
    let mut files: Vec<serde_json::Value> = Vec::new();
    let mut n = 0usize;
    for entry in read.flatten() {
        n += 1;
        if n > 5000 {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // keep hidden/system clutter out of the tree
        }
        let sub = entry.path();
        let sub_str = sub.to_string_lossy().to_string();
        if sub.is_dir() {
            dirs.push(json!({"name": name, "path": sub_str, "kind": "dir"}));
        } else if sub.is_file() {
            let size = std::fs::metadata(&sub).map(|m| m.len()).unwrap_or(0);
            files.push(json!({
                "name": name,
                "path": sub_str,
                "kind": "file",
                "size": size,
                "video": is_video_ext(sub.extension().and_then(|x| x.to_str())),
            }));
        }
    }
    let by_name = |a: &serde_json::Value, b: &serde_json::Value| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    };
    dirs.sort_by(by_name);
    files.sort_by(by_name);
    let parent = p
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .filter(|s| s != "/")
        .or_else(|| Some("/".into()));
    Json(json!({
        "path": sub_str_of(&p),
        "parent": parent,
        "dirs": dirs,
        "files": files,
    }))
    .into_response()
}

fn sub_str_of(p: &std::path::Path) -> String {
    p.to_string_lossy().to_string()
}

/// POST /api/library/scan — {root} for an ad-hoc one-shot folder, or (no
/// root) all registered+enabled folders. Builds the in-memory catalog by
/// ffprobing each video (non-destructive: entries of folders not scanned
/// this time are kept) and auto-queues newly found files per the saved
/// automation (logic blocks first, then the folder's default target).
pub async fn library_scan(State(store): State<Store>, Json(req): Json<ScanReq>) -> Response {
    // Resolve roots: an explicit {root} wins (ad-hoc, no folder id);
    // otherwise all enabled registered folders.
    let libcfg = load_lib_cfg();
    let roots: Vec<(Option<String>, PathBuf)> =
        if let Some(r) = req.root.filter(|r| !r.trim().is_empty()) {
            let r = r.trim().to_string();
            let p = PathBuf::from(r.clone());
            if !p.is_dir() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("not a directory: {r}") })),
                )
                    .into_response();
            }
            vec![(None, p)]
        } else {
            libcfg
                .folders
                .iter()
                .filter(|f| f.enabled)
                .map(|f| (Some(f.id.clone()), PathBuf::from(f.path.clone())))
                .filter(|(_, p)| p.is_dir())
                .collect()
        };
    if roots.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "no folders registered — add a folder first (or pass {\"root\":\"/path/to/videos\"})"
            })),
        )
            .into_response();
    }
    let limit = req.limit.unwrap_or(500).max(1).min(5000);

    // Re-encode outputs land NEXT TO their source inside the folder — a
    // naive rescan would discover them as new videos and re-encode them in
    // an endless cascade. Skip files whose name is a known re-encoded
    // output of the current catalog.
    let out_names: std::collections::HashSet<String> = {
        let mut n: std::collections::HashSet<String> = store
            .lock()
            .unwrap()
            .list_library()
            .into_iter()
            .filter_map(|e| e.reencoded)
            .collect();
        n.extend(libcfg.tombstone.clone());
        n
    };

    // Walk each root (blocking, but cheap) and ffprobe each video.
    let mut entries: Vec<LibraryEntry> = Vec::new();
    for (fid, rootp) in &roots {
        let rels = std::thread::scope(|sc| {
            sc.spawn(|| {
                let mut v: Vec<PathBuf> = Vec::new();
                walk_videos(rootp, rootp, &mut v, limit);
                v
            })
            .join()
            .unwrap_or_default()
        });
        let rels: Vec<PathBuf> = rels
            .into_iter()
            .filter(|rel| {
                let fname = rel.file_name().and_then(|x| x.to_str()).unwrap_or("");
                if out_names.contains(fname) {
                    // A replace-original source KEEPS its own name (the
                    // finished file moved onto the source path) — it is a
                    // tracked entry, not a stray artifact. Never drop it.
                    let key = match fid {
                        Some(f) => format!("{f}/{}", rel.to_string_lossy()),
                        None => rel.to_string_lossy().to_string(),
                    };
                    return store.lock().unwrap().library.contains_key(&key);
                }
                true
            })
            .collect();
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
                container: info
                    .container
                    .clone()
                    .or_else(|| if ext.is_empty() { None } else { Some(ext) }),
                width: info.width,
                height: info.height,
                fps: info.fps,
                video_bit_rate: info.video_bit_rate,
                pix_fmt: info.pix_fmt,
                folder: fid.clone(),
                status: "pending".into(),
                reencoded: None,
                new_size: None,
                error: None,
            });
        }
    }

    // Carry-forward (read-only): a rescan must not reset files already
    // processed. In-flight statuses are only kept when a live non-terminal
    // library job still exists for the file — otherwise the entry goes back
    // to "pending" so it can be re-queued.
    let (carried, old, live) = {
        let s = store.lock().unwrap();
        let scanned_fids: std::collections::HashSet<String> =
            roots.iter().filter_map(|(f, _)| f.clone()).collect();
        let adhoc = roots.iter().any(|(f, _)| f.is_none());
        // Non-destructive catalog: entries of folders that weren't part of
        // this scan stay untouched (as do ad-hoc entries when registered
        // folders are being scanned).
        let carried = s
            .list_library()
            .into_iter()
            .filter(|e| match &e.folder {
                Some(fid) => !scanned_fids.contains(fid),
                None => !adhoc,
            })
            .collect::<Vec<_>>();
        let old = s.list_library();
        let live: std::collections::HashSet<String> = s
            .list()
            .into_iter()
            .filter(|j| j.lib_path.is_some() && !["done", "error"].contains(&j.status.as_str()))
            .map(|j| j.input_file)
            .collect();
        (carried, old, live)
    };
    for e in entries.iter_mut() {
        let key = e.key();
        if let Some(o) = old.iter().find(|o| o.key() == key) {
            let keep = match o.status.as_str() {
                "done" => true,
                "queued" | "running" | "dispatched" => live.contains(&key),
                // "error" (and unknown) fall back to pending so a rescan
                // retries the file.
                _ => false,
            };
            if keep {
                e.status = o.status.clone();
                e.reencoded = o.reencoded.clone();
                e.new_size = o.new_size;
                e.error = o.error.clone();
            }
        }
    }

    // Automation: for each newly found (pending) file, evaluate the logic
    // blocks top-down (first match wins); fall back to the folder's default
    // target (or the legacy auto.json for ad-hoc roots).
    let online: Vec<Node> = {
        let c = store.lock().unwrap().redis();
        list_nodes_redis(c, "online", None, None).await
    };
    let has_blocks = libcfg.blocks.iter().any(|b| b.enabled);
    // Effective automation graph: the saved graph if it has real nodes,
    // else the legacy block list migrated to a linear graph (same meaning).
    let eff_graph: Option<WorkflowGraph> = if libcfg
        .graph
        .nodes
        .iter()
        .any(|n| n.kind == "condition" || n.kind == "action")
    {
        Some(libcfg.graph.clone())
    } else if has_blocks {
        Some(migrate_blocks_to_graph(&libcfg.blocks))
    } else {
        None
    };
    let mut queued: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for e in entries.iter_mut() {
        if e.status != "pending" {
            continue;
        }
        let folder = libcfg
            .folders
            .iter()
            .find(|f| e.folder.as_deref() == Some(f.id.as_str()));
        let folder_target: TargetCfg = folder.map(|f| f.target.clone()).unwrap_or_default();
        // Graph precedence: 1) the folder's chosen NAMED logic (library-tab
        // assignment), 2) legacy per-folder inline graph, 3) global library
        // graph. None of them -> the folder's bulk target below.
        let logic_graph: Option<&WorkflowGraph> = folder
            .and_then(|f| f.logic_id.as_deref())
            .and_then(|lid| libcfg.logics.iter().find(|l| l.id == lid))
            .map(|l| &l.graph);
        let folder_graph: Option<&WorkflowGraph> = folder
            .filter(|f| {
                f.graph
                    .nodes
                    .iter()
                    .any(|n| n.kind == "condition" || n.kind == "action")
            })
            .map(|f| &f.graph);
        let decision: Option<(TargetCfg, bool, Vec<PostOp>, bool)> = {
            let g = logic_graph.or(folder_graph).or(eff_graph.as_ref());
            if let Some(g) = g {
                // Walk the node graph; first action reached wins, its
                // output port carries the post-encode plan.
                eval_graph(g, e, &online, &folder_target).or_else(|| {
                    Some((
                        folder_target.clone(),
                        folder_target.video_codec.is_some(),
                        Vec::new(),
                        false,
                    ))
                })
            } else if e.folder.is_some() {
                let has_vc = folder_target.video_codec.is_some();
                Some((folder_target, has_vc, Vec::new(), false))
            } else {
                let ac = load_auto();
                if ac.enabled {
                    Some((ac.into(), true, Vec::new(), false))
                } else {
                    None
                }
            }
        };
        if let Some((t, do_encode, mut post, hw_only)) = decision {
            if do_encode {
                // Legacy target flags (folder-row checkboxes / old graphs)
                // still mean the same thing when no post nodes are chained.
                if post.is_empty() {
                    if t.verify_output {
                        post.push(PostOp::Verify {
                            mode: None,
                            value: None,
                            on_failure: None,
                        });
                    }
                    if t.replace_original {
                        post.push(PostOp::Replace);
                    }
                }
                // Never create a second job for a file that already has a
                // live (queued/dispatched/running) one — requeue+scan races
                // would otherwise double-encode the same file.
                if live.contains(&e.key()) {
                    e.status = "queued".into();
                    queued.push(e.key());
                    continue;
                }
                match dispatch_lib_job(&store, e, &t, &post, hw_only).await {
                    Ok(true) => {
                        e.status = "queued".into();
                        queued.push(e.key());
                    }
                    Ok(false) => {
                        // stays pending — a later scan retries it
                    }
                    Err(msg) => {
                        e.status = "error".into();
                        e.error = Some(msg);
                    }
                }
            } else {
                e.status = "skipped".into();
                skipped.push(e.key());
            }
        }
    }
    let all = {
        let mut v = entries.clone();
        v.extend(carried);
        let mut s = store.lock().unwrap();
        s.set_library(v.clone());
        v
    };
    persist_catalog(&store);
    Json(json!({
        "count": all.len(),
        "queued": queued,
        "skipped": skipped,
        "entries": all,
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
        lib_path: None,
        gpu_device: None,
        post: Vec::new(),
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
    // idempotent re-register by name — the id is looked up in Redis, so a
    // node keeps its identity across server (and its own) restarts.
    let mut c = store.lock().unwrap().redis();
    let already: Option<String> = redis::cmd("GET")
        .arg(rk_name(&req.name))
        .query_async(&mut c)
        .await
        .ok()
        .flatten();
    let node = reg_node(c, &req.name, &ffmpeg_version, &haccels, &hw_encoders, &accels).await;
    Json(json!({"nodeId": node.id, "new": already.is_none()})).into_response()
}

/// POST /api/nodes/:id/heartbeat — liveness (+ optional live stats). The
/// body is optional: older node binaries POST without one.
pub async fn node_heartbeat(
    State(store): State<Store>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<HeartbeatStats>>,
) -> Response {
    let stats = body.map(|b| b.0).unwrap_or_default();
    let c = store.lock().unwrap().redis();
    match beat_node(c, &id, &stats).await {
        Some(n) => match serde_json::to_value(n) {
            Ok(v) => Json(json!({"ok": true, "node": v})).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "unknown node"}))).into_response(),
    }
}

/// PATCH /api/nodes/:id — worker settings from the dashboard gear:
/// display name, which accel backends the node may be used for, software
/// fallback toggle, and max concurrent jobs.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct NodeConfigBody {
    /// New display name (JSON key `name`). `""` clears the override (back to
    /// the registration name); absent leaves it unchanged.
    #[serde(alias = "displayName")]
    pub name: Option<String>,
    /// Backends allowed as workers (empty = all). Absent = unchanged.
    pub enabled_backends: Option<Vec<String>>,
    /// Absent = unchanged.
    pub allow_software: Option<bool>,
    /// Absent = unchanged. Clamped 1..=16.
    pub max_jobs: Option<u32>,
}

pub async fn node_config_patch(
    State(store): State<Store>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<NodeConfigBody>,
) -> Response {
    let mut c = store.lock().unwrap().redis();
    // Read current so we can validate + return the merged node.
    let before: NodeHash = match redis::cmd("HGETALL")
        .arg(rk_node(&id))
        .query_async(&mut c)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
                .into_response();
        }
    };
    if before.get("name").is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown node"}))).into_response();
    }
    let mut pipe = redis::pipe();
    if let Some(name) = &body.name {
        if name.is_empty() {
            pipe.cmd("HDEL").arg(rk_node(&id)).arg("displayName");
        } else {
            pipe.cmd("HSET").arg(rk_node(&id)).arg("displayName").arg(name.as_str());
        }
    }
    if let Some(bk) = &body.enabled_backends {
        // Ignore the backend names the node doesn't actually have.
        let valid: Vec<String> = {
            let accels: Vec<Accel> = serde_json::from_str(before.get("accels").map(|s| s.as_str()).unwrap_or("[]")).unwrap_or_default();
            let have: std::collections::HashSet<&str> = accels.iter().map(|a| a.backend.as_str()).collect();
            bk.iter().filter(|b| have.contains(b.as_str())).cloned().collect()
        };
        pipe.cmd("HSET")
            .arg(rk_node(&id))
            .arg("enabledBackends")
            .arg(serde_json::to_string(&valid).unwrap_or_else(|_| "[]".into()));
    }
    if let Some(sw) = body.allow_software {
        pipe.cmd("HSET").arg(rk_node(&id)).arg("allowSoftware").arg(if sw { "true" } else { "false" });
    }
    if let Some(m) = body.max_jobs {
        pipe.cmd("HSET")
            .arg(rk_node(&id))
            .arg("maxJobs")
            .arg(m.clamp(1, 16).to_string());
    }
    let _: () = match pipe.query_async(&mut c).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
                .into_response();
        }
    };
    match hgetall_node(c, &id).await {
        Some(n) => Json(json!({"ok": true, "node": serde_json::to_value(n).unwrap_or_default()}))
            .into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "unknown node"}))).into_response(),
    }
}

/// GET /api/nodes — registry (Redis) with live status + acceleration
/// capabilities. Filters: status=online|offline|all (default online),
/// backend=nvenc|qsv|... (has that HW accel), family=h264|av1|... (has an HW
/// encoder for that codec family).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct NodeFilter {
    status: Option<String>,
    backend: Option<String>,
    family: Option<String>,
}

pub async fn list_nodes(State(store): State<Store>, Query(f): Query<NodeFilter>) -> impl IntoResponse {
    let c = store.lock().unwrap().redis();
    let status = f.status.as_deref().unwrap_or("online");
    let v: Vec<Value> = list_nodes_redis(c, status, f.backend.as_deref(), f.family.as_deref())
        .await
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

    // 3. Upsert into the Redis registry (idempotent by name).
    let c = store.lock().unwrap().redis();
    let node = reg_node(c, &name, &ffmpeg_version, &haccels, &hw_encoders, &accels).await;

    Json(serde_json::to_value(node).unwrap_or(Value::Null)).into_response()
}

/// GET /api/nodes/:id — one node.
pub async fn get_node(State(store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    let c = store.lock().unwrap().redis();
    match hgetall_node(c, &id).await {
        Some(n) => match serde_json::to_value(n) {
            Ok(v) => Json(v).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "node not found"}))).into_response(),
    }
}

/// GET /api/nodes/:id/poll — long-poll (up to 20s) for the next dispatched
/// job. The response also carries the node's worker settings so the node can
/// adapt (e.g. raise/lower its concurrent-job pool).
pub async fn node_poll(State(store): State<Store>, UrlPath(id): UrlPath<String>) -> Response {
    let node = {
        let c = store.lock().unwrap().redis();
        match hgetall_node(c, &id).await {
            Some(n) => n,
            None => return (StatusCode::NOT_FOUND, Json(json!({"error": "unknown node"}))).into_response(),
        }
    };
    let cfg = json!({
        "maxJobs": node.max_jobs,
        "enabledBackends": node.enabled_backends.clone(),
        "allowSoftware": node.allow_software,
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let claimed = store.lock().unwrap().claim_for_node(&id);
        if let Some(job) = claimed {
            let c = store.lock().unwrap().redis();
            let _ = set_current_job(c, &id, Some(&job.id)).await;
            return match serde_json::to_value(job) {
                Ok(v) => Json(json!({"job": v, "config": cfg})).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
            };
        }
        if std::time::Instant::now() >= deadline {
            return Json(json!({"job": null, "config": cfg})).into_response();
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
    let req_error = req.error.clone();
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
    }
    if req.status == "done" || req.status == "error" {
        let c = store.lock().unwrap().redis();
        let _ = set_current_job(c, &id, None).await;
    }
    // Library-automation job failed → mark the entry so a rescan re-queues it.
    if req.status == "error" {
        let lp = store.lock().unwrap().jobs.get(&req.job_id).and_then(|j| j.lib_path.clone());
        if let Some(lp) = lp {
            {
                let mut s = store.lock().unwrap();
                s.update_library_entry(&lp, |e| {
                    e.status = "error".into();
                    e.error = req_error;
                });
            }
            // (lock dropped — persist_catalog re-acquires it)
            persist_catalog(&store);
        }
    }
    Json(json!({"ok": true})).into_response()
}

/// GET /api/nodes/:id/file/:name (or /*name for library subpaths) — a node
/// fetches an input file from the server. Uploads first; library jobs pull
/// straight from the storage root (name is then a relative path with slashes).
pub async fn node_fetch_file(State(store): State<Store>, UrlPath((_id, name)): UrlPath<(String, String)>) -> Response {
    let _ = _id;
    if name.is_empty() || name.contains("..") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad filename"}))).into_response();
    }
    let path = {
        // 1) uploads (manual test flow)
        let up = uploads_dir().join(&name);
        if up.is_file() {
            Some(up)
        } else {
            // 2) catalog key "{folder_id}/{rel}" -> registered folder
            name.split_once('/')
                .and_then(|(fid, rel)| {
                    load_lib_cfg()
                        .folders
                        .iter()
                        .find(|f| f.id == fid)
                        .map(|f| PathBuf::from(f.path.clone()).join(rel))
                })
                .filter(|p| p.is_file())
                .or_else(|| {
                    // 3) legacy: relative to the storage root
                    let root = store.lock().unwrap().storage_root().to_string();
                    let rp = PathBuf::from(root).join(&name);
                    rp.is_file().then_some(rp)
                })
        }
    };
    match path {
        Some(p) => match std::fs::read(&p) {
            Ok(b) => (StatusCode::OK, [("content-type", "application/octet-stream")], b).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        },
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
    }
}

/// Wait until a file's on-disk size equals `expected`. This host shows
/// write-size metadata lag after buffered writes (write() returns Ok before
/// the size catches up — reproducible with `examples/fstest`), so stat once
/// more for up to ~2s after rename/copy before trusting the size.
fn wait_file_size(path: &std::path::Path, expected: u64) -> u64 {
    for _ in 0..40 {
        if let Some(len) = std::fs::metadata(path).map(|m| m.len()).ok() {
            if len == expected {
                return len;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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
                    return (StatusCode::BAD_REQUEST, Json(json!({ "error": "failed to store file" }))).into_response();
                }
                // A torn connection can deliver a file part with an empty
                // body (clean EOF at the part boundary) — never accept a
                // 0-byte output: it would be "moved next to the source" and
                // marked done with a corrupt artifact.
                if tmp_size == 0 {
                    let _ = std::fs::remove_file(&tmp_path);
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "uploaded file is empty (torn upload)" }))).into_response();
                }
                // This host shows write-size metadata lag after buffered
                // writes (write() returns Ok before the on-disk size
                // catches up — reproducible with examples/fstest). Force the
                // size to be committed before we rename + stat the file.
                if let Err(e) = file.sync_all().await {
                    eprintln!("[node_result] sync {id}: {e}");
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
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("write {e}") }))).into_response();
    }
    // Verify the on-disk size actually reached tmp_size before trusting the
    // artifact (write-size metadata lag on this host — see wait_file_size).
    let sz_on_disk = wait_file_size(&path, tmp_size);
    if sz_on_disk != tmp_size {
        eprintln!("[node_result] VERIFY FAIL {out_name}: expected {tmp_size} on-disk={sz_on_disk} job={job_id}");
        let _ = std::fs::remove_file(&path);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "uploaded file size mismatch (corrupt upload)" }))).into_response();
    }
    let size = tmp_size;
    // Library-automation jobs land next to the source file, not in outputs/.
    let lp = {
        let mut s = store.lock().unwrap();
        let lp = s.jobs.get(&job_id).and_then(|j| j.lib_path.clone());
        if let Some(j) = s.jobs.get_mut(&job_id) {
            j.output_file = out_name.clone();
            j.output_size = Some(size);
        }
        lp
    };
    if let Some(lp) = &lp {
        if !lp.is_empty() {
            // Resolve the real source from the catalog (registered
            // folder, or legacy storage root).
            if let Some(src) = resolve_lib_source(lp, &store) {
                // Per-job post-encode plan (the ops chained on the encode
                // node's output port; legacy target flags were folded in
                // at dispatch time).
                let (post_plan, src_dur) = {
                    let s = store.lock().unwrap();
                    let jp = s.jobs.get(&job_id).map(|j| j.post.clone()).unwrap_or_default();
                    let sd = s.library.get(lp).and_then(|e| e.duration);
                    (jp, sd)
                };
                let src_name = src.file_name().and_then(|x| x.to_str()).unwrap_or("").to_string();
                let fail = |msg: String, lp: &str, job_id: &str, store: &Store| -> Response {
                    {
                        let mut s = store.lock().unwrap();
                        s.update_library_entry(lp, |e| {
                            e.status = "error".into();
                            e.error = Some(msg.clone());
                            e.reencoded = None;
                            e.new_size = None;
                        });
                        if let Some(j) = s.jobs.get_mut(job_id) {
                            j.status = "error".into();
                            j.error = Some(msg.clone());
                            j.output_size = None;
                        }
                    }
                    persist_catalog(store);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": msg })),
                    )
                        .into_response()
                };
                // Stage next to the source (never on top of it) so a failed
                // health check can't corrupt the original.
                let dest = src
                    .parent()
                    .map(|d| d.join(out_name.clone()))
                    .unwrap_or_else(|| PathBuf::from(out_name.clone()));
                if let Some(dir) = dest.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // rename fails across devices (e.g. outputs on /, storage on
                // /tmp or a NAS mount) — fall back to copy + delete.
                let renamed = std::fs::rename(&path, &dest).is_ok();
                let moved = renamed || {
                    let ok = std::fs::copy(&path, &dest).is_ok();
                    if ok {
                        let _ = std::fs::remove_file(&path);
                    }
                    ok
                };
                // The artifact is only "done" when the DEST file's size is
                // verified equal to the uploaded size — never before.
                let dsize = wait_file_size(&dest, size);
                if !moved || dsize != size {
                    eprintln!("[node_result] VERIFY FAIL dest {out_name}: moved={moved} dest_size={dsize} expected={size} job={job_id}");
                    let _ = std::fs::remove_file(&dest);
                    let _ = std::fs::remove_file(&path);
                    return fail(format!("output file failed size verification (got {dsize}, expected {size})"), lp, &job_id, &store);
                }
                let mut replaced = false;
                let mut requeue = false;
                // Execute the post-encode plan IN CHAIN ORDER. A failed
                // health check stops the chain (don't replace a corrupt
                // file with the original) and either errors the entry or
                // sends it back to the queue, per the verify node's
                // on-failure choice.
                for op in &post_plan {
                    match op {
                        PostOp::Verify { mode, value, on_failure } => {
                            // Sampling: "all" checks every file; "pct" a
                            // random subset; "every" every Nth (stable key
                            // hash) — so a 10k-file folder isn't probed
                            // file by file.
                            if !verify_selected(lp, mode.as_deref(), *value) {
                                continue;
                            }
                            // The file must probe as a real video — a video
                            // stream present, positive duration, and at
                            // least half the source's length (a truncated/
                            // corrupt encode shows up here, not after the
                            // user hits play).
                            let info = probe_full(&dest.to_string_lossy()).await;
                            let odur = info.duration.unwrap_or(0.0);
                            let ok_video = info.video_codec.is_some() && odur > 0.0;
                            let ok_dur = match src_dur {
                                Some(sd) if sd > 0.0 => odur >= sd * 0.5,
                                _ => true,
                            };
                            if !ok_video || !ok_dur {
                                let vdbg = info
                                    .video_codec
                                    .clone()
                                    .unwrap_or_else(|| "<none>".into());
                                eprintln!("[node_result] HEALTHCHECK FAIL {out_name}: video={vdbg} out_dur={odur} src_dur={src_dur:?} job={job_id}");
                                let _ = std::fs::remove_file(&dest);
                                if on_failure.as_deref() == Some("requeue") {
                                    // Back to the queue — the next scan
                                    // dispatches a fresh attempt.
                                    let msg = format!(
                                        "health check failed: output video={vdbg} duration={odur}s (source {src_dur:?}s) — requeued"
                                    );
                                    {
                                        let mut s = store.lock().unwrap();
                                        s.update_library_entry(lp, |e| {
                                            e.status = "pending".into();
                                            e.error = Some(msg.clone());
                                            e.reencoded = None;
                                            e.new_size = None;
                                        });
                                        if let Some(j) = s.jobs.get_mut(&job_id) {
                                            j.status = "error".into();
                                            j.error = Some(msg.clone());
                                            j.output_size = None;
                                        }
                                    }
                                    persist_catalog(&store);
                                    return (
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        Json(json!({ "error": "health check failed — requeued" })),
                                    )
                                        .into_response();
                                }
                                return fail(format!("health check failed: output video={vdbg} duration={odur}s (source {src_dur:?}s)"), lp, &job_id, &store);
                            }
                        }
                        PostOp::Replace => {
                            // Replace-the-original: keep the source as
                            // `stem.orig.ext` (overwrites a previous
                            // backup), then move the finished file onto
                            // the source path.
                            let stem = src.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
                            let ext = src.extension().and_then(|x| x.to_str()).unwrap_or("").to_string();
                            let backup = if ext.is_empty() {
                                src.with_file_name(format!("{stem}.orig"))
                            } else {
                                src.with_file_name(format!("{stem}.orig.{ext}"))
                            };
                            let backup_name = backup.file_name().and_then(|x| x.to_str()).unwrap_or("").to_string();
                            let b0 = std::fs::rename(&src, &backup).is_ok();
                            let b1 = {
                                let ok = std::fs::copy(&src, &backup).is_ok();
                                if ok {
                                    let _ = std::fs::remove_file(&src);
                                }
                                ok
                            };
                            if !(b0 || b1) {
                                return fail(format!("could not back up original ({src_name}) before replace"), lp, &job_id, &store);
                            }
                            let r0 = std::fs::rename(&dest, &src).is_ok();
                            let r1 = {
                                let ok = std::fs::copy(&dest, &src).is_ok();
                                if ok {
                                    let _ = std::fs::remove_file(&dest);
                                }
                                ok
                            };
                            if !(r0 || r1) || wait_file_size(&src, size) != size {
                                // Best effort: put the original back where it was.
                                let _ = std::fs::remove_file(&src);
                                let _ = std::fs::rename(&backup, &src);
                                let _ = std::fs::remove_file(&dest);
                                return fail(format!("replace failed while moving output onto {src_name}"), lp, &job_id, &store);
                            }
                            replaced = true;
                            {
                                let mut s = store.lock().unwrap();
                                s.update_library_entry(lp, |e| {
                                    e.status = "done".into();
                                    // The finished file now lives at the SOURCE path.
                                    e.reencoded = Some(src_name.clone());
                                    e.new_size = Some(size);
                                    e.error = None;
                                });
                            }
                            // Tombstone the backup name so rescans never
                            // discover it as a fresh video (tombstones
                            // only ever grow).
                            {
                                let _g = LIB_CFG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
                                let mut c = load_lib_cfg();
                                if !c.tombstone.iter().any(|t| t == &backup_name) {
                                    c.tombstone.push(backup_name);
                                    c.tombstone.sort();
                                }
                                let _ = save_lib_cfg(&c);
                            }
                        }
                        PostOp::SaveHere => {
                            // Library jobs already stage the output next
                            // to the source — the explicit node just
                            // documents the intent on the graph.
                        }
                        PostOp::Requeue => {
                            // After the rest of the plan: back to the
                            // queue (a rescan re-dispatches the entry).
                            requeue = true;
                        }
                    }
                }
                if !replaced {
                    {
                        let mut s = store.lock().unwrap();
                        s.update_library_entry(lp, |e| {
                            e.status = "done".into();
                            e.reencoded = Some(out_name.clone());
                            e.new_size = Some(size);
                            e.error = None;
                        });
                    }
                }
                if requeue {
                    let mut s = store.lock().unwrap();
                    s.update_library_entry(lp, |e| {
                        e.status = "pending".into();
                    });
                }
                // (lock dropped — persist_catalog re-acquires it)
                persist_catalog(&store);
            }
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

    let (node_id, node_name, effective, accelerated, gpu_device) = {
        let c = store.lock().unwrap().redis();
        let online = list_nodes_redis(c, "online", None, None).await;
        let counts = active_jobs_by_node(&store, None);
        let idx = if let Some(nid) = &req.node_id {
            online.iter().position(|n| n.id == *nid)
        } else if req.force_codec.unwrap_or(false) {
            // Forced encoder: must land on a node that actually has it.
            pick_node_encoder_idx(&online, &counts, &vcodec)
        } else {
            pick_node_idx(&online, &counts, &codec_family(&vcodec))
        };
        let node = match idx {
            Some(i) => online[i].clone(),
            None => return (StatusCode::BAD_REQUEST, Json(json!({"error": "no available node"}))).into_response(),
        };
        let (effective, accelerated) = if req.force_codec.unwrap_or(false) {
            (vcodec.clone(), false)
        } else {
            resolve_video_codec(&vcodec, &usable_hw_encoders(&node))
        };
        // Multi-GPU: pin the least-loaded device for the chosen HW encoder.
        let dev_load = active_jobs_by_node_dev(&store, None, &node.id);
        let gpu_device = if accelerated {
            pick_device_for_encoder(&node, &effective, &dev_load)
        } else {
            None
        };
        (node.id.clone(), node.display().to_string(), effective, accelerated, gpu_device)
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
        lib_path: None,
        gpu_device,
        post: Vec::new(),
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

    // Node registry backend: Redis (persistent node identity + liveness).
    let redis_url = std::env::var("CT_REDIS").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let rclient = redis::Client::open(redis_url.clone()).expect("redis client");
    let rconn = loop {
        match rclient.get_multiplexed_tokio_connection().await {
            Ok(c) => break c,
            Err(e) => {
                eprintln!("[CodecTrans] redis {redis_url} unreachable ({e}) — retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };
    println!("[CodecTrans] node registry -> redis {redis_url}");
    let store: Store = Arc::new(Mutex::new(JobStore::new(rconn)));

    // Restore the persisted library catalog (survives restarts — a rescan
    // must not re-queue finished files).
    {
        let mut s = store.lock().unwrap();
        s.set_library(load_catalog());
    }
    println!(
        "[CodecTrans] library catalog: {} entries",
        store.lock().unwrap().list_library().len()
    );

    // mDNS: advertise this server on the LAN + browse for nodes.
    spawn_mdns(&store, port).await;

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/server-stats", get(server_stats))
        .route("/api/codecs", get(codecs))
        .route("/api/library", get(library_get))
        .route("/api/library/scan", post(library_scan))
        .route("/api/fs/children", get(fs_children))
        .route("/api/library/auto", get(auto_get).post(auto_post))
        .route("/api/library/folders", get(folders_get).post(folders_post))
        .route("/api/library/blocks", get(blocks_get).post(blocks_post))
        .route("/api/logic", get(logics_get).post(logics_post))
        .route("/api/logic/:id", axum::routing::delete(logic_delete))
        .route(
            "/api/library/folders/:id/blocks",
            get(folder_blocks_get).post(folder_blocks_post),
        )
        .route("/api/library/requeue", post(library_requeue))
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
        .route("/api/nodes/:id", get(get_node).patch(node_config_patch))
        .route("/api/nodes/:id/heartbeat", post(node_heartbeat))
        .route("/api/nodes/:id/poll", get(node_poll))
        .route("/api/nodes/:id/report", post(node_report))
        .route("/api/nodes/:id/file/*name", get(node_fetch_file))
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

    // 4. Self-heal stuck jobs: a job stuck in "dispatched" for 10+ minutes
    //    (node died during claim/download before ever reporting "running")
    //    goes back to the queue so another node (or the same one, later)
    //    can pick it up instead of the file being stranded forever.
    {
        let store = store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let now = now_ms();
                let mut s = store.lock().unwrap();
                for j in s.jobs.values_mut() {
                    if j.status == "dispatched" && now - j.created_at > 600_000 {
                        j.status = "queued".to_string();
                        j.node_id = None;
                        eprintln!("[CodecTrans] job {} stuck dispatched 10m — requeued", j.id);
                    }
                }
            }
        });
    }
}
