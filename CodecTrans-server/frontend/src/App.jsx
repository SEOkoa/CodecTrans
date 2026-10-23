import React, { useEffect, useRef, useState } from "react";
import { api, fmtSize, fmtDur, fmtRate } from "./api.js";

const STATUS_LABEL = {
  queued: "Queued",
  dispatched: "Dispatched",
  running: "Transcoding…",
  done: "Done",
  error: "Error",
};

// Map an HW encoder name (h264_nvenc, hevc_vaapi, av1_qsv, …) to a short
// family label + a color class, so a node's accelerators render as colored
// chips instead of a raw list.
const CODEC_CHIPS = {
  h264: { label: "h264", cls: "c-h264" },
  h265: { label: "hevc", cls: "c-h265" },
  vp9: { label: "vp9", cls: "c-vp9" },
  vp8: { label: "vp8", cls: "c-vp8" },
  av1: { label: "av1", cls: "c-av1" },
  mpeg4: { label: "mpeg4", cls: "c-mpeg4" },
};
function chipFor(enc) {
  const e = enc.toLowerCase();
  let fam;
  if (e.includes("h264") || e.includes("x264") || e.includes("avc")) fam = "h264";
  else if (e.includes("hevc") || e.includes("h265") || e.includes("x265")) fam = "h265";
  else if (e.includes("vp9")) fam = "vp9";
  else if (e.includes("vp8")) fam = "vp8";
  else if (e.includes("av1")) fam = "av1";
  else if (e.includes("mpeg4")) fam = "mpeg4";
  return (fam && CODEC_CHIPS[fam]) || { label: enc, cls: "c-other" };
}
// fps -> "29.97" style label (trim trailing zeros)
function fmtFps(f) {
  if (f == null || !isFinite(f)) return "";
  const r = Math.round(f * 100) / 100;
  return String(r);
}

// HW encoder name -> codec family (mirrors the server's codec_family).
function codecFamily(enc) {
  const e = (enc || "").toLowerCase();
  if (e.includes("h264") || e.includes("x264") || e.includes("avc")) return "h264";
  if (e.includes("hevc") || e.includes("h265") || e.includes("x265")) return "h265";
  if (e.includes("vp9")) return "vp9";
  if (e.includes("vp8")) return "vp8";
  if (e.includes("av1")) return "av1";
  if (e.includes("mpeg4")) return "mpeg4";
  if (e.includes("mjpeg") || e.includes("jpeg")) return "mjpeg";
  return e.split("_")[0] || enc;
}

function HwChips({ encs }) {
  if (!encs || encs.length === 0) return <span className="cpu-only">CPU only</span>;
  return (
    <span className="chips">
      <span className="chips-label">⚡ HW</span>
      {encs.map((e) => {
        const c = chipFor(e);
        return (
          <span key={e} className={`chip ${c.cls}`} title={e}>
            {c.label}
          </span>
        );
      })}
    </span>
  );
}

// Group a node's accelerator entries by physical device and render
// "GPU #0 · name" / "CPU #0 · name" with per-backend codec chips, so a
// box with a dGPU + iGPU (or several of either) is obvious at a glance.
function AccelPanel({ accels, flat }) {
  if (!accels || accels.length === 0) {
    return (
      <div className="meta">
        <span>
          <HwChips encs={flat} />
        </span>
      </div>
    );
  }
  const groups = new Map();
  for (const a of accels) {
    const key = `${a.kind}-${a.device}`;
    if (!groups.has(key)) {
      groups.set(key, { kind: a.kind, device: a.device, name: a.name, backends: new Map() });
    }
    const g = groups.get(key);
    if (!g.backends.has(a.backend)) g.backends.set(a.backend, []);
    for (const e of a.encoders) {
      if (!g.backends.get(a.backend).includes(e)) g.backends.get(a.backend).push(e);
    }
  }
  return (
    <div className="accels">
      {[...groups.values()].map((g) => (
        <div className="accel-group" key={`${g.kind}-${g.device}`}>
          <div className="accel-head">
            <span className="accel-kind">{g.kind === "gpu" ? "🖥" : "⚙️"}</span>
            <b>{g.kind.toUpperCase()} #{g.device}</b>
            <span className="accel-name" title={g.name}>{g.name}</span>
          </div>
          {[...g.backends.entries()].map(([backend, encs]) => (
            <div className="accel-row" key={backend}>
              <span className="accel-backend">{backend}</span>
              <HwChips encs={encs} />
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

export default function App() {
  const [health, setHealth] = useState(null);
  const [codecs, setCodecs] = useState(null);

  // tabs: dashboard (default) | library | settings | test (the original screen)
  const [tab, setTab] = useState("dashboard");
  // dashboard: server host stats, refreshed on the 2s tick
  const [stats, setStats] = useState(null);
  // library automation settings (persisted server-side in auto.json)
  const [auto, setAuto] = useState(null);
  const [autoMsg, setAutoMsg] = useState("");
  const [autoSaving, setAutoSaving] = useState(false);

  // upload
  const [file, setFile] = useState(null);
  const [uploadedFile, setUploadedFile] = useState(null);
  const [uploading, setUploading] = useState(false);
  const [uploadProgress, setUploadProgress] = useState(0);
  const [dragActive, setDragActive] = useState(false);
  const dragCounter = useRef(0);
  const fileRef = useRef(null);
  // Live-ref to the active job so the periodic refresher (a stable closure)
  // can keep its progress current without re-subscribing on every change.
  const activeJobRef = useRef(null);

  // settings
  const [videoCodec, setVideoCodec] = useState("h264");
  const [audioCodec, setAudioCodec] = useState("aac");
  const [container, setContainer] = useState("mp4");
  const [crf, setCrf] = useState(23);
  const [rateMode, setRateMode] = useState("crf"); // "crf" | "pct"
  const [bitratePct, setBitratePct] = useState(70); // % of source video bitrate (100 = original)
  const [target, setTarget] = useState("server"); // server | auto | <nodeId>
  // source analysis (from the upload response's ffprobe)
  const [analysis, setAnalysis] = useState(null);

  // nodes
  const [nodes, setNodes] = useState([]);
  // registry view filters: status (online hidden-offline default), HW accel
  // backend, and encoder codec family
  const [nodeFilter, setNodeFilter] = useState({ status: "online", backend: "", family: "" });
  // node search (server finds a node by the IP:port it printed)
  const [searchIp, setSearchIp] = useState("");
  const [searchPort, setSearchPort] = useState("8401");
  const [searchMsg, setSearchMsg] = useState("");
  const [discovered, setDiscovered] = useState([]);
  // storage library (server-scanned directory of videos)
  const [library, setLibrary] = useState({ root: "", entries: [] });
  const [libRoot, setLibRoot] = useState("");
  const [scanning, setScanning] = useState(false);

  // jobs
  const [jobs, setJobs] = useState([]);
  const [activeJob, setActiveJob] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  // load catalog + health
  useEffect(() => {
    api.health().then(setHealth).catch(() => setHealth({ status: "down" }));
    api.autoGet().then(setAuto).catch(() => {});
    api
      .library()
      .then((l) => {
        setLibrary(l);
        if (l.root) setLibRoot(l.root);
      })
      .catch(() => {});
    api
      .codecs()
      .then((c) => {
        setCodecs(c);
        if (c.videoCodecs?.length) setVideoCodec(c.videoCodecs[0].id);
        if (c.audioCodecs?.length) setAudioCodec(c.audioCodecs[0].id);
        if (c.containers?.length) setContainer(c.containers[0].id);
      })
      .catch(() => {});
  }, []);

  // refresh jobs + nodes periodically
  useEffect(() => {
    const t = setInterval(() => {
      api.jobs().then(setJobs).catch(() => {});
      api.nodes().then(setNodes).catch(() => {});
      api.discoveredNodes().then(setDiscovered).catch(() => {});
      api.library().then(setLibrary).catch(() => {});
      if (tab === "dashboard") api.serverStats().then(setStats).catch(() => {});
      // keep the active job's progress bar live until it's terminal
      const aj = activeJobRef.current;
      if (aj && !["done", "error"].includes(aj.status)) {
        api
          .job(aj.id)
          .then((latest) => {
            activeJobRef.current = latest;
            setActiveJob(latest);
          })
          .catch(() => {});
      }
    }, 2000);
    return () => clearInterval(t);
  }, [tab]);

  // poll the active job for a smooth progress bar (500ms). Depend only on the
  // job id so the interval isn't torn down on every progress tick.
  useEffect(() => {
    const jid = activeJob?.id;
    if (!jid) return;
    const t = setInterval(async () => {
      try {
        const j = await api.job(jid);
        activeJobRef.current = j;
        setActiveJob(j);
        if (j.status === "done" || j.status === "error") {
          clearInterval(t);
          setJobs(await api.jobs());
          setBusy(false);
        }
      } catch {}
    }, 500);
    return () => clearInterval(t);
  }, [activeJob?.id]);

  // Shared upload path for both the file picker and drag-and-drop.
  async function handleFile(f) {
    if (!f) return;
    setFile(f);
    setUploadedFile(null);
    setError("");
    setUploading(true);
    setUploadProgress(0);
    try {
      const r = await api.upload(f, (p) => setUploadProgress(p));
      setUploadedFile(r.file);
      setAnalysis(r.analysis || null);
      setUploadProgress(1);
    } catch (err) {
      setError(err.message);
    } finally {
      setUploading(false);
    }
  }

  // File-picker change. Reset the input value so re-selecting the same file
  // still fires onChange.
  function onUpload(e) {
    handleFile(e.target.files?.[0]);
    e.target.value = "";
  }

  // Drag-and-drop: use a counter so entering/leaving child elements doesn't
  // flicker the highlight.
  function onDragEnter(e) {
    e.preventDefault();
    dragCounter.current += 1;
    if (e.dataTransfer?.types?.includes("Files")) setDragActive(true);
  }
  function onDragLeave(e) {
    e.preventDefault();
    dragCounter.current -= 1;
    if (dragCounter.current <= 0) {
      dragCounter.current = 0;
      setDragActive(false);
    }
  }
  function onDrop(e) {
    e.preventDefault();
    dragCounter.current = 0;
    setDragActive(false);
    handleFile(e.dataTransfer?.files?.[0]);
  }

  async function onStart() {
    if (!uploadedFile) {
      setError("Upload a file first.");
      return;
    }
    setBusy(true);
    setError("");
    const usePct = videoCodec !== "copy" && rateMode === "pct";
    const base = {
      file: uploadedFile,
      videoCodec,
      audioCodec,
      container,
      crf: videoCodec === "copy" || usePct ? undefined : crf,
      // capped-VBR mode: server computes kbps from the source's bitrate
      bitratePct: usePct ? bitratePct : undefined,
    };
    try {
      let id;
      if (target === "server") {
        id = (await api.transcode(base)).id;
      } else if (target === "auto") {
        id = (await api.dispatch(base)).id; // server auto-picks node + accelerates
      } else {
        id = (await api.dispatch({ ...base, nodeId: target })).id;
      }
      const j = await api.job(id);
      activeJobRef.current = j;
      setActiveJob(j);
      setJobs(await api.jobs());
    } catch (err) {
      setError(err.message);
    } finally {
      // `busy` only covers the submit call itself. The button is NOT locked
      // while a job runs — the user can queue more jobs (the progress bar in
      // "3 · Jobs" tracks the active job independently).
      setBusy(false);
    }
  }

  const progress = activeJob ? Math.round((activeJob.progress || 0) * 100) : 0;
  // Estimated target kbps for "% of source" mode — mirrors the server math so
  // the user sees the number before hitting Start.
  const estKbps =
    analysis?.videoBitRate != null
      ? Math.max(100, Math.round((analysis.videoBitRate * bitratePct) / 100 / 1000))
      : null;
  const onlineNodes = nodes.filter((n) => n.status === "online");
  const offlineNodes = nodes.filter((n) => n.status !== "online");

  // Registry view: what's actually shown in the Nodes card.
  const shownNodes = nodes.filter((n) => {
    if (nodeFilter.status !== "all" && n.status !== nodeFilter.status) return false;
    if (
      nodeFilter.backend &&
      !n.accels?.some((a) => a.backend === nodeFilter.backend)
    )
      return false;
    if (
      nodeFilter.family &&
      !n.hwEncoders?.some((h) => codecFamily(h) === nodeFilter.family)
    )
      return false;
    return true;
  });
  // Filter options derived from what the registry actually offers.
  const backendOptions = [...new Set(nodes.flatMap((n) => n.accels?.map((a) => a.backend) || []))].sort();
  const familyOptions = [...new Set(nodes.flatMap((n) => n.hwEncoders?.map(codecFamily) || []))].sort();

  // "Server finds a node": probe the IP:port the node printed in its terminal.
  async function doSearch() {
    const ip = searchIp.trim();
    if (!ip) return;
    setError("");
    setSearchMsg("");
    try {
      const n = await api.searchNode(ip, searchPort);
      setNodes(await api.nodes());
      setSearchMsg(
        `Found '${n.name}' — ${
          n.hwEncoders?.length ? "⚡ " + n.hwEncoders.join(", ") : "CPU only"
        }`
      );
    } catch (err) {
      setError(err.message);
    }
  }

  // Scan a storage directory: server walks it (read-only) + ffprobes each video.
  async function doScan() {
    setScanning(true);
    setError("");
    try {
      const l = await api.libraryScan(libRoot.trim());
      setLibrary(l);
      if (l.root) setLibRoot(l.root);
    } catch (err) {
      setError(err.message);
    } finally {
      setScanning(false);
    }
  }

  // Persist the library-automation settings (server saves them to auto.json
  // and applies them to newly found videos on the next scan).
  async function saveAuto() {
    if (!auto) return;
    setAutoSaving(true);
    setAutoMsg("");
    try {
      await api.autoPost(auto);
      setAutoMsg("저장됨 ✓");
      setTimeout(() => setAutoMsg(""), 3000);
    } catch (err) {
      setAutoMsg(err.message);
    } finally {
      setAutoSaving(false);
    }
  }

  return (
    <div className="wrap">
      <header>
        <h1>CodecTrans</h1>
        <nav className="tabs">
          {[
            ["dashboard", "대시보드"],
            ["library", "라이브러리"],
            ["settings", "세팅"],
            ["test", "테스트"],
          ].map(([id, label]) => (
            <button
              key={id}
              className={tab === id ? "on" : ""}
              onClick={() => setTab(id)}
            >
              {label}
            </button>
          ))}
        </nav>
        <div className={`pill ${health?.status === "ok" ? "ok" : "bad"}`}>
          {health?.status === "ok" ? "backend online" : "backend offline"}
        </div>
      </header>
      {health?.ffmpeg && <div className="sub">{health.ffmpeg}</div>}

      {/* ═══ 테스트 — the original upload/transcode screen ═══ */}
      {tab === "test" && (
      <div className="grid">
        {/* left: upload + settings */}
        <section className="card">
          <h2>1 · Source</h2>
          <div
            className={`dropzone ${dragActive ? "active" : ""}`}
            onDragEnter={onDragEnter}
            onDragOver={(e) => e.preventDefault()}
            onDragLeave={onDragLeave}
            onDrop={onDrop}
            onClick={() => fileRef.current?.click()}
            role="button"
            tabIndex={0}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") fileRef.current?.click();
            }}
          >
            <input
              ref={fileRef}
              type="file"
              accept="video/*,audio/*"
              onChange={onUpload}
              className="file-hidden"
            />
            <div className="dz-ico">🎬</div>
            <div className="dz-text">
              <b>{dragActive ? "Drop it here" : "Drag a video here"}</b>
              <span>or click to browse your files</span>
            </div>
          </div>
          {file && (
            <div className="meta">
              <span>{file.name}</span>
              <span>{fmtSize(file.size)}</span>
            </div>
          )}
          {uploading && (
            <div className="meta">
              <div className="prog-row">
                <span>Uploading… {Math.round(uploadProgress * 100)}%</span>
                <span>{file ? fmtSize(file.size) : ""}</span>
              </div>
              <div className="bar">
                <div className="bar-fill" style={{ width: `${uploadProgress * 100}%` }} />
              </div>
            </div>
          )}
          {uploadedFile && <div className="meta ok-text">Uploaded: {uploadedFile}</div>}

          {/* source analysis — what ffprobe found in the uploaded file */}
          {analysis && (
            <div className="analysis">
              <div className="an-title">🔎 Source</div>
              <div className="an-grid">
                <span>
                  <b>Video</b>{" "}
                  {analysis.videoCodec || "?"}
                  {analysis.videoProfile ? ` (${analysis.videoProfile})` : ""}
                </span>
                <span>
                  <b>Size</b>{" "}
                  {analysis.width ? `${analysis.width}×${analysis.height}` : "?"}
                  {analysis.fps ? ` @ ${fmtFps(analysis.fps)}` : ""} fps
                </span>
                <span>
                  <b>Length</b> {fmtDur(analysis.duration) || "?"}
                </span>
                <span>
                  <b>Bitrate</b> {fmtRate(analysis.videoBitRate) || "?"}
                </span>
                <span>
                  <b>Audio</b>{" "}
                  {analysis.audioCodec
                    ? `${analysis.audioCodec} · ${analysis.audioSampleRate
                        ? analysis.audioSampleRate / 1000 + " kHz"
                        : ""}${analysis.audioChannels ? " · " + analysis.audioChannels + "ch" : ""}`
                    : "none"}
                </span>
                <span>
                  <b>Pixel</b> {analysis.pixFmt || "?"} · {analysis.container || "?"}
                </span>
              </div>
            </div>
          )}

          <h2>2 · Target</h2>
          <label>
            Video codec
            <select value={videoCodec} onChange={(e) => setVideoCodec(e.target.value)}>
              {codecs?.videoCodecs.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Audio codec
            <select value={audioCodec} onChange={(e) => setAudioCodec(e.target.value)}>
              {codecs?.audioCodecs.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Container
            <select value={container} onChange={(e) => setContainer(e.target.value)}>
              {codecs?.containers.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label}
                </option>
              ))}
            </select>
          </label>
          {videoCodec !== "copy" && (
            <>
              <div className="ratemode">
                <button
                  type="button"
                  className={`rm-opt ${rateMode === "crf" ? "on" : ""}`}
                  onClick={() => setRateMode("crf")}
                >
                  Quality (CRF)
                </button>
                <button
                  type="button"
                  className={`rm-opt ${rateMode === "pct" ? "on" : ""}`}
                  onClick={() => setRateMode("pct")}
                >
                  % of source bitrate
                </button>
              </div>
              {rateMode === "crf" ? (
                <label>
                  Quality (CRF {crf}) — lower is better
                  <input
                    type="range"
                    min="0"
                    max="51"
                    value={crf}
                    onChange={(e) => setCrf(Number(e.target.value))}
                  />
                </label>
              ) : (
                <label>
                  Target: {bitratePct}% of source bitrate
                  {estKbps != null && (
                    <span className="est">
                      ≈ {fmtRate(estKbps * 1000)}
                      {analysis?.videoBitRate
                        ? ` (source ${fmtRate(analysis.videoBitRate)})`
                        : ""}{" "}
                      · capped at {fmtRate(estKbps * 1200)}
                    </span>
                  )}
                  {estKbps == null && (
                    <span className="est">
                      source bitrate unknown — the server will estimate from the
                      file size
                    </span>
                  )}
                  <input
                    type="range"
                    min="10"
                    max="100"
                    step="5"
                    value={bitratePct}
                    onChange={(e) => setBitratePct(Number(e.target.value))}
                  />
                  <div className="rm-scale">
                    <span>smaller file</span>
                    <span>50%</span>
                    <span className="mid">100% = original</span>
                  </div>
                </label>
              )}
            </>
          )}

          <label>
            Run on
            <select value={target} onChange={(e) => setTarget(e.target.value)}>
              <option value="server">Server (local CPU)</option>
              <option value="auto">Auto — best node (HW accel)</option>
              {onlineNodes.map((n) => (
                <option key={n.id} value={n.id}>
                  Node: {n.name}
                </option>
              ))}
            </select>
          </label>
          {target !== "server" && onlineNodes.length === 0 && (
            <div className="err">No nodes online — start a CodecTrans-node.</div>
          )}

          <button
            className="primary"
            onClick={onStart}
            disabled={busy || uploading || !uploadedFile}
          >
            {busy
              ? "Working…"
              : target === "server"
              ? "Transcode"
              : "Dispatch"}
          </button>
          {error && <div className="err">{error}</div>}
        </section>

        {/* right: progress + history */}
        <section className="card">
          <h2>3 · Jobs</h2>
          {activeJob && (
            <div className="active">
              <div className="row">
                <span className={`status ${activeJob.status}`}>{STATUS_LABEL[activeJob.status] || activeJob.status}</span>
                {activeJob.accelerated && (
                  <span className="accel" title={`HW: ${activeJob.effectiveVideoCodec}`}>
                    ⚡ {activeJob.effectiveVideoCodec}
                  </span>
                )}
                <span className="pct">{progress}%</span>
              </div>
              <div className="bar">
                <div
                  className={`fill ${activeJob.status === "error" ? "bad" : ""}`}
                  style={{ width: `${activeJob.status === "error" ? 100 : progress}%` }}
                />
              </div>
              <div className="meta">
                <span>by {activeJob.executedBy || "server"}</span>
                {activeJob.outputFile && (activeJob.status === "done" || activeJob.status === "error") && (
                  <span>
                    {activeJob.outputFile} · {fmtSize(activeJob.outputSize)}
                  </span>
                )}
              </div>
              {activeJob.error && <div className="err">{activeJob.error}</div>}
            </div>
          )}

          <div className="history">
            {jobs.length === 0 && <div className="empty">No jobs yet.</div>}
            {jobs.map((j) => (
              <div key={j.id} className="job">
                <div className="row">
                  <span className={`status ${j.status}`}>{STATUS_LABEL[j.status] || j.status}</span>
                  {j.accelerated && <span className="accel" title={`HW: ${j.effectiveVideoCodec}`}>⚡</span>}
                  <span className="jname">{j.outputFile || j.inputFile}</span>
                  <span className="jmeta">
                    {j.executedBy} · {j.effectiveVideoCodec} → {j.container}
                    {j.outputSize != null ? ` · ${fmtSize(j.outputSize)}` : ""}
                  </span>
                </div>
                {j.status === "done" && (
                  <a className="dl" href={api.downloadUrl(j.outputFile)} download>
                    ⬇ download
                  </a>
                )}
                {j.error && <div className="err">{j.error}</div>}
              </div>
            ))}
          </div>
        </section>
      </div>
      )}

      {/* ═══ 대시보드 — server CPU/RAM + node registry ═══ */}
      {tab === "dashboard" && (
        <>
          <section className="card stats">
            <h2>서버</h2>
            <div className="statgrid">
              <div className="stat">
                <div className="stat-top">
                  <span className="stat-label">CPU</span>
                  <span className="stat-val">
                    {stats?.cpuPct != null ? `${Math.round(stats.cpuPct)}%` : "—"}
                  </span>
                </div>
                <div className="bar">
                  <div
                    className="bar-fill"
                    style={{ width: `${Math.min(100, stats?.cpuPct ?? 0)}%` }}
                  />
                </div>
                {stats?.cores != null && (
                  <div className="meta">
                    <span>{stats.cores} cores</span>
                  </div>
                )}
              </div>
              <div className="stat">
                <div className="stat-top">
                  <span className="stat-label">RAM</span>
                  <span className="stat-val">
                    {stats?.ramPct != null ? `${Math.round(stats.ramPct)}%` : "—"}
                  </span>
                </div>
                <div className="bar">
                  <div
                    className="bar-fill"
                    style={{ width: `${Math.min(100, stats?.ramPct ?? 0)}%` }}
                  />
                </div>
                {stats?.ramTotal != null && (
                  <div className="meta">
                    <span>
                      {fmtSize(stats.ramUsed)} / {fmtSize(stats.ramTotal)}
                    </span>
                  </div>
                )}
              </div>
            </div>
          </section>
      <section className="card nodes">
        <h2>
          Nodes ({shownNodes.length})
          {offlineNodes.length > 0 && (
            <span className="meta" style={{ marginLeft: 8, fontWeight: 400 }}>
              · {offlineNodes.length} offline
            </span>
          )}
        </h2>

        {/* registry filters: online default (offline hidden), HW accel backend, codec family */}
        <div className="nodefilters">
          <div className="seg">
            {["online", "offline", "all"].map((s) => (
              <button
                key={s}
                className={nodeFilter.status === s ? "on" : ""}
                onClick={() => setNodeFilter((f) => ({ ...f, status: s }))}
              >
                {s === "online" ? `Online (${onlineNodes.length})` : s === "offline" ? `Offline (${offlineNodes.length})` : "All"}
              </button>
            ))}
          </div>
          <select
            value={nodeFilter.backend}
            onChange={(e) => setNodeFilter((f) => ({ ...f, backend: e.target.value }))}
          >
            <option value="">All backends</option>
            {backendOptions.map((b) => (
              <option key={b} value={b}>
                {b}
              </option>
            ))}
          </select>
          <select
            value={nodeFilter.family}
            onChange={(e) => setNodeFilter((f) => ({ ...f, family: e.target.value }))}
          >
            <option value="">All codecs</option>
            {familyOptions.map((f) => (
              <option key={f} value={f}>
                {f}
              </option>
            ))}
          </select>
        </div>

        {/* server finds a node: enter the IP:port it printed in its terminal */}
        <div className="search">
          <input
            value={searchIp}
            onChange={(e) => setSearchIp(e.target.value)}
            placeholder="node IP (e.g. 192.168.x.x)"
          />
          <input
            value={searchPort}
            onChange={(e) => setSearchPort(e.target.value)}
            placeholder="port"
            className="port"
          />
          <button onClick={doSearch} disabled={!searchIp.trim()}>
            Search
          </button>
        </div>
        {searchMsg && <div className="meta ok-text">{searchMsg}</div>}
        <div className="hint">
          A node prints its <code>IP:port</code> on start (e.g.
          <code> http://192.168.x.x:8401</code>). Enter it here — the server
          verifies and registers it. Or set <code>CT_SERVER</code> on the node
          and it connects itself.
        </div>

        {nodes.length === 0 && (
          <div className="empty">
            No nodes yet. Run <code>CodecTransNode</code> and search it above.
          </div>
        )}
        {nodes.length > 0 && shownNodes.length === 0 && (
          <div className="empty">
            No nodes match the current filter.
            <button
              className="linkish"
              onClick={() => setNodeFilter({ status: "online", backend: "", family: "" })}
            >
              Reset
            </button>
          </div>
        )}
        {shownNodes.map((n) => (
          <div key={n.id} className="node">
            <div className="row">
              <span className={`status ${n.status === "online" ? "done" : ""}`}>{n.status}</span>
              <span className="jname">{n.name}</span>
              {n.currentJob && <span className="jmeta">busy: {n.currentJob.slice(0, 8)}…</span>}
              <span className="jmeta">{n.ffmpegVersion.split(" ").slice(0, 3).join(" ")}</span>
            </div>
            <AccelPanel accels={n.accels} flat={n.hwEncoders} />
          </div>
        ))}

        {discovered.length > 0 && (
          <>
            <h3>Seen on the LAN (mDNS, not yet registered)</h3>
            {discovered.map((d) => (
              <div key={d.name} className="node">
                <div className="row">
                  <span className="status done">seen</span>
                  <span className="jname">{d.name}</span>
                  <span className="jmeta">
                    {d.ip}:{d.port}
                  </span>
                </div>
                <div className="meta">
                  <span>
                    <HwChips encs={d.hwEncoders} />
                  </span>
                </div>
              </div>
            ))}
          </>
        )}
      </section>
        </>
      )}

      {/* ═══ 라이브러리 — folder + automation ═══ */}
      {tab === "library" && (
        <>
          <section className="card auto">
            <h2>자동화</h2>
            <div className="hint">
              활성화하면 스캔할 때마다 <b>새로 발견된</b> 영상을 아래 설정으로
              자동 큐잉합니다. 가장 적합한 노드의 HW 인코더가 자동으로 선택되고,
              결과는 원본 옆에 저장됩니다 (원본은 건드리지 않음).
            </div>
            {auto ? (
              <div className="autoform">
                <label className="chk">
                  <input
                    type="checkbox"
                    checked={!!auto.enabled}
                    onChange={(e) => setAuto({ ...auto, enabled: e.target.checked })}
                  />
                  새 영상 자동 처리
                </label>
                <div className="auto-row">
                  <label>
                    비디오 코덱
                    <select
                      value={auto.videoCodec || "h264"}
                      onChange={(e) => setAuto({ ...auto, videoCodec: e.target.value })}
                    >
                      {(codecs?.videoCodecs || [])
                        .filter((c) => c.id !== "copy")
                        .map((c) => (
                          <option key={c.id} value={c.id}>
                            {c.label}
                          </option>
                        ))}
                    </select>
                  </label>
                  <label>
                    오디오 코덱
                    <select
                      value={auto.audioCodec || "aac"}
                      onChange={(e) => setAuto({ ...auto, audioCodec: e.target.value })}
                    >
                      {(codecs?.audioCodecs || []).map((c) => (
                        <option key={c.id} value={c.id}>
                          {c.label}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label>
                    컨테이너
                    <select
                      value={auto.container || "mp4"}
                      onChange={(e) => setAuto({ ...auto, container: e.target.value })}
                    >
                      {(codecs?.containers || []).map((c) => (
                        <option key={c.id} value={c.id}>
                          {c.label}
                        </option>
                      ))}
                    </select>
                  </label>
                </div>
                <div className="auto-row">
                  <div className="ratemode">
                    <button
                      type="button"
                      className={`rm-opt ${auto.mode === "crf" ? "on" : ""}`}
                      onClick={() => setAuto({ ...auto, mode: "crf" })}
                    >
                      질량 (CRF)
                    </button>
                    <button
                      type="button"
                      className={`rm-opt ${auto.mode !== "crf" ? "on" : ""}`}
                      onClick={() => setAuto({ ...auto, mode: "pct" })}
                    >
                      원본 비트레이트 %
                    </button>
                  </div>
                  {auto.mode !== "crf" ? (
                    <label>
                      목표: {auto.bitratePct ?? 70}%
                      <input
                        type="range"
                        min="10"
                        max="100"
                        step="5"
                        value={auto.bitratePct ?? 70}
                        onChange={(e) =>
                          setAuto({ ...auto, bitratePct: Number(e.target.value) })
                        }
                      />
                      <div className="rm-scale">
                        <span>작아짐</span>
                        <span>50%</span>
                        <span className="mid">100% = 원본</span>
                      </div>
                    </label>
                  ) : (
                    <label>
                      CRF {auto.crf ?? 23} — 낮을수록 선명
                      <input
                        type="range"
                        min="0"
                        max="51"
                        value={auto.crf ?? 23}
                        onChange={(e) => setAuto({ ...auto, crf: Number(e.target.value) })}
                      />
                    </label>
                  )}
                </div>
                <div className="row">
                  <button className="primary" onClick={saveAuto} disabled={autoSaving}>
                    {autoSaving ? "저장 중…" : "자동화 저장"}
                  </button>
                  {autoMsg && <span className="meta ok-text">{autoMsg}</span>}
                </div>
              </div>
            ) : (
              <div className="meta">
                <span>자동화 설정을 불러오는 중…</span>
              </div>
            )}
          </section>

      {/* storage library (server-scanned directory of videos) */}
      <section className="card lib">
        <h2>Library</h2>
        <div className="hint">
          Point the server at a folder of videos (a NAS share it can mount, or a
          local dir). It scans <b>read-only</b> — your files are never touched.
          Next: pick a target codec and re-encode in place (backed up).
        </div>
        <div className="search">
          <input
            value={libRoot}
            onChange={(e) => setLibRoot(e.target.value)}
            placeholder="storage path (e.g. /mnt/nas/Videos)"
          />
          <button onClick={doScan} disabled={scanning || !libRoot.trim()}>
            {scanning ? "Scanning…" : "Scan"}
          </button>
        </div>

        {library.root && (
          <div className="meta">
            root: <code>{library.root}</code> · {library.entries.length} videos
          </div>
        )}

        {library.entries.length > 0 ? (
          <div className="libtable">
            <div className="libhead row">
              <span className="c-name">File</span>
              <span className="c-codec">Codec</span>
              <span className="c-spec">Specs</span>
              <span className="c-size">Size</span>
              <span className="c-dur">Duration</span>
              <span className="c-status">Status</span>
            </div>
            {library.entries.map((e) => (
              <div key={e.path} className="librow row">
                <span className="c-name jname" title={e.path}>{e.name}</span>
                <span className="c-codec">
                  {e.videoCodec || "?"}
                  {e.audioCodec ? ` / ${e.audioCodec}` : ""}
                </span>
                <span className="c-spec jmeta">
                  {e.width ? `${e.width}×${e.height}` : "—"}
                  {e.fps ? ` · ${fmtFps(e.fps)}fps` : ""}
                  {e.videoBitRate ? ` · ${fmtRate(e.videoBitRate)}` : ""}
                </span>
                <span className="c-size jmeta">{fmtSize(e.size)}</span>
                <span className="c-dur jmeta">{e.duration ? fmtDur(e.duration) : "—"}</span>
                <span className="c-status">
                  <span className={`status ${e.status === "done" ? "done" : ""}`}>{e.status}</span>
                </span>
              </div>
            ))}
          </div>
        ) : (
          <div className="empty">
            {library.root ? "No videos found." : "Scan a folder to build the library."}
          </div>
        )}
      </section>
        </>
      )}

      {/* ═══ 세팅 — skeleton for now ═══ */}
      {tab === "settings" && (
        <section className="card settings">
          <h2>세팅</h2>
          <div className="hint">
            더 많은 설정은 추가 예정입니다. 지금은 서버 정보만 보여드려요.
          </div>
          <div className="statgrid">
            <div className="stat">
              <div className="stat-top">
                <span className="stat-label">서비스</span>
                <span className="stat-val">{health?.service || "—"}</span>
              </div>
            </div>
            <div className="stat">
              <div className="stat-top">
                <span className="stat-label">FFmpeg</span>
                <span className="stat-val">
                  {health?.ffmpeg ? health.ffmpeg.split(" ").slice(0, 3).join(" ") : "—"}
                </span>
              </div>
            </div>
          </div>
        </section>
      )}
    </div>
  );
}
