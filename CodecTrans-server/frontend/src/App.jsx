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

// Legacy ordered block list → node graph (same meaning): start → c1 →(✓) a1,
// c1(✗) → c2 →(✓) a2, … so old settings keep working on the canvas.
function migrateBlocksToGraph(blocks) {
  const live = blocks.filter((b) => b.enabled);
  const nodes = [{ id: "start", kind: "start", enabled: true, x: 40, y: 160 }];
  const edges = [];
  let prevFalse = null;
  live.forEach((b, i) => {
    const cid = `c${i + 1}`;
    const aid = `a${i + 1}`;
    nodes.push({ id: cid, kind: "condition", enabled: true, condition: b.condition || {}, x: 340, y: 30 + i * 200 });
    nodes.push({ id: aid, kind: "action", enabled: true, action: b.action || {}, x: 660, y: 30 + i * 200 });
    // start→c1 is portless; cN(✗)→cN+1 rides the false branch
    edges.push({ from: prevFalse || "start", ...(prevFalse ? { fromPort: "false" } : {}), to: cid });
    edges.push({ from: cid, fromPort: "true", to: aid });
    prevFalse = cid;
  });
  return { nodes, edges };
}
// Bézier path for a wire. When the target sits to the LEFT of the source
// (false-branch drop-down), bulge outward instead of looping back.
function wirePath(x1, y1, x2, y2) {
  let dx;
  if (x2 >= x1) dx = Math.max(40, Math.min(130, (x2 - x1) / 2));
  else dx = Math.min(180, Math.max(70, (x1 - x2) / 2 + 80));
  return `M ${x1} ${y1} C ${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`;
}

// Header title for a node — specific, not "조건"/"액션" only. Used when the
// user hasn't set a custom label.
function nodeTitle(n) {
  if (n.label) return n.label;
  const c = n.condition || {};
  const a = n.action || {};
  const codec = (id) => (id ? (id === "h265" ? "HEVC" : id.toUpperCase()) : "?");
  switch (n.kind) {
    case "start":
      return "새 영상 발견";
    case "condition":
      switch (c.kind) {
        case "srcCodec":
          return "코덱 = " + (c.codec ? codec(c.codec) : "∅");
        case "sizeGt":
          return "크기 > " + (c.value ?? "∅") + " MB";
        case "sizeLt":
          return "크기 < " + (c.value ?? "∅") + " MB";
        case "bitrateGt":
          return "비트레이트 > " + (c.value ?? "∅") + " Mbps";
        case "bitrateLt":
          return "비트레이트 < " + (c.value ?? "∅") + " Mbps";
        case "nameContains":
          return "이름 '" + (c.pattern || "∅") + "' 포함";
        case "nodeHasHw": {
          const cls =
            c.accelClass === "gpu" ? "GPU " : c.accelClass === "cpu" ? "iGPU " : "";
          return "노드: " + cls + (c.hwFamily || "?") + " HW 인코더";
        }
        default:
          return "항상 통과";
      }
    case "action":
      if (a.kind === "skip") return "건너뛰기";
      if (a.kind === "healthcheck") return "헬스체크";
      const t = a.target || {};
      const rate = t.mode === "crf" ? "CRF " + (t.crf ?? 23) : (t.bitratePct ?? 70) + "%";
      return "인코딩 → " + (t.videoCodec ? codec(t.videoCodec) : "폴더 기본") + " · " + rate;
    default:
      return n.kind;
  }
}

// node width in the logic canvas (must match .gnode CSS width)
const NODE_W = 224;

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

  // tabs: dashboard | library | logic | settings | test (the original screen)
  const [tab, setTab] = useState("dashboard");
  // dashboard: server host stats, refreshed on the 2s tick
  const [stats, setStats] = useState(null);

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
  // storage library (server-scanned folders of videos)
  const [library, setLibrary] = useState({ root: "", entries: [] });
  const [scanning, setScanning] = useState(false);
  // registered folders (add/remove list) + per-folder encoding target
  const [folders, setFolders] = useState([]);
  const [folderMsg, setFolderMsg] = useState("");
  const [newFolder, setNewFolder] = useState("");
  // logic automation — ComfyUI-style node graph (start → condition ✓/✗ → action)
  const [graph, setGraph] = useState({ nodes: [], edges: [] });
  const [blockMsg, setBlockMsg] = useState("");

  // jobs
  const [jobs, setJobs] = useState([]);
  const [activeJob, setActiveJob] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  // load catalog + health
  useEffect(() => {
    api.health().then(setHealth).catch(() => setHealth({ status: "down" }));
    api.foldersGet().then(setFolders).catch(() => {});
    api
      .blocksGet()
      .then((d) => {
        let g =
          d.graph && d.graph.nodes?.length
            ? d.graph
            : migrateBlocksToGraph(d.blocks || []);
        if (!g.nodes.some((n) => n.kind === "start")) {
          g = {
            nodes: [{ id: "start", kind: "start", enabled: true, x: 40, y: 160 }, ...g.nodes],
            edges: g.edges,
          };
        }
        setGraph(g);
      })
      .catch(() => {});
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

  // Scan all enabled registered folders (read-only); the server auto-queues
  // newly found videos per the saved automation (logic blocks first, then the
  // folder default target).
  async function doScan() {
    setScanning(true);
    setError("");
    try {
      const l = await api.libraryScan();
      setLibrary({ root: "", entries: l.entries || [] });
    } catch (err) {
      setError(err.message);
    } finally {
      setScanning(false);
    }
  }

  // ── folders (add/remove list + per-folder target) ──────────────
  const uid = () =>
    crypto.randomUUID
      ? crypto.randomUUID()
      : "id-" + Date.now() + "-" + Math.random().toString(36).slice(2);

  // ── library re-encode (per-file ⟳ button) ──────────────────────
  const libKey = (e) => (e.folder ? e.folder + "/" : "") + e.path;
  const libJob = (e) =>
    jobs.find((j) => j.libPath === libKey(e) && !["done", "error"].includes(j.status));
  async function doRequeue(e) {
    setError("");
    try {
      await api.requeue(libKey(e));
      await doScan(); // the scan re-queues the now-pending entry
    } catch (err) {
      setError(err.message);
    }
  }
  const flash = (set) => (m) => {
    set(m);
    setTimeout(() => set(""), 2500);
  };
  const flashFolder = flash(setFolderMsg);
  const flashBlock = flash(setBlockMsg);
  async function saveFolders(next) {
    setFolders(next);
    try {
      await api.foldersPost(next);
      flashFolder("폴더 저장됨 ✓");
    } catch (err) {
      flashFolder(err.message);
    }
  }
  function addFolder() {
    const p = newFolder.trim();
    if (!p || folders.some((f) => f.path === p)) return;
    saveFolders([
      ...folders,
      {
        id: uid(),
        path: p,
        enabled: true,
        target: {
          videoCodec: "h265",
          audioCodec: "aac",
          container: "mp4",
          mode: "pct",
          bitratePct: 70,
        },
      },
    ]);
    setNewFolder("");
  }
  const patchFolder = (id, fn) =>
    saveFolders(folders.map((f) => (f.id === id ? fn(f) : f)));
  const patchTarget = (id, patch) =>
    patchFolder(id, (f) => ({ ...f, target: { ...f.target, ...patch } }));
  const removeFolder = (id) => saveFolders(folders.filter((f) => f.id !== id));
  const folderPath = (id) => {
    if (!id) return "(임시 루트)";
    const f = folders.find((x) => x.id === id);
    return f ? f.path : "제거됨";
  };
  const shortPath = (p) => {
    const parts = p.split("/").filter(Boolean);
    return parts.slice(-2).join("/") || p;
  };

  // ── logic node graph (ComfyUI style) ────────────────────────
  const graphRef = useRef(graph);
  useEffect(() => {
    graphRef.current = graph;
  }, [graph]);
  const commitGraph = (g) => {
    setGraph(g);
    graphRef.current = g;
    api.blocksPost({ graph: g }).catch((err) => flashBlock(err.message));
  };
  const patchNode = (id, patch) => {
    const g = graphRef.current;
    commitGraph({ ...g, nodes: g.nodes.map((n) => (n.id === id ? { ...n, ...patch } : n)) });
  };
  const patchCond = (id, patch) => {
    const n = graphRef.current.nodes.find((n) => n.id === id);
    patchNode(id, { condition: { ...(n?.condition || {}), ...patch } });
  };
  const patchAct = (id, patch) => {
    const n = graphRef.current.nodes.find((n) => n.id === id);
    patchNode(id, { action: { ...(n?.action || {}), ...patch } });
  };
  const patchActTarget = (id, patch) => {
    const n = graphRef.current.nodes.find((n) => n.id === id);
    patchNode(id, {
      action: { ...(n?.action || {}), target: { ...(n?.action?.target || {}), ...patch } },
    });
  };
  const addNode = (kind, preset = {}) => {
    const g = graphRef.current;
    if (kind === "start" && g.nodes.some((n) => n.kind === "start")) return;
    const id = `n${Date.now().toString(36)}${Math.floor(Math.random() * 999)}`;
    const base = {
      id,
      enabled: true,
      x: 380 + (g.nodes.length % 4) * 48,
      y: 50 + (g.nodes.length % 6) * 44,
    };
    let node;
    if (kind === "start") node = { ...base, kind: "start" };
    else if (kind === "cond")
      node = {
        ...base,
        kind: "condition",
        condition:
          preset.kind === "nodeHasHw"
            ? { kind: "nodeHasHw", hwFamily: "av1" }
            : { kind: preset.kind || "always" },
      };
    else if (kind === "act")
      node = {
        ...base,
        kind: "action",
        action:
          preset.kind === "encode"
            ? { kind: "encode", target: { videoCodec: "av1", container: "mp4", mode: "pct", bitratePct: 70 } }
            : { kind: preset.kind || "skip" },
      };
    else return;
    commitGraph({ ...g, nodes: [...g.nodes, node] });
  };
  const removeNode = (id) => {
    if (id === "start") return;
    const g = graphRef.current;
    commitGraph({
      nodes: g.nodes.filter((n) => n.id !== id),
      edges: g.edges.filter((e) => e.from !== id && e.to !== id),
    });
  };
  const removeEdgeAt = (i) => {
    const g = graphRef.current;
    commitGraph({ ...g, edges: g.edges.filter((_, j) => j !== i) });
  };

  // canvas view: pan (x,y) + zoom (k) applied to the .gworld wrapper
  const [view, setView] = useState({ x: 20, y: 10, k: 1 });
  const viewRef = useRef(view);
  useEffect(() => {
    viewRef.current = view;
  }, [view]);
  const zoomBy = (factor) => {
    const v = viewRef.current;
    const k = Math.max(0.4, Math.min(2.5, v.k * factor));
    // zoom around the canvas center
    const rect = canvasRef.current?.getBoundingClientRect();
    const cx = rect ? rect.width / 2 : 400;
    const cy = rect ? rect.height / 2 : 300;
    setView({ x: cx - ((cx - v.x) / v.k) * k, y: cy - ((cy - v.y) / v.k) * k, k });
  };

  // node drag + wire creation + canvas pan (window-level listeners;
  // ref-driven so the mount-only effect always sees fresh state)
  const canvasRef = useRef(null);
  const dragRef = useRef(null); // { id, ox, oy } — offsets in WORLD coords
  const pendingRef = useRef(null); // { from, port }
  const panRef = useRef(null); // { sx, sy, vx, vy }
  const [dragId, setDragId] = useState(null);
  const [pending, setPending] = useState(null); // { from, port, x, y } (world)

  const toWorld = (e) => {
    const rect = canvasRef.current.getBoundingClientRect();
    const v = viewRef.current;
    return { x: (e.clientX - rect.left - v.x) / v.k, y: (e.clientY - rect.top - v.y) / v.k };
  };
  const onNodeDown = (e, n) => {
    if (e.target.closest("input,select,button,label,textarea")) return;
    const w = toWorld(e);
    dragRef.current = { id: n.id, ox: w.x - n.x, oy: w.y - n.y };
    setDragId(n.id);
    e.preventDefault();
  };
  const onSocketDown = (e, n, port) => {
    e.stopPropagation();
    e.preventDefault();
    const w = toWorld(e);
    pendingRef.current = { from: n.id, port };
    setPending({ from: n.id, port, x: w.x, y: w.y });
  };
  // dragging empty canvas = pan the view
  const onCanvasDown = (e) => {
    if (e.target.closest(".gnode,.gsock,.ghit,.gzoom")) return;
    const v = viewRef.current;
    panRef.current = { sx: e.clientX, sy: e.clientY, vx: v.x, vy: v.y };
  };
  useEffect(() => {
    const move = (e) => {
      if (panRef.current) {
        const p = panRef.current;
        setView((v) => ({ ...v, x: p.vx + (e.clientX - p.sx), y: p.vy + (e.clientY - p.sy) }));
        return;
      }
      const w = toWorld(e);
      const d = dragRef.current;
      if (d) {
        const x = Math.max(-200, w.x - d.ox);
        const y = Math.max(-100, w.y - d.oy);
        setGraph((g) => ({
          ...g,
          nodes: g.nodes.map((n) => (n.id === d.id ? { ...n, x, y } : n)),
        }));
      }
      const p = pendingRef.current;
      if (p) setPending({ ...p, x: w.x, y: w.y });
    };
    const up = (e) => {
      if (panRef.current) {
        panRef.current = null;
        return;
      }
      if (dragRef.current) {
        dragRef.current = null;
        setDragId(null);
        api.blocksPost({ graph: graphRef.current }).catch(() => {});
      }
      if (pendingRef.current) {
        const p = pendingRef.current;
        pendingRef.current = null;
        setPending(null);
        const el = document.elementFromPoint(e.clientX, e.clientY);
        const t = el && el.closest ? el.closest("[data-node-in]") : null;
        if (t) {
          const to = t.getAttribute("data-node-in");
          if (to && to !== p.from) {
            const g = graphRef.current;
            // one incoming wire per node — replace any previous one
            const edges = g.edges.filter((e2) => e2.to !== to);
            edges.push({ from: p.from, ...(p.port ? { fromPort: p.port } : {}), to });
            commitGraph({ ...g, edges });
          }
        }
      }
    };
    // wheel zoom (native, non-passive so preventDefault works)
    const wheel = (e) => {
      if (!canvasRef.current || !e.target.closest(".gcanvas")) return;
      e.preventDefault();
      const v = viewRef.current;
      const factor = e.deltaY < 0 ? 1.1 : 1 / 1.1;
      const k = Math.max(0.4, Math.min(2.5, v.k * factor));
      const rect = canvasRef.current.getBoundingClientRect();
      const cx = e.clientX - rect.left;
      const cy = e.clientY - rect.top;
      setView({ x: cx - ((cx - v.x) / v.k) * k, y: cy - ((cy - v.y) / v.k) * k, k });
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    window.addEventListener("wheel", wheel, { passive: false });
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      window.removeEventListener("wheel", wheel);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="wrap">
      <header>
        <h1>CodecTrans</h1>
        <nav className="tabs">
          {[
            ["dashboard", "대시보드"],
            ["library", "라이브러리"],
            ["logic", "로직"],
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

      {/* ═══ 라이브러리 — folder list + per-folder target ═══ */}
      {tab === "library" && (
        <>
          <section className="card auto">
            <h2>폴더 관리</h2>
            <div className="hint">
              감시할 폴더를 리스트에 추가/제거하고, 폴더별 <b>기본 인코딩 설정</b>을
              정하세요. 스캔 시 새로 발견된 영상을 자동 큐잉합니다 — <b>로직 탭의
              블록 조건이 먼저 적용</b>되고, 안 맞으면 폴더 기본 설정을 씁니다. 결과는
              원본 옆에 저장되며 원본은 건드리지 않습니다.
            </div>
            <div className="search">
              <input
                value={newFolder}
                onChange={(e) => setNewFolder(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && addFolder()}
                placeholder="폴더 경로 (예: /mnt/nas/Videos)"
              />
              <button onClick={addFolder} disabled={!newFolder.trim()}>
                추가
              </button>
            </div>
            {folders.length === 0 && (
              <div className="empty">
                등록된 폴더가 없습니다. 위 입력창에 폴더 경로를 넣고 추가하세요.
              </div>
            )}
            {folders.map((f) => {
              const mode = f.target.mode || "pct";
              return (
                <div key={f.id} className={"folder" + (f.enabled ? "" : " off")}>
                  <div className="row">
                    <label className="chk" title="활성/비활성">
                      <input
                        type="checkbox"
                        checked={!!f.enabled}
                        onChange={() => patchFolder(f.id, (x) => ({ ...x, enabled: !x.enabled }))}
                      />
                    </label>
                    <code className="fpath" title={f.path}>
                      {f.path}
                    </code>
                    <button
                      className="rm-del"
                      title="폴더 제거"
                      onClick={() => removeFolder(f.id)}
                    >
                      ✕
                    </button>
                  </div>
                  <div className="frow">
                    <label>
                      비디오
                      <select
                        value={f.target.videoCodec || ""}
                        onChange={(e) => patchTarget(f.id, { videoCodec: e.target.value || null })}
                      >
                        <option value="">코덱 없음 (큐잉 안 함)</option>
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
                      오디오
                      <select
                        value={f.target.audioCodec || "aac"}
                        onChange={(e) => patchTarget(f.id, { audioCodec: e.target.value })}
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
                        value={f.target.container || "mp4"}
                        onChange={(e) => patchTarget(f.id, { container: e.target.value })}
                      >
                        {(codecs?.containers || []).map((c) => (
                          <option key={c.id} value={c.id}>
                            {c.label}
                          </option>
                        ))}
                      </select>
                    </label>
                    <div className="rateval">
                      <div className="ratemode">
                        <button
                          type="button"
                          className={`rm-opt ${mode === "crf" ? "on" : ""}`}
                          onClick={() => patchTarget(f.id, { mode: "crf" })}
                        >
                          CRF
                        </button>
                        <button
                          type="button"
                          className={`rm-opt ${mode !== "crf" ? "on" : ""}`}
                          onClick={() => patchTarget(f.id, { mode: "pct" })}
                        >
                          %
                        </button>
                      </div>
                      <input
                        type="number"
                        className="num"
                        min={mode === "crf" ? 0 : 10}
                        max={mode === "crf" ? 51 : 100}
                        value={mode === "crf" ? f.target.crf ?? 23 : f.target.bitratePct ?? 70}
                        onChange={(e) =>
                          patchTarget(
                            f.id,
                            mode === "crf"
                              ? { crf: Number(e.target.value) }
                              : { bitratePct: Number(e.target.value) }
                          )
                        }
                      />
                    </div>
                  </div>
                </div>
              );
            })}
            {folderMsg && <div className="meta ok-text">{folderMsg}</div>}
          </section>

          <section className="card lib">
            <h2>Library</h2>
            <div className="hint">
              활성 폴더 전체를 <b>read-only</b>로 스캔합니다 — 파일은 절대
              건드리지 않습니다. 새로 발견된 영상은 폴더/로직 설정대로 자동 큐잉됩니다.
            </div>
            <div className="search">
              <button
                onClick={doScan}
                disabled={scanning || !folders.some((f) => f.enabled)}
              >
                {scanning ? "Scanning…" : "전체 다시 스캔"}
              </button>
            </div>
            {library.entries.length > 0 && (
              <div className="libsummary">
                {(() => {
                  const es = library.entries;
                  const done = es.filter((e) => e.status === "done");
                  const err = es.filter((e) => e.status === "error");
                  const act = es.filter((e) => libJob(e));
                  const saved = done.reduce(
                    (a, e) => a + Math.max(0, (e.size || 0) - (e.newSize || 0)),
                    0
                  );
                  const shrunk = done.filter((e) => (e.newSize || 0) < (e.size || 0));
                  return (
                    <>
                      <span className="ls-item"><b>{es.length}</b> 파일</span>
                      <span className="ls-item ok"><b>{done.length}</b> 완료</span>
                      {act.length > 0 && (
                        <span className="ls-item run"><b>{act.length}</b> 진행 중</span>
                      )}
                      {err.length > 0 && (
                        <span className="ls-item bad"><b>{err.length}</b> 오류</span>
                      )}
                      {saved > 0 && (
                        <span className="ls-item save" title={`${shrunk.length}개 파일 원본 대비 용량 감소분`}>
                          💾 <b>{fmtSize(saved)}</b> 절약
                        </span>
                      )}
                    </>
                  );
                })()}
              </div>
            )}
            {library.entries.length > 0 ? (
              <div className="libtable">
                <div className="libhead row">
                  <span className="c-name">File</span>
                  <span className="c-folder">Folder</span>
                  <span className="c-codec">Codec</span>
                  <span className="c-spec">Specs</span>
                  <span className="c-size">Size</span>
                  <span className="c-dur">Duration</span>
                  <span className="c-status">Status</span>
                </div>
                {library.entries.map((e) => (
                  <div
                    key={(e.folder ? e.folder + "/" : "") + e.path}
                    className="librow row"
                  >
                    <span className="c-name jname" title={e.path}>
                      {e.name}
                    </span>
                    <span className="c-folder jmeta" title={folderPath(e.folder)}>
                      {e.folder ? shortPath(folderPath(e.folder)) : "(임시)"}
                    </span>
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
                    <span className="c-dur jmeta">
                      {e.duration ? fmtDur(e.duration) : "—"}
                    </span>
                    <span className="c-status">
                      {libJob(e) ? (
                        (() => {
                          const j = libJob(e);
                          const pct = Math.round((j.progress || 0) * 100);
                          return (
                            <span
                              className="progcell"
                              title={`${j.status} → ${j.effectiveVideoCodec || j.videoCodec} (${j.executedBy || "?"})`}
                            >
                              <span className="prog">
                                <i style={{ width: pct + "%" }} />
                              </span>
                              <span className="prog-pct">{pct}%</span>
                            </span>
                          );
                        })()
                      ) : (
                        <span className="statusrow">
                          <span
                            className={`status ${
                              e.status === "done"
                                ? "done"
                                : e.status === "error"
                                ? "error"
                                : e.status === "queued"
                                ? "running"
                                : ""
                            }`}
                            title={e.status === "error" && e.error ? e.error : undefined}
                          >
                            {e.status}
                          </span>
                          {(e.status === "done" || e.status === "error") && (
                            <button
                              className="mini"
                              title="재인코딩 — 다시 큐에 넣기"
                              onClick={() => doRequeue(e)}
                            >
                              ⟳
                            </button>
                          )}
                        </span>
                      )}
                      {e.status === "error" && e.error && (
                        <div className="errline" title={e.error}>
                          {e.error}
                        </div>
                      )}
                    </span>
                  </div>
                ))}
              </div>
            ) : (
              <div className="empty">
                활성 폴더에 영상이 없거나 아직 스캔하지 않았습니다.
              </div>
            )}
          </section>
        </>
      )}

                  {/* ═══ 로직 — ComfyUI-style node graph automation ═══ */}
      {tab === "logic" && (
        <>
          <section className="card auto">
            <h2>자동화 로직</h2>
            <div className="hint">
              노드 그래프(워크플로) 방식입니다. <b>왼쪽 팔레트에서 노드를 클릭</b>해
              캔버스에 추가하고, 소켓(●)을 드래그해 와이어를 이어주세요. 새 영상이{" "}
              <b>시작</b>에서 들어와 와이어를 따라가다가 <b>첫 도착한 액션 노드</b>
              에서 처리됩니다. 조건 노드는{" "}
              <b style={{ color: "#35d07f" }}>✓(맞으면)</b> /{" "}
              <b style={{ color: "#e05555" }}> ✗(안 맞으면)</b> 두 갈래 —
              직렬은 AND, 나란히는 OR. 어떤 액션에도 못 가면 폴더 기본 설정을 씁니다.
            </div>
            <div className="gtoolbar">
              <span className="meta dim">
                노드: 헤더로 드래그 · 제목 더블클릭 = 이름 변경 · 와이어 클릭 = 끊기 ·
                캔버스 드래그 = 이동 · 휠 = 확대/축소
              </span>
              {blockMsg && <span className="meta ok-text">{blockMsg}</span>}
            </div>
            <div className="logic-flex">
              <aside className="gpals">
                <div className="gpal-sec">시작</div>
                <button
                  className="gpal-item"
                  disabled={graph.nodes.some((n) => n.kind === "start")}
                  onClick={() => addNode("start")}
                >
                  ▶ 시작
                  <span>새 영상이 발견되면</span>
                </button>
                <div className="gpal-sec">📹 영상 조건</div>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "srcCodec" })}>
                  원본 코덱 =
                  <span>h264 / hevc / av1 …</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "sizeGt" })}>
                  파일 크기 &gt;
                  <span>MB 단위로</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "sizeLt" })}>
                  파일 크기 &lt;
                  <span>MB 단위로</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "bitrateGt" })}>
                  원본 비트레이트 &gt;
                  <span>Mbps 단위로</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "bitrateLt" })}>
                  원본 비트레이트 &lt;
                  <span>Mbps 단위로</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "nameContains" })}>
                  이름에 포함
                  <span>예: 4K, raw, .mkv</span>
                </button>
                <div className="gpal-sec">🖥 노드(서버) 조건</div>
                <button className="gpal-item" onClick={() => addNode("cond", { kind: "nodeHasHw" })}>
                  HW 가속 지원
                  <span>코덱 + GPU/CPU 구분 — 예: “av1을 GPU 가속으로 하는 노드가 있으면”</span>
                </button>
                <div className="gpal-sec">⚙ 액션</div>
                <button className="gpal-item" onClick={() => addNode("act", { kind: "encode" })}>
                  인코딩 시작
                  <span>코덱 · CRF 또는 원본 %</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("act", { kind: "healthcheck" })}>
                  헬스체크
                  <span>노드 온라인 시만</span>
                </button>
                <button className="gpal-item" onClick={() => addNode("act", { kind: "skip" })}>
                  건너뛰기
                  <span>인코딩하지 않음</span>
                </button>
              </aside>
              <div
                className="gcanvas"
                ref={canvasRef}
                onMouseDown={(e) => onCanvasDown(e)}
              >
                <div
                  className="gworld"
                  style={{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }}
                >
                  <svg className="gwires" width="10" height="10" style={{ overflow: "visible" }}>
                    {graph.edges.map((e, i) => {
                      const a = graph.nodes.find((n) => n.id === e.from);
                      const b = graph.nodes.find((n) => n.id === e.to);
                      if (!a || !b) return null;
                      const [x1, y1] = [a.x + NODE_W, e.fromPort === "false" ? a.y + 33 : a.y + 15];
                      const [x2, y2] = [b.x, b.y + 15];
                      const d = wirePath(x1, y1, x2, y2);
                      const cls = e.fromPort === "false" ? "false" : e.fromPort === "true" ? "true" : "base";
                      return (
                        <g key={i}>
                          <path d={d} className={`gwire ${cls}`} />
                          <path d={d} className="ghit" onClick={() => removeEdgeAt(i)} />
                        </g>
                      );
                    })}
                    {pending &&
                      (() => {
                        const a = graph.nodes.find((n) => n.id === pending.from);
                        if (!a) return null;
                        const x1 = a.x + NODE_W;
                        const y1 = pending.port === "false" ? a.y + 33 : a.y + 15;
                        return (
                          <path
                            d={wirePath(x1, y1, pending.x, pending.y)}
                            className="gwire tmp"
                          />
                        );
                      })()}
                  </svg>
                  {graph.nodes.map((n) => {
                    const isCond = n.kind === "condition";
                    const isAct = n.kind === "action";
                    const mode = n.action?.target?.mode || "pct";
                    return (
                      <div
                        key={n.id}
                        className={`gnode kind-${n.kind}${dragId === n.id ? " dragging" : ""}`}
                        style={{ left: n.x, top: n.y }}
                      >
                        <div className="ghead" onMouseDown={(e) => onNodeDown(e, n)}>
                          <span
                            className={
                              "gchip " +
                              (n.kind === "start" ? "c-start" : isCond ? "c-cond" : "c-act")
                            }
                          >
                            {n.kind === "start" ? "시작" : isCond ? "조건" : "액션"}
                          </span>
                          <span
                            className="gtitle"
                            title={n.label ? "더블클릭: 이름 변경" : "더블클릭: 이름 붙이기"}
                            onDoubleClick={() => {
                              const t = prompt("노드 이름 (비우면 자동 이름)", n.label || "");
                              if (t !== null) patchNode(n.id, { label: t.trim() || undefined });
                            }}
                          >
                            {nodeTitle(n)}
                          </span>
                          <span className="gfill" />
                          <label
                            className="gchk"
                            title="활성/비활성"
                            onMouseDown={(e) => e.stopPropagation()}
                          >
                            <input
                              type="checkbox"
                              checked={!!n.enabled}
                              onChange={() => patchNode(n.id, { enabled: !n.enabled })}
                            />
                          </label>
                          {n.kind !== "start" && (
                            <button
                              className="gdel"
                              title="노드 삭제"
                              onMouseDown={(e) => e.stopPropagation()}
                              onClick={() => removeNode(n.id)}
                            >
                              ✕
                            </button>
                          )}
                        </div>
                        <div className="gbody">
                          {n.kind === "start" && (
                            <div className="gsub">스캔에서 새 영상이 발견되면</div>
                          )}
                          {isCond && (
                            <>
                              <select
                                value={n.condition?.kind || "always"}
                                onChange={(e) => patchCond(n.id, { kind: e.target.value })}
                              >
                                <option value="always">항상</option>
                                <option value="srcCodec">원본 코덱 =</option>
                                <option value="sizeGt">파일 크기 &gt;</option>
                                <option value="sizeLt">파일 크기 &lt;</option>
                                <option value="bitrateGt">원본 비트레이트 &gt;</option>
                                <option value="bitrateLt">원본 비트레이트 &lt;</option>
                                <option value="nameContains">이름에 포함</option>
                                <option value="nodeHasHw">노드가 HW 가속 지원</option>
                              </select>
                              {n.condition?.kind === "srcCodec" && (
                                <select
                                  value={n.condition?.codec || ""}
                                  onChange={(e) =>
                                    patchCond(n.id, { codec: e.target.value || null })
                                  }
                                >
                                  <option value="">(어떤 코덱이든)</option>
                                  {(codecs?.videoCodecs || [])
                                    .filter((c) => c.id !== "copy")
                                    .map((c) => (
                                      <option key={c.id} value={c.id}>
                                        {c.label}
                                      </option>
                                    ))}
                                </select>
                              )}
                              {(n.condition?.kind === "sizeGt" ||
                                n.condition?.kind === "sizeLt") && (
                                <span className="lunit">
                                  <input
                                    type="number"
                                    className="num"
                                    min="0"
                                    value={n.condition?.value ?? ""}
                                    onChange={(e) =>
                                      patchCond(n.id, {
                                        value:
                                          e.target.value === "" ? null : Number(e.target.value),
                                      })
                                    }
                                  />
                                  MB
                                </span>
                              )}
                              {(n.condition?.kind === "bitrateGt" ||
                                n.condition?.kind === "bitrateLt") && (
                                <span className="lunit">
                                  <input
                                    type="number"
                                    className="num"
                                    min="0"
                                    value={n.condition?.value ?? ""}
                                    onChange={(e) =>
                                      patchCond(n.id, {
                                        value:
                                          e.target.value === "" ? null : Number(e.target.value),
                                      })
                                    }
                                  />
                                  Mbps
                                </span>
                              )}
                              {n.condition?.kind === "nameContains" && (
                                <input
                                  type="text"
                                  className="txt"
                                  placeholder="예: 4K, raw, .mkv"
                                  value={n.condition?.pattern || ""}
                                  onChange={(e) =>
                                    patchCond(n.id, { pattern: e.target.value || null })
                                  }
                                />
                              )}
                              {n.condition?.kind === "nodeHasHw" && (
                                <>
                                  <select
                                    value={n.condition?.hwFamily || "av1"}
                                    onChange={(e) =>
                                      patchCond(n.id, { hwFamily: e.target.value })
                                    }
                                  >
                                    {["h264", "h265", "av1", "vp9"].map((fam) => (
                                      <option key={fam} value={fam}>
                                        {fam}
                                      </option>
                                    ))}
                                  </select>
                                  <select
                                    value={n.condition?.accelClass || ""}
                                    onChange={(e) =>
                                      patchCond(n.id, {
                                        accelClass: e.target.value || null,
                                      })
                                    }
                                  >
                                    <option value="">가속기: 어떤 것이든</option>
                                    <option value="gpu">GPU 전용 (nvenc/videotoolbox)</option>
                                    <option value="cpu">iGPU 전용 (qsv/vaapi)</option>
                                  </select>
                                </>
                              )}
                            </>
                          )}
                          {isAct && (
                            <>
                              <select
                                value={n.action?.kind || "encode"}
                                onChange={(e) => patchAct(n.id, { kind: e.target.value })}
                              >
                                <option value="encode">인코딩 시작</option>
                                <option value="healthcheck">헬스체크 (노드 온라인 시만)</option>
                                <option value="skip">건너뛰기</option>
                              </select>
                              {n.action?.kind === "encode" && (
                                <>
                                  <label>
                                    비디오
                                    <select
                                      value={n.action?.target?.videoCodec || ""}
                                      onChange={(e) =>
                                        patchActTarget(n.id, {
                                          videoCodec: e.target.value || null,
                                        })
                                      }
                                    >
                                      <option value="">폴더 기본</option>
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
                                    오디오
                                    <select
                                      value={n.action?.target?.audioCodec || ""}
                                      onChange={(e) =>
                                        patchActTarget(n.id, {
                                          audioCodec: e.target.value || null,
                                        })
                                      }
                                    >
                                      <option value="">원본 유지</option>
                                      <option value="aac">AAC</option>
                                      <option value="opus">Opus</option>
                                    </select>
                                  </label>
                                  <div className="rateval">
                                    <div className="ratemode">
                                      <button
                                        type="button"
                                        className={`rm-opt ${mode === "crf" ? "on" : ""}`}
                                        onClick={() => patchActTarget(n.id, { mode: "crf" })}
                                      >
                                        CRF
                                      </button>
                                      <button
                                        type="button"
                                        className={`rm-opt ${mode !== "crf" ? "on" : ""}`}
                                        onClick={() => patchActTarget(n.id, { mode: "pct" })}
                                      >
                                        %
                                      </button>
                                    </div>
                                    <input
                                      type="number"
                                      className="num"
                                      min={mode === "crf" ? 0 : 10}
                                      max={mode === "crf" ? 51 : 100}
                                      value={
                                        mode === "crf"
                                          ? n.action?.target?.crf ?? 23
                                          : n.action?.target?.bitratePct ?? 70
                                      }
                                      onChange={(e) =>
                                        patchActTarget(
                                          n.id,
                                          mode === "crf"
                                            ? { crf: Number(e.target.value) }
                                            : { bitratePct: Number(e.target.value) }
                                        )
                                      }
                                    />
                                  </div>
                                </>
                              )}
                            </>
                          )}
                        </div>
                        {n.kind !== "start" && (
                          <div className="gsock in" data-node-in={n.id} title="입력 소켓" />
                        )}
                        {n.kind === "start" && (
                          <div
                            className="gsock out"
                            style={{ top: 9 }}
                            onMouseDown={(e) => onSocketDown(e, n, "")}
                            title="출력 — 새 영상"
                          />
                        )}
                        {isCond && (
                          <>
                            <div
                              className="gsock out t"
                              style={{ top: 9 }}
                              onMouseDown={(e) => onSocketDown(e, n, "true")}
                              title="✓ 조건이 맞으면"
                            />
                            <div
                              className="gsock out f"
                              style={{ top: 27 }}
                              onMouseDown={(e) => onSocketDown(e, n, "false")}
                              title="✗ 조건이 안 맞으면"
                            />
                            <div className="glab t" style={{ top: 9 }}>✓</div>
                            <div className="glab f" style={{ top: 27 }}>✗</div>
                          </>
                        )}
                      </div>
                    );
                  })}
                </div>
                <div className="gzoom">
                  <button onClick={() => zoomBy(1 / 1.2)} title="축소">−</button>
                  <span>{Math.round(view.k * 100)}%</span>
                  <button onClick={() => zoomBy(1.2)} title="확대">+</button>
                  <button onClick={() => setView({ x: 20, y: 10, k: 1 })} title="100%로 재설정">
                    ⟲
                  </button>
                </div>
              </div>
            </div>
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
