import React, { useEffect, useRef, useState } from "react";
import { api, fmtSize, fmtDur, fmtRate, setAuthToken } from "./api.js";
import { t, setLang } from "./i18n.js";
const STATUS_LABEL = {
  queued: "Queued",
  dispatched: "Dispatched",
  running: "Transcoding…",
  done: "Done",
  error: "Error",
  notvideo: t("\uBE44\uC601\uC0C1")
};

// Map an HW encoder name (h264_nvenc, hevc_vaapi, av1_qsv, …) to a short
// family label + a color class, so a node's accelerators render as colored
// chips instead of a raw list.
const CODEC_CHIPS = {
  h264: {
    label: "h264",
    cls: "c-h264"
  },
  h265: {
    label: "hevc",
    cls: "c-h265"
  },
  vp9: {
    label: "vp9",
    cls: "c-vp9"
  },
  vp8: {
    label: "vp8",
    cls: "c-vp8"
  },
  av1: {
    label: "av1",
    cls: "c-av1"
  },
  mpeg4: {
    label: "mpeg4",
    cls: "c-mpeg4"
  }
};
function chipFor(enc) {
  const e = enc.toLowerCase();
  let fam;
  if (e.includes("h264") || e.includes("x264") || e.includes("avc")) fam = "h264";else if (e.includes("hevc") || e.includes("h265") || e.includes("x265")) fam = "h265";else if (e.includes("vp9")) fam = "vp9";else if (e.includes("vp8")) fam = "vp8";else if (e.includes("av1")) fam = "av1";else if (e.includes("mpeg4")) fam = "mpeg4";
  return fam && CODEC_CHIPS[fam] || {
    label: enc,
    cls: "c-other"
  };
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
  const live = blocks.filter(b => b.enabled);
  const nodes = [{
    id: "start",
    kind: "start",
    enabled: true,
    x: 40,
    y: 160
  }];
  const edges = [];
  let prevFalse = null;
  live.forEach((b, i) => {
    const cid = `c${i + 1}`;
    const aid = `a${i + 1}`;
    nodes.push({
      id: cid,
      kind: "condition",
      enabled: true,
      condition: b.condition || {},
      x: 340,
      y: 30 + i * 200
    });
    nodes.push({
      id: aid,
      kind: "action",
      enabled: true,
      action: b.action || {},
      x: 660,
      y: 30 + i * 200
    });
    // start→c1 is portless; cN(✗)→cN+1 rides the false branch
    edges.push({
      from: prevFalse || "start",
      ...(prevFalse ? {
        fromPort: "false"
      } : {}),
      to: cid
    });
    edges.push({
      from: cid,
      fromPort: "true",
      to: aid
    });
    prevFalse = cid;
  });
  return {
    nodes,
    edges
  };
}
// Bézier path for a wire. When the target sits to the LEFT of the source
// (false-branch drop-down), bulge outward instead of looping back.
function wirePath(x1, y1, x2, y2) {
  let dx;
  if (x2 >= x1) dx = Math.max(40, Math.min(130, (x2 - x1) / 2));else dx = Math.min(180, Math.max(70, (x1 - x2) / 2 + 80));
  return `M ${x1} ${y1} C ${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`;
}

// Header title for a node — specific, not "조건"/"액션" only. Used when the
// user hasn't set a custom label.
function nodeTitle(n) {
  if (n.label) return n.label;
  const c = n.condition || {};
  const a = n.action || {};
  const codec = id => {
    if (!id) return "?";
    const map = {
      h264: "H.264",
      libx264: "H.264",
      h265: "HEVC",
      hevc: "HEVC",
      libx265: "HEVC",
      av1: "AV1",
      libsvtav1: "AV1",
      "libaom-av1": "AV1",
      vp9: "VP9",
      "libvpx-vp9": "VP9",
      vp8: "VP8",
      "libvpx": "VP8",
      mpeg4: "MPEG-4"
    };
    return map[id] || id.toUpperCase();
  };
  switch (n.kind) {
    case "start":
      return t("\uC0C8 \uC601\uC0C1 \uBC1C\uACAC");
    case "condition":
      switch (c.kind) {
        case "srcCodec":
          {
            const sel = c.codecs || (c.codec ? [c.codec] : []);
            return sel.length ? t("\uCF54\uB371 = ") + sel.map(codec).join(", ") : t("\uCF54\uB371 = \uC5B4\uB5A4 \uAC83\uC774\uB4E0");
          }
        case "sizeGt":
          return t("\uD06C\uAE30 > ") + (c.value ?? "∅") + " MB";
        case "sizeLt":
          return t("\uD06C\uAE30 < ") + (c.value ?? "∅") + " MB";
        case "heightGte":
          {
            const v = c.value ?? "∅";
            const tag = v >= 2160 ? " (4K+)" : v >= 1440 ? " (QHD+)" : v >= 1080 ? " (FHD+)" : v >= 720 ? " (HD+)" : "";
            return t("\uC138\uB85C \u2265 ") + v + "px" + tag;
          }
        case "bitrateGt":
          return t("\uBE44\uD2B8\uB808\uC774\uD2B8 > ") + (c.value ?? "∅") + " Mbps";
        case "bitrateLt":
          return t("\uBE44\uD2B8\uB808\uC774\uD2B8 < ") + (c.value ?? "∅") + " Mbps";
        case "nameContains":
          return t("\uC774\uB984 '") + (c.pattern || "∅") + t("' \uD3EC\uD568");
        case "srcHealth":
          {
            const m = c.healthMode || "front";
            if (m === "all") return t("\uD5EC\uC2A4\uCCB4\uD06C \xB7 \uC804\uCCB4 (\uAE34 \uC601\uC0C1\uC740 \uC624\uB798 \uAC78\uB9BC)");
            if (m === "random") return t("\uD5EC\uC2A4\uCCB4\uD06C \xB7 \uBB34\uC791\uC704 ") + (c.healthValue ?? 10) + t("% (\uC55E+\uB79C\uB364, \uCD5C\uB300 15\uCD08)");
            return t("\uD5EC\uC2A4\uCCB4\uD06C \xB7 \uC55E\uBD80\uBD84 ") + (c.healthValue ?? 10) + t("% (\uCD5C\uB300 15\uCD08)");
          }
        case "nodeHasHw":
          {
            const cls = c.accelClass === "gpu" ? "GPU " : c.accelClass === "cpu" ? "iGPU " : "";
            return t("\uB178\uB4DC: ") + cls + (c.hwFamily || "?") + t(" HW \uC778\uCF54\uB354");
          }
        case "field":
          {
            const fld = {
              size_mb: t("\uD06C\uAE30(MB)"),
              size: t("\uD06C\uAE30(byte)"),
              duration_s: t("\uAE38\uC774(s)"),
              width: t("\uAC00\uB85C(px)"),
              height: t("\uC138\uB85C(px)"),
              fps: "FPS",
              bitrate: t("\uBE44\uD2B8\uB808\uC774\uD2B8(Mbps)"),
              video_bit_rate: t("\uC601\uC0C1\uBE44\uD2B8(Mbps)"),
              audio_bit_rate: t("\uC624\uB514\uC624\uBE44\uD2B8(Mbps)"),
              name: t("\uC774\uB984"),
              codec: t("\uCF54\uB371"),
              container: t("\uCEE8\uD14C\uC774\uB108"),
              pixel_fmt: t("\uD53D\uC140\uD3EC\uB9F7")
            }[c.field] || c.field || "?";
            const opm = {
              gt: ">",
              gte: "≥",
              lt: "<",
              lte: "≤",
              eq: "=",
              neq: "≠",
              contains: t("\uD3EC\uD568"),
              startsWith: t("\uC2DC\uC791"),
              endsWith: t("\uB05D")
            }[c.op] || c.op || "≥";
            const val = ["name", "codec", "container", "pixel_fmt"].includes(c.field) ? c.pattern || "∅" : c.value ?? "∅";
            return "🧩 " + fld + " " + opm + " " + val;
          }
        case "cmd":
          return "🧩 cmd · exit 0 = ✓";
        default:
          return t("\uD56D\uC0C1 \uD1B5\uACFC");
      }
    case "action":
      if (a.kind === "skip") return t("\uAC74\uB108\uB6F0\uAE30");
      if (a.kind === "failToFolder") return t("\uC2E4\uD328 \uCC98\uB9AC");
      if (a.kind === "healthcheck") return t("\uD5EC\uC2A4\uCCB4\uD06C");
      if (a.kind === "verify") {
        const m = a.verifyMode || "all";
        if (m === "pct") return t("\uCD9C\uB825 \uAC80\uC99D \xB7 ") + (a.verifyValue ?? 100) + t("% \uC0D8\uD50C");
        if (m === "every") return t("\uCD9C\uB825 \uAC80\uC99D \xB7 ") + (a.verifyValue ?? 1) + t("\uC5D0 1\uAC1C");
        return t("\uCD9C\uB825 \uAC80\uC99D (\uC804\uCCB4)");
      }
      if (a.kind === "replace") return t("\uC6D0\uBCF8 \uB300\uCE58");
      if (a.kind === "saveHere") return t("\uC6D0\uBCF8 \uC704\uCE58\uC5D0 \uC800\uC7A5");
      if (a.kind === "requeue") return t("\uD050\uB85C \uB418\uB3CC\uB9AC\uAE30");
      if (a.kind === "postCmd") return "⚙️ postCmd";
      const t = a.target || {};
      const chs = t.choices || [];
      if (chs.length) {
        const names = chs.map(c => (c.videoCodec ? codec(c.videoCodec) : "?") + (c.accel ? "/" + c.accel : ""));
        const m0 = chs[0].mode ?? t.mode;
        const prate = m0 === "crf" ? "CRF " + (chs[0].crf ?? t.crf ?? 23) : (chs[0].bitratePct ?? t.bitratePct ?? 70) + "%";
        return t("\uC778\uCF54\uB529 \u2192 ") + names.join(" → ") + " · " + prate + (a.hwOnly ? t(" \xB7 \u26A1HW \uC804\uC6A9") : "");
      }
      const rate = t.mode === "crf" ? "CRF " + (t.crf ?? 23) : (t.bitratePct ?? 70) + "%";
      return t("\uC778\uCF54\uB529 \u2192 ") + (t.videoCodec ? codec(t.videoCodec) : t("\uD3F4\uB354 \uAE30\uBCF8")) + " · " + rate + (a.hwOnly ? t(" \xB7 \u26A1HW \uC804\uC6A9") : "");
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
function HwChips({
  encs
}) {
  if (!encs || encs.length === 0) return <span className="cpu-only">CPU only</span>;
  return <span className="chips">
      <span className="chips-label">⚡ HW</span>
      {encs.map(e => {
      const c = chipFor(e);
      return <span key={e} className={`chip ${c.cls}`} title={e}>
            {c.label}
          </span>;
    })}
    </span>;
}

// Group a node's accelerator entries by physical device and render
// "GPU #0 · name" / "CPU #0 · name" with per-backend codec chips, so a
// box with a dGPU + iGPU (or several of either) is obvious at a glance.
function AccelPanel({
  accels,
  flat
}) {
  if (!accels || accels.length === 0) {
    return <div className="meta">
        <span>
          <HwChips encs={flat} />
        </span>
      </div>;
  }
  const groups = new Map();
  for (const a of accels) {
    const key = `${a.kind}-${a.device}`;
    if (!groups.has(key)) {
      groups.set(key, {
        kind: a.kind,
        device: a.device,
        name: a.name,
        backends: new Map()
      });
    }
    const g = groups.get(key);
    if (!g.backends.has(a.backend)) g.backends.set(a.backend, []);
    for (const e of a.encoders) {
      if (!g.backends.get(a.backend).includes(e)) g.backends.get(a.backend).push(e);
    }
  }
  return <div className="accels">
      {[...groups.values()].map(g => <div className="accel-group" key={`${g.kind}-${g.device}`}>
          <div className="accel-head">
            <span className="accel-kind">{g.kind === "gpu" ? "🖥" : "⚙️"}</span>
            <b>{g.kind.toUpperCase()} #{g.device}</b>
            <span className="accel-name" title={g.name}>{g.name}</span>
          </div>
          {[...g.backends.entries()].map(([backend, encs]) => <div className="accel-row" key={backend}>
              <span className="accel-backend">{backend}</span>
              <HwChips encs={encs} />
            </div>)}
        </div>)}
    </div>;
}
export default function App() {
  const [health, setHealth] = useState(null);
  const [codecs, setCodecs] = useState(null);

  // Server runs with CT_API_TOKEN → first 401 shows a token entry banner.
  const [authRequired, setAuthRequired] = useState(false);
  const [tokenDraft, setTokenDraft] = useState("");
  useEffect(() => {
    const on401 = () => setAuthRequired(true);
    window.addEventListener("ct-unauthorized", on401);
    return () => window.removeEventListener("ct-unauthorized", on401);
  }, []);

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
  const [nodeFilter, setNodeFilter] = useState({
    status: "online",
    backend: "",
    family: ""
  });
  // node search (server finds a node by the IP:port it printed)
  const [searchIp, setSearchIp] = useState("");
  const [searchPort, setSearchPort] = useState("8401");
  const [searchMsg, setSearchMsg] = useState("");
  const [discovered, setDiscovered] = useState([]);
  // storage library (server-scanned folders of videos)
  const [library, setLibrary] = useState({
    root: "",
    entries: []
  });
  const [scanning, setScanning] = useState(false);
  // registered folders (add/remove list) + per-folder encoding target
  const [folders, setFolders] = useState([]);
  const [folderMsg, setFolderMsg] = useState("");
  const [newFolder, setNewFolder] = useState("");
  // filesystem tree browser (folder picker) — "folder" = library folder add,
  // "faildir" = the settings tab's fail (quarantine) folder
  const [treeOpen, setTreeOpen] = useState(false);
  const [treeMode, setTreeMode] = useState("folder");
  const [tree, setTree] = useState(null); // {path, parent, dirs, files}
  const [treePath, setTreePath] = useState("/");
  const [treeJump, setTreeJump] = useState("");
  const [treeErr, setTreeErr] = useState("");
  // logic automation — named logics (saved list; the canvas edits one).
  // The library tab assigns a named logic (or none) to each folder.
  const [graph, setGraph] = useState({
    nodes: [],
    edges: []
  });
  const [blockMsg, setBlockMsg] = useState("");
  const [logics, setLogics] = useState([]);
  // id of the logic the canvas is editing ("" = a brand-new unsaved one)
  const [activeLogic, setActiveLogic] = useState("");
  const activeLogicRef = useRef("");
  const [logicName, setLogicName] = useState("");
  // Unsaved canvas edits (explicit 💾 저장 button — no auto-save, so a
  // careless drag can't clobber a saved logic).
  const [logicDirty, setLogicDirty] = useState(false);
  const logicDirtyRef = useRef(false);
  // node worker-settings modal (dashboard gear)
  const [nodeCfg, setNodeCfg] = useState(null);
  // server settings (settings tab) + fail quarantine (보관함 tab)
  const [failDirDraft, setFailDirDraft] = useState(null); // null = not loaded yet
  const [autoScanDraft, setAutoScanDraft] = useState(null); // null = not loaded yet
  const [settingsMsg, setSettingsMsg] = useState("");
  const [lang, setLangState] = useState("ko"); // "ko" | "en" — server setting
  useEffect(() => {
    setLang(lang);
  }, [lang]);
  const [failures, setFailures] = useState({
    failDir: null,
    items: []
  });
  const [failMsg, setFailMsg] = useState("");
  const [failSort, setFailSort] = useState({
    key: "movedAt",
    dir: -1
  });
  // library tab: per-library sheet selection + table sort
  const [libSel, setLibSel] = useState("all");
  // Clickable summary-chip filter: "remaining" | "done" | "active" |
  // "error" | "failed" (null = no filter).
  const [libFilter, setLibFilter] = useState(null);
  const pickLib = sel => {
    setLibSel(sel);
    setLibFilter(null);
  };
  const [libSort, setLibSort] = useState({
    key: "name",
    dir: 1
  });

  // jobs
  const [jobs, setJobs] = useState([]);
  const [activeJob, setActiveJob] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  // load catalog + health
  useEffect(() => {
    api.health().then(setHealth).catch(() => setHealth({
      status: "down"
    }));
    api.foldersGet().then(setFolders).catch(() => {});
    api.logicsGet().then(ls => {
      setLogics(ls);
      // Start on the first saved logic (or a blank canvas with a start node).
      // Pass `ls` explicitly: the `logics` state is still [] at this point,
      // and selectLogic's default arg would take the blank-canvas branch.
      selectLogic(ls.length ? ls[0].id : "", ls);
    }).catch(() => {});
    api.library().then(l => {
      setLibrary(l);
      if (l.root) setLibRoot(l.root);
    }).catch(() => {});
    api.settingsGet().then(s => {
      setFailDirDraft(s.failDir ?? "");
      setAutoScanDraft(s.autoScanMinutes ?? 0);
      // setLang BEFORE setLangState: the re-render reads the module var,
      // and a bare module mutation (useEffect) would never re-render.
      const l = s.language === "en" ? "en" : "ko";
      setLang(l);
      setLangState(l);
    }).catch(() => {});
    api.failuresGet().then(setFailures).catch(() => {});
    api.codecs().then(c => {
      setCodecs(c);
      if (c.videoCodecs?.length) setVideoCodec(c.videoCodecs[0].id);
      if (c.audioCodecs?.length) setAudioCodec(c.audioCodecs[0].id);
      if (c.containers?.length) setContainer(c.containers[0].id);
    }).catch(() => {});
  }, []);

  // refresh jobs + nodes periodically
  useEffect(() => {
    const t = setInterval(() => {
      api.jobs().then(setJobs).catch(() => {});
      api.nodes().then(setNodes).catch(() => {});
      api.discoveredNodes().then(setDiscovered).catch(() => {});
      api.library().then(setLibrary).catch(() => {});
      // Keep the folder list in sync when it changes in ANOTHER window or
      // session (a deleted folder must disappear from the sheets). Compare
      // signatures so an in-flight local edit is never clobbered: if local
      // state already differs from the server, keep local until the POST
      // lands (then both sides agree).
      api.foldersGet().then(f => {
        const sig = x => JSON.stringify(x.map(y => [y.id, y.path, y.enabled, y.logicId, y.target]));
        setFolders(cur => sig(f) === sig(cur) ? cur : f);
        setLibSel(cur => cur === "all" || cur === "skipped" || cur === "notvideo" || f.some(y => y.id === cur) ? cur : "all");
      }).catch(() => {});
      if (tab === "dashboard") api.serverStats().then(setStats).catch(() => {});
      // keep the active job's progress bar live until it's terminal
      const aj = activeJobRef.current;
      if (aj && !["done", "error"].includes(aj.status)) {
        api.job(aj.id).then(latest => {
          activeJobRef.current = latest;
          setActiveJob(latest);
        }).catch(() => {});
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
      const r = await api.upload(f, p => setUploadProgress(p));
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
      bitratePct: usePct ? bitratePct : undefined
    };
    try {
      let id;
      if (target === "server") {
        id = (await api.transcode(base)).id;
      } else if (target === "auto") {
        id = (await api.dispatch(base)).id; // server auto-picks node + accelerates
      } else {
        id = (await api.dispatch({
          ...base,
          nodeId: target
        })).id;
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
  const estKbps = analysis?.videoBitRate != null ? Math.max(100, Math.round(analysis.videoBitRate * bitratePct / 100 / 1000)) : null;
  const onlineNodes = nodes.filter(n => n.status === "online");
  const offlineNodes = nodes.filter(n => n.status !== "online");

  // Registry view: what's actually shown in the Nodes card.
  const shownNodes = nodes.filter(n => {
    if (nodeFilter.status !== "all" && n.status !== nodeFilter.status) return false;
    if (nodeFilter.backend && !n.accels?.some(a => a.backend === nodeFilter.backend)) return false;
    if (nodeFilter.family && !n.hwEncoders?.some(h => codecFamily(h) === nodeFilter.family)) return false;
    return true;
  });
  // Filter options derived from what the registry actually offers.
  const backendOptions = [...new Set(nodes.flatMap(n => n.accels?.map(a => a.backend) || []))].sort();
  const familyOptions = [...new Set(nodes.flatMap(n => n.hwEncoders?.map(codecFamily) || []))].sort();

  // "Server finds a node": probe the IP:port the node printed in its terminal.
  async function doSearch() {
    const ip = searchIp.trim();
    if (!ip) return;
    setError("");
    setSearchMsg("");
    try {
      const n = await api.searchNode(ip, searchPort);
      setNodes(await api.nodes());
      setSearchMsg(`Found '${n.name}' — ${n.hwEncoders?.length ? "⚡ " + n.hwEncoders.join(", ") : "CPU only"}`);
    } catch (err) {
      setError(err.message);
    }
  }

  // ── node worker-settings modal (dashboard gear) ─────────────
  const openNodeCfg = n => {
    const all = new Set((n.accels || []).map(a => a.backend));
    setNodeCfg({
      id: n.id,
      regName: n.name,
      name: n.displayName || "",
      accels: n.accels || [],
      // absent (undefined) = never configured = all backends checked;
      // an explicit list (possibly empty) = exactly those checked.
      backends: n.enabledBackends === undefined ? all : new Set(n.enabledBackends),
      allowSw: n.allowSoftware !== false,
      maxJobs: n.maxJobs || 1,
      msg: ""
    });
  };
  const patchNodeCfg = patch => setNodeCfg(c => c ? {
    ...c,
    ...patch
  } : c);
  const toggleNodeBackend = b => setNodeCfg(c => {
    if (!c) return c;
    const s = new Set(c.backends);
    if (s.has(b)) s.delete(b);else s.add(b);
    return {
      ...c,
      backends: s
    };
  });
  const nodeCfgBackends = n => {
    // one checkbox per backend the node actually reports, with the device names
    const m = new Map();
    for (const a of n.accels || []) {
      if (!m.has(a.backend)) m.set(a.backend, []);
      m.get(a.backend).push(`${a.name} (${a.kind === "gpu" ? "GPU" : "CPU"})`);
    }
    return [...m.entries()];
  };
  async function saveNodeCfg() {
    if (!nodeCfg) return;
    patchNodeCfg({
      msg: t("\uC800\uC7A5 \uC911\u2026")
    });
    try {
      const r = await api.nodeConfig(nodeCfg.id, {
        name: nodeCfg.name,
        enabledBackends: [...nodeCfg.backends],
        allowSoftware: nodeCfg.allowSw,
        maxJobs: nodeCfg.maxJobs
      });
      setNodes(ns => ns.map(x => x.id === nodeCfg.id ? {
        ...x,
        ...r.node
      } : x));
      setNodeCfg(null);
    } catch (err) {
      patchNodeCfg({
        msg: err.message
      });
    }
  }

  // Scan ALL registered folders (read-only; disabled folders are indexed
  // but never auto-queued); the server auto-queues newly found videos per
  // the saved automation (logic blocks first, then the folder default
  // target). With a folderId: scan only that registered folder (the
  // per-folder 스캔 button).
  const [scanProg, setScanProg] = useState(null); // live {active, scanned, total, scope}
  const [scanFolder, setScanFolder] = useState(""); // "" = whole-library scan
  const scanTimer = useRef(null);
  const stopScanPoll = () => {
    if (scanTimer.current) {
      clearInterval(scanTimer.current);
      scanTimer.current = null;
    }
  };
  useEffect(() => () => stopScanPoll(), []);
  async function doScan(folderId) {
    setScanning(true);
    setScanFolder(folderId || "");
    setError("");
    const poll = async () => {
      try {
        setScanProg(await api.scanProgress());
      } catch (e) {
        /* progress is best-effort */
      }
    };
    stopScanPoll();
    poll();
    scanTimer.current = setInterval(poll, 800);
    try {
      const l = await api.libraryScan(undefined, undefined, folderId);
      if (l.alreadyRunning) {
        // A scan is already running (the POST returned instantly). Keep
        // polling its live progress until it actually finishes.
        flashBlock(t("\uC2A4\uCE94\uC774 \uC774\uBBF8 \uC9C4\uD589 \uC911 \u2014 ") + (l.progress?.scanned ?? 0) + "/" + (l.progress?.total ?? "?") + t(" \uC644\uB8CC \uB300\uAE30"));
        const waitDone = setInterval(async () => {
          try {
            const p = await api.scanProgress();
            setScanProg(p);
            if (!p.active) {
              clearInterval(waitDone);
              stopScanPoll();
              setScanning(false);
              setScanFolder("");
            }
          } catch (e) {
            /* best-effort */
          }
        }, 1000);
        return;
      }
      setLibrary({
        root: "",
        entries: l.entries || []
      });
      flashBlock(l.queued?.length ? t("\uC2A4\uCE94 \uC644\uB8CC \u2014 ") + (l.scanned ?? "?") + t("\uAC1C \uD655\uC778, \uD050\uC789 ") + l.queued.length + t("\uAC74") : t("\uC2A4\uCE94 \uC644\uB8CC \u2014 ") + (l.scanned ?? "?") + t("\uAC1C \uD655\uC778"));
    } catch (err) {
      setError(err.message);
    } finally {
      stopScanPoll();
      setScanning(false);
      setScanFolder("");
    }
  }

  // ── fail quarantine (보관함) + settings ────────────────────
  const flashFail = m => {
    setFailMsg(m);
    setTimeout(() => setFailMsg(""), 3500);
  };
  async function loadFailures() {
    try {
      const d = await api.failuresGet();
      setFailures({
        failDir: d.failDir ?? null,
        items: d.items || []
      });
    } catch (err) {
      flashFail(err.message);
    }
  }
  async function saveFailDir() {
    try {
      const r = await api.settingsPost({
        failDir: failDirDraft || null
      });
      setFailDirDraft(r.failDir ?? "");
      setSettingsMsg(t("\uC138\uD305 \uC800\uC7A5\uB428 \u2713"));
      setTimeout(() => setSettingsMsg(""), 2500);
      loadFailures();
    } catch (err) {
      setSettingsMsg(err.message);
    }
  }
  async function restoreFailure(path) {
    try {
      await api.failureRestore(path);
      flashFail(t("\uC6D0\uBCF8 \uC704\uCE58\uB85C \uBCF5\uC6D0\uB428 \u2713"));
      await loadFailures();
      await doScan(); // the re-armed entry gets re-queued by the scan
    } catch (err) {
      flashFail(err.message);
    }
  }
  async function deleteFailure(path) {
    if (!window.confirm(t("\uBCF4\uAD00\uB41C \uD30C\uC77C\uC744 \uC0AD\uC81C\uD560\uAE4C\uC694? (\uC6D0\uBCF8 \uC704\uCE58\uB85C \uB3CC\uC544\uAC00\uC9C0 \uC54A\uC2B5\uB2C8\uB2E4.)"))) return;
    try {
      await api.failureDelete(path);
      flashFail(t("\uC0AD\uC81C\uB428"));
      await loadFailures();
    } catch (err) {
      flashFail(err.message);
    }
  }
  // refresh the 보관함 list whenever the tab is opened
  useEffect(() => {
    if (tab === "failures") loadFailures();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  // ── folders (add/remove list + per-folder target) ──────────────
  const uid = () => crypto.randomUUID ? crypto.randomUUID() : "id-" + Date.now() + "-" + Math.random().toString(36).slice(2);

  // ── library re-encode (per-file ⟳ button) ──────────────────────
  const libKey = e => (e.folder ? e.folder + "/" : "") + e.path;
  const libJob = e => jobs.find(j => j.libPath === libKey(e) && !["done", "error"].includes(j.status));
  async function doRequeue(e) {
    setError("");
    try {
      await api.requeue(libKey(e));
      await doScan(); // the scan re-queues the now-pending entry
    } catch (err) {
      setError(err.message);
    }
  }

  // ── library tab: per-library sheets + sortable table ──────────
  const libName = f => (f.path || "").split("/").filter(Boolean).pop() || f.path;
  const libEntries = f => library.entries.filter(e => e.folder === f.id);
  // Sheet count = ENCODING TARGETS: files that are neither skipped by the
  // logic/folder settings nor non-video (those can never be queued).
  const libEntriesTarget = es => es.filter(e => e.status !== "skipped" && e.status !== "notvideo").length;
  const selFolder = libSel === "all" ? null : folders.find(f => f.id === libSel);
  // Entries of the selected sheet (chip filter NOT applied) — the summary
  // chip counts come from this, so filtering the table doesn't change them.
  const sheetBase = libSel === "skipped" ? library.entries.filter(e => e.status === "skipped") : libSel === "notvideo" ? library.entries.filter(e => e.status === "notvideo") : libSel === "all" || !selFolder ? library.entries : library.entries.filter(e => e.folder === libSel);
  // Clickable summary-chip filter (state: libFilter).
  const libFilterMatch = e => {
    if (!libFilter) return true;
    switch (libFilter) {
      case "remaining":
        // not finished, and not in a never-queued state
        return e.status !== "done" && e.status !== "skipped" && e.status !== "notvideo";
      case "done":
        return e.status === "done";
      case "active":
        return !!libJob(e);
      case "error":
        return e.status === "error";
      case "failed":
        return e.status === "failed";
      default:
        return true;
    }
  };
  const visibleEntries = (() => {
    // a sheet for a removed folder falls back to "all"
    const base = libFilter ? sheetBase.filter(libFilterMatch) : sheetBase;
    const {
      key,
      dir
    } = libSort;
    const val = e => {
      switch (key) {
        case "name":
          return e.name || "";
        case "folder":
          return libSel === "all" ? e.folder ? libName(folders.find(f => f.id === e.folder) || {}) : t("(\uC784\uC2DC)") : "";
        case "codec":
          return e.videoCodec || "";
        case "size":
          return e.size || 0;
        case "duration":
          return e.duration || 0;
        case "status":
          return e.status || "";
        default:
          return e.name || "";
      }
    };
    return [...base].sort((a, b) => {
      const va = val(a);
      const vb = val(b);
      if (typeof va === "number" && typeof vb === "number") return (va - vb) * dir;
      return String(va).localeCompare(String(vb), "ko") * dir;
    });
  })();
  const toggleLibSort = key => setLibSort(s => s.key === key ? {
    key,
    dir: -s.dir
  } : {
    key,
    dir: 1
  });
  const sortArrow = key => libSort.key === key ? libSort.dir === 1 ? " ↑" : " ↓" : "";
  const Th = ({
    k,
    className,
    children
  }) => <span className={`libth ${className || ""}`} onClick={() => toggleLibSort(k)} title={t("\uD074\uB9AD: \uC624\uB984/\uB0B4\uB9BC \uC815\uB82C")}>
      {children}
      {sortArrow(k)}
    </span>;
  const flash = set => m => {
    set(m);
    setTimeout(() => set(""), 2500);
  };
  const flashFolder = flash(setFolderMsg);
  const flashBlock = flash(setBlockMsg);
  async function saveFolders(next) {
    setFolders(next);
    try {
      await api.foldersPost(next);
      // The server deletes removed folders' catalog entries with the
      // folder — refetch so the sheet list drops them immediately.
      const l = await api.library();
      setLibrary(l);
      if (l.root) setLibRoot(l.root);
      if (libSel !== "all" && libSel !== "skipped" && libSel !== "notvideo" && !next.some(f => f.id === libSel)) setLibSel("all");
      flashFolder(t("\uD3F4\uB354 \uC800\uC7A5\uB428 \u2713"));
    } catch (err) {
      flashFolder(err.message);
    }
  }
  function addFolder() {
    const p = newFolder.trim();
    if (!p || folders.some(f => f.path === p)) return;
    // New libraries start INACTIVE — the user must flip the 활성화 button,
    // so a half-configured folder never starts auto-queueing videos.
    saveFolders([...folders, {
      id: uid(),
      path: p,
      enabled: false,
      target: {
        videoCodec: "h265",
        audioCodec: "aac",
        container: "mp4",
        mode: "pct",
        bitratePct: 70
      }
    }]);
    setNewFolder("");
    flashFolder(t("\uCD94\uAC00\uB428 \u2014 \uBE44\uD65C\uC131 \uC0C1\uD0DC. \uC124\uC815 \uD6C4 \uD589\uC758 [\uD65C\uC131\uD654] \uBC84\uD2BC\uC73C\uB85C \uCF1C\uC138\uC694"));
  }
  // ── filesystem tree browser ────────────────────────────────
  async function loadTree(path) {
    try {
      const d = await api.fsChildren(path);
      setTree(d);
      setTreePath(d.path);
      setTreeErr("");
    } catch (err) {
      setTreeErr(err.message || t("\uBD88\uB7EC\uC624\uAE30 \uC2E4\uD328"));
    }
  }
  function openTree(mode = "folder") {
    setTreeMode(mode);
    setTreeOpen(true);
    setTreeJump("");
    loadTree("/");
  }
  function treeNav(path) {
    loadTree(path);
  }
  function treeJumpGo() {
    const p = treeJump.trim();
    if (!p) return;
    loadTree(p.startsWith("/") ? p : "/" + p);
  }
  function treeUp() {
    if (tree?.parent && tree.parent !== treePath) loadTree(tree.parent);
  }
  function addTreeFolder() {
    const p = treePath;
    if (!p || folders.some(f => f.path === p)) return;
    // Same as addFolder: new libraries start inactive.
    saveFolders([...folders, {
      id: uid(),
      path: p,
      enabled: false,
      target: {
        videoCodec: "h265",
        audioCodec: "aac",
        container: "mp4",
        mode: "pct",
        bitratePct: 70
      }
    }]);
    setTreeOpen(false);
    flashFolder(t("\uCD94\uAC00\uB428 \u2014 \uBE44\uD65C\uC131 \uC0C1\uD0DC. \uC124\uC815 \uD6C4 \uD589\uC758 [\uD65C\uC131\uD654] \uBC84\uD2BC\uC73C\uB85C \uCF1C\uC138\uC694"));
  }
  const treeCrumbs = p => {
    const parts = p.split("/").filter(Boolean);
    const out = [{
      label: "/",
      path: "/"
    }];
    let acc = "";
    for (const part of parts) {
      acc += "/" + part;
      out.push({
        label: part,
        path: acc
      });
    }
    return out;
  };
  const patchFolder = (id, fn) => saveFolders(folders.map(f => f.id === id ? fn(f) : f));
  const patchTarget = (id, patch) => patchFolder(id, f => ({
    ...f,
    target: {
      ...f.target,
      ...patch
    }
  }));
  const removeFolder = id => saveFolders(folders.filter(f => f.id !== id));
  const folderPath = id => {
    if (!id) return t("(\uC784\uC2DC \uB8E8\uD2B8)");
    const f = folders.find(x => x.id === id);
    return f ? f.path : t("\uC81C\uAC70\uB428");
  };
  const shortPath = p => {
    const parts = p.split("/").filter(Boolean);
    return parts.slice(-2).join("/") || p;
  };

  // ── logic node graph (ComfyUI style) ────────────────────────
  const graphRef = useRef(graph);
  useEffect(() => {
    graphRef.current = graph;
  }, [graph]);
  // Warn before leaving the page with unsaved logic edits.
  useEffect(() => {
    const h = e => {
      if (logicDirtyRef.current) {
        e.preventDefault();
        e.returnValue = "";
      }
    };
    window.addEventListener("beforeunload", h);
    return () => window.removeEventListener("beforeunload", h);
  }, []);
  // Save the canvas as a NAMED logic (create when the id is still empty).
  // Edits are STAGED ONLY — nothing hits the server until the 💾 저장
  // button is pressed (accidental drags/changes stay local).
  // Name/graph are read from REFS: selectLogic mutates the refs synchronously
  // while setLogicName/setGraph are still async — a stale render closure must
  // never clobber a just-loaded logic with the blank-canvas defaults.
  const logicNameRef = useRef("");
  const commitGraph = g => {
    setGraph(g);
    graphRef.current = g;
    logicDirtyRef.current = true;
    setLogicDirty(true);
  };
  const saveLogic = () => {
    const id = activeLogicRef.current;
    const name = (logicNameRef.current || logicName || t("\uC0C8 \uB85C\uC9C1")).trim();
    if (!name) {
      flashBlock(t("\uB85C\uC9C1 \uC774\uB984\uC744 \uBA3C\uC800 \uC785\uB825\uD558\uC138\uC694"));
      return;
    }
    const g = graphRef.current;
    api.logicsPost(id ? {
      id,
      name,
      graph: g
    } : {
      name,
      graph: g
    }).then(r => {
      if (!id && r.logic) {
        activeLogicRef.current = r.logic.id;
        setActiveLogic(r.logic.id);
      }
      if (r.logics) setLogics(r.logics);
      logicDirtyRef.current = false;
      setLogicDirty(false);
      flashBlock(id ? t("\uB85C\uC9C1 \uC800\uC7A5\uB428") : t("\uC0C8 \uB85C\uC9C1\uC73C\uB85C \uC800\uC7A5\uB428"));
    }).catch(err => flashBlock(err.message));
  };
  const ensureStart = g => g.nodes.some(n => n.kind === "start") ? g : {
    nodes: [{
      id: "start",
      kind: "start",
      enabled: true,
      x: 40,
      y: 160
    }, ...g.nodes],
    edges: g.edges
  };
  // Switch the canvas to a saved named logic ("" = new blank one).
  // Unsaved edits are discarded on switch — guarded by a confirm.
  const selectLogic = (id, list = logics) => {
    if (logicDirtyRef.current && id !== activeLogicRef.current) {
      const ok = window.confirm(t("\uC800\uC7A5\uB418\uC9C0 \uC54A\uC740 \uBCC0\uACBD\uC774 \uC788\uC2B5\uB2C8\uB2E4.\n\uB2E4\uB978 \uB85C\uC9C1\uC73C\uB85C \uC804\uD658\uD558\uBA74 \uC774 \uBCC0\uACBD\uC740 \uC0AC\uB77C\uC9D1\uB2C8\uB2E4. \uACC4\uC18D\uD560\uAE4C\uC694?"));
      if (!ok) return;
    }
    activeLogicRef.current = id;
    setActiveLogic(id);
    logicDirtyRef.current = false;
    setLogicDirty(false);
    const l = list.find(x => x.id === id);
    let g;
    if (l) {
      g = ensureStart(l.graph && l.graph.nodes?.length ? l.graph : {
        nodes: [],
        edges: []
      });
      logicNameRef.current = l.name;
    } else {
      g = {
        nodes: [{
          id: "start",
          kind: "start",
          enabled: true,
          x: 40,
          y: 160
        }],
        edges: []
      };
      logicNameRef.current = t("\uC0C8 \uB85C\uC9C1 ") + (list.length + 1);
    }
    graphRef.current = g; // sync BEFORE any stale-closure commit can run
    setLogicName(logicNameRef.current);
    setGraph(g);
  };
  const patchLogicName = name => {
    logicNameRef.current = name;
    setLogicName(name);
    logicDirtyRef.current = true;
    setLogicDirty(true);
  };
  const deleteLogic = () => {
    const id = activeLogicRef.current;
    if (!id) return;
    if (!window.confirm(t("\uC774 \uB85C\uC9C1\uC744 \uC0AD\uC81C\uD560\uAE4C\uC694? (\uC774 \uB85C\uC9C1\uC744 \uC4F0\uB358 \uD3F4\uB354\uB294 \uC77C\uAD04 \uC801\uC6A9\uC73C\uB85C \uB3CC\uC544\uAC11\uB2C8\uB2E4)"))) return;
    api.logicDelete(id).then(r => {
      const rest = r.logics || [];
      setLogics(rest);
      selectLogic(rest.length ? rest[0].id : "", rest);
      api.foldersGet().then(setFolders).catch(() => {});
    }).catch(e => flashBlock(e.message));
  };
  const patchNode = (id, patch) => {
    const g = graphRef.current;
    commitGraph({
      ...g,
      nodes: g.nodes.map(n => n.id === id ? {
        ...n,
        ...patch
      } : n)
    });
  };
  const patchCond = (id, patch) => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    patchNode(id, {
      condition: {
        ...(n?.condition || {}),
        ...patch
      }
    });
  };
  const patchAct = (id, patch) => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    patchNode(id, {
      action: {
        ...(n?.action || {}),
        ...patch
      }
    });
  };
  const patchActTarget = (id, patch) => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    patchNode(id, {
      action: {
        ...(n?.action || {}),
        target: {
          ...(n?.action?.target || {}),
          ...patch
        }
      }
    });
  };
  // ── encoder-preference list (choices) on an encode node ──────────
  const patchChoice = (id, idx, patch) => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    const cur = n?.action?.target?.choices || [];
    patchActTarget(id, {
      choices: cur.map((c, i) => i === idx ? {
        ...c,
        ...patch
      } : c)
    });
  };
  const patchChoiceAdd = id => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    const t = n?.action?.target || {};
    const cur = t.choices || [];
    const last = cur[cur.length - 1] || {};
    patchActTarget(id, {
      choices: [...cur, {
        videoCodec: last.videoCodec || "h265",
        accel: null,
        mode: last.mode || t.mode || "pct",
        crf: last.crf ?? t.crf,
        bitratePct: last.bitratePct ?? t.bitratePct
      }]
    });
  };
  const patchChoiceDel = (id, idx) => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    const cur = n?.action?.target?.choices || [];
    patchActTarget(id, {
      choices: cur.filter((_, i) => i !== idx)
    });
  };
  const enablePriority = id => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    const t = n?.action?.target || {};
    patchActTarget(id, {
      choices: [{
        videoCodec: t.videoCodec || "h265",
        accel: null,
        mode: t.mode || "pct",
        crf: t.crf,
        bitratePct: t.bitratePct
      }]
    });
  };
  const patchAllChoiceRate = (id, patch) => {
    const n = graphRef.current.nodes.find(n => n.id === id);
    const cur = n?.action?.target?.choices || [];
    patchActTarget(id, {
      choices: cur.map(c => ({
        ...c,
        ...patch
      }))
    });
  };
  const addNode = (kind, preset = {}, pos = null) => {
    const g = graphRef.current;
    if (kind === "start" && g.nodes.some(n => n.kind === "start")) return;
    const id = `n${Date.now().toString(36)}${Math.floor(Math.random() * 999)}`;
    const base = {
      id,
      enabled: true,
      // explicit drop position wins; otherwise a cascading default slot
      x: pos ? pos.x : 380 + g.nodes.length % 4 * 48,
      y: pos ? pos.y : 50 + g.nodes.length % 6 * 44
    };
    let node;
    if (kind === "start") node = {
      ...base,
      kind: "start"
    };else if (kind === "cond") node = {
      ...base,
      kind: "condition",
      condition: preset.kind === "nodeHasHw" ? {
        kind: "nodeHasHw",
        hwFamily: "av1"
      } : preset.kind === "srcHealth" ? {
        kind: "srcHealth",
        healthMode: "front",
        healthValue: 10
      } : preset.kind === "field" ? {
        kind: "field",
        field: "size",
        op: "gte",
        value: 0
      } : preset.kind === "cmd" ? {
        kind: "cmd",
        cmd: "",
        cmdTimeout: 30
      } : {
        kind: preset.kind || "always"
      }
    };else if (kind === "act") node = {
      ...base,
      kind: "action",
      action: preset.kind === "encode" ? {
        kind: "encode",
        target: {
          videoCodec: "av1",
          container: "mp4",
          mode: "pct",
          bitratePct: 70
        }
      } : preset.kind === "verify" ? {
        kind: "verify",
        verifyMode: "all"
      } : preset.kind === "postCmd" ? {
        kind: "postCmd",
        cmd: "",
        cmdTimeout: 30
      } : {
        kind: preset.kind || "skip"
      }
    };else return;
    commitGraph({
      ...g,
      nodes: [...g.nodes, node]
    });
  };
  const removeNode = id => {
    if (id === "start") return;
    const g = graphRef.current;
    commitGraph({
      nodes: g.nodes.filter(n => n.id !== id),
      edges: g.edges.filter(e => e.from !== id && e.to !== id)
    });
  };
  const removeEdgeAt = i => {
    const g = graphRef.current;
    commitGraph({
      ...g,
      edges: g.edges.filter((_, j) => j !== i)
    });
  };
  // Palette → canvas drag & drop. HTML5 DnD: the payload travels in
  // dataTransfer (readable only on drop), and the drop point is converted
  // from canvas-viewport px to world coordinates (pan + zoom aware).
  const palDragStart = (e, kind, preset) => {
    e.dataTransfer.setData("application/x-ct-node", JSON.stringify({
      kind,
      preset
    }));
    e.dataTransfer.effectAllowed = "copy";
  };
  const Pal = ({
    kind,
    preset,
    disabled,
    children
  }) => <button className="gpal-item" draggable={!disabled} disabled={disabled} onClick={() => addNode(kind, preset)} onDragStart={e => palDragStart(e, kind, preset)} title={t("\uD074\uB9AD = \uAE30\uBCF8 \uC704\uCE58 \uCD94\uAC00 \xB7 \uCE94\uBC84\uC2A4\uB85C \uB4DC\uB798\uADF8 = \uB193\uC740 \uC704\uCE58\uC5D0 \uCD94\uAC00")}>
      {children}
    </button>;
  const onCanvasDrop = e => {
    e.preventDefault();
    const raw = e.dataTransfer.getData("application/x-ct-node");
    if (!raw) return;
    let payload;
    try {
      payload = JSON.parse(raw);
    } catch {
      return;
    }
    const rect = canvasRef.current.getBoundingClientRect();
    const x = (e.clientX - rect.left - view.x) / view.k;
    const y = (e.clientY - rect.top - view.y) / view.k;
    addNode(payload.kind, payload.preset || {}, {
      x: Math.round(x - 60),
      y: Math.round(y - 12)
    });
  };

  // canvas view: pan (x,y) + zoom (k) applied to the .gworld wrapper
  const [view, setView] = useState({
    x: 20,
    y: 10,
    k: 1
  });
  const viewRef = useRef(view);
  useEffect(() => {
    viewRef.current = view;
  }, [view]);
  const zoomBy = factor => {
    const v = viewRef.current;
    const k = Math.max(0.4, Math.min(2.5, v.k * factor));
    // zoom around the canvas center
    const rect = canvasRef.current?.getBoundingClientRect();
    const cx = rect ? rect.width / 2 : 400;
    const cy = rect ? rect.height / 2 : 300;
    setView({
      x: cx - (cx - v.x) / v.k * k,
      y: cy - (cy - v.y) / v.k * k,
      k
    });
  };

  // node drag + wire creation + canvas pan (window-level listeners;
  // ref-driven so the mount-only effect always sees fresh state)
  const canvasRef = useRef(null);
  const dragRef = useRef(null); // { id, ox, oy } — offsets in WORLD coords
  const pendingRef = useRef(null); // { from, port }
  const panRef = useRef(null); // { sx, sy, vx, vy }
  const [dragId, setDragId] = useState(null);
  const [pending, setPending] = useState(null); // { from, port, x, y } (world)

  const toWorld = e => {
    const rect = canvasRef.current.getBoundingClientRect();
    const v = viewRef.current;
    return {
      x: (e.clientX - rect.left - v.x) / v.k,
      y: (e.clientY - rect.top - v.y) / v.k
    };
  };
  // Touch shim: expose clientX/Y/target so mouse-based handlers work on mobile.
  const asPointer = e => {
    const t = e.touches && e.touches[0] || e.changedTouches && e.changedTouches[0];
    return t ? {
      clientX: t.clientX,
      clientY: t.clientY,
      target: e.target,
      preventDefault: () => e.preventDefault(),
      stopPropagation: () => e.stopPropagation()
    } : e;
  };
  const onNodeDown = (e, n) => {
    if (e.target.closest("input,select,button,label,textarea")) return;
    const w = toWorld(e);
    dragRef.current = {
      id: n.id,
      ox: w.x - n.x,
      oy: w.y - n.y
    };
    setDragId(n.id);
    e.preventDefault();
  };
  const onSocketDown = (e, n, port) => {
    e.stopPropagation();
    e.preventDefault();
    const w = toWorld(e);
    pendingRef.current = {
      from: n.id,
      port
    };
    setPending({
      from: n.id,
      port,
      x: w.x,
      y: w.y
    });
  };
  // dragging empty canvas = pan the view
  const onCanvasDown = e => {
    if (e.target.closest(".gnode,.gsock,.ghit,.gzoom")) return;
    const v = viewRef.current;
    panRef.current = {
      sx: e.clientX,
      sy: e.clientY,
      vx: v.x,
      vy: v.y
    };
  };
  useEffect(() => {
    const move = e => {
      if (panRef.current) {
        const p = panRef.current;
        setView(v => ({
          ...v,
          x: p.vx + (e.clientX - p.sx),
          y: p.vy + (e.clientY - p.sy)
        }));
        return;
      }
      const w = toWorld(e);
      const d = dragRef.current;
      if (d) {
        const x = Math.max(-200, w.x - d.ox);
        const y = Math.max(-100, w.y - d.oy);
        setGraph(g => ({
          ...g,
          nodes: g.nodes.map(n => n.id === d.id ? {
            ...n,
            x,
            y
          } : n)
        }));
      }
      const p = pendingRef.current;
      if (p) setPending({
        ...p,
        x: w.x,
        y: w.y
      });
    };
    const up = e => {
      if (panRef.current) {
        panRef.current = null;
        return;
      }
      if (dragRef.current) {
        dragRef.current = null;
        setDragId(null);
        commitGraph(graphRef.current);
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
            const edges = g.edges.filter(e2 => e2.to !== to);
            edges.push({
              from: p.from,
              ...(p.port ? {
                fromPort: p.port
              } : {}),
              to
            });
            commitGraph({
              ...g,
              edges
            });
          }
        }
      }
    };
    // wheel zoom (native, non-passive so preventDefault works)
    const wheel = e => {
      if (!canvasRef.current || !e.target.closest(".gcanvas")) return;
      e.preventDefault();
      const v = viewRef.current;
      const factor = e.deltaY < 0 ? 1.1 : 1 / 1.1;
      const k = Math.max(0.4, Math.min(2.5, v.k * factor));
      const rect = canvasRef.current.getBoundingClientRect();
      const cx = e.clientX - rect.left;
      const cy = e.clientY - rect.top;
      setView({
        x: cx - (cx - v.x) / v.k * k,
        y: cy - (cy - v.y) / v.k * k,
        k
      });
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    window.addEventListener("wheel", wheel, {
      passive: false
    });
    // Touch: pan / drag-node / draw-wire with one finger (touch-action:none
    // on .gcanvas stops the browser from stealing the gesture for scrolling).
    const gestureActive = () => !!(panRef.current || dragRef.current || pendingRef.current);
    const tmove = e => {
      if (!gestureActive()) return;
      e.preventDefault();
      move(asPointer(e));
    };
    const tup = e => {
      if (!gestureActive()) return;
      e.preventDefault();
      up(asPointer(e));
    };
    window.addEventListener("touchmove", tmove, {
      passive: false
    });
    window.addEventListener("touchend", tup);
    window.addEventListener("touchcancel", tup);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      window.removeEventListener("wheel", wheel);
      window.removeEventListener("touchmove", tmove);
      window.removeEventListener("touchend", tup);
      window.removeEventListener("touchcancel", tup);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return <div className="wrap">
      <header>
        <h1>CodecTrans</h1>
        <nav className="tabs">
          {[["dashboard", t("\uB300\uC2DC\uBCF4\uB4DC")], ["library", t("\uB77C\uC774\uBE0C\uB7EC\uB9AC")], ["failures", t("\uC2E4\uD328 \uBCF4\uAD00\uD568")], ["logic", t("\uB85C\uC9C1")], ["settings", t("\uC138\uD305")], ["test", t("\uD14C\uC2A4\uD2B8")]].map(([id, label]) => <button key={id} className={tab === id ? "on" : ""} onClick={() => setTab(id)}>
              {label}
            </button>)}
        </nav>
        <div className={`pill ${health?.status === "ok" ? "ok" : "bad"}`}>
          {health?.status === "ok" ? "backend online" : "backend offline"}
        </div>
      </header>
      {health?.ffmpeg && <div className="sub">{health.ffmpeg}</div>}

      {authRequired && <div className="authbar">
          <span>{t("\uC11C\uBC84\uC5D0 API \uD1A0\uD070(CT_API_TOKEN)\uC774 \uC124\uC815\uB418\uC5B4 \uC788\uC2B5\uB2C8\uB2E4 \u2014 \uD1A0\uD070\uC744 \uC785\uB825\uD558\uC138\uC694.")}</span>
          <input type="password" value={tokenDraft} onChange={e => setTokenDraft(e.target.value)} onKeyDown={e => {
        if (e.key === "Enter" && tokenDraft.trim()) {
          setAuthToken(tokenDraft.trim());
          location.reload();
        }
      }} placeholder={t("API \uD1A0\uD070")} autoFocus />
          <button onClick={() => {
        if (tokenDraft.trim()) {
          setAuthToken(tokenDraft.trim());
          location.reload();
        }
      }}>{t("\uC800\uC7A5")}</button>
          <button className="ghost" onClick={() => {
        setAuthRequired(false);
        setTokenDraft("");
      }}>{t("\uB2EB\uAE30")}</button>
        </div>}

      {/* ═══ 테스트 — the original upload/transcode screen ═══ */}
      {tab === "test" && <div className="grid">
        {/* left: upload + settings */}
        <section className="card">
          <h2>1 · Source</h2>
          <div className={`dropzone ${dragActive ? "active" : ""}`} onDragEnter={onDragEnter} onDragOver={e => e.preventDefault()} onDragLeave={onDragLeave} onDrop={onDrop} onClick={() => fileRef.current?.click()} role="button" tabIndex={0} onKeyDown={e => {
          if (e.key === "Enter" || e.key === " ") fileRef.current?.click();
        }}>
            <input ref={fileRef} type="file" accept="video/*,audio/*" onChange={onUpload} className="file-hidden" />
            <div className="dz-ico">🎬</div>
            <div className="dz-text">
              <b>{dragActive ? "Drop it here" : "Drag a video here"}</b>
              <span>or click to browse your files</span>
            </div>
          </div>
          {file && <div className="meta">
              <span>{file.name}</span>
              <span>{fmtSize(file.size)}</span>
            </div>}
          {uploading && <div className="meta">
              <div className="prog-row">
                <span>Uploading… {Math.round(uploadProgress * 100)}%</span>
                <span>{file ? fmtSize(file.size) : ""}</span>
              </div>
              <div className="bar">
                <div className="bar-fill" style={{
              width: `${uploadProgress * 100}%`
            }} />
              </div>
            </div>}
          {uploadedFile && <div className="meta ok-text">Uploaded: {uploadedFile}</div>}

          {/* source analysis — what ffprobe found in the uploaded file */}
          {analysis && <div className="analysis">
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
                  {analysis.audioCodec ? `${analysis.audioCodec} · ${analysis.audioSampleRate ? analysis.audioSampleRate / 1000 + " kHz" : ""}${analysis.audioChannels ? " · " + analysis.audioChannels + "ch" : ""}` : "none"}
                </span>
                <span>
                  <b>Pixel</b> {analysis.pixFmt || "?"} · {analysis.container || "?"}
                </span>
              </div>
            </div>}

          <h2>2 · Target</h2>
          <label>
            Video codec
            <select value={videoCodec} onChange={e => setVideoCodec(e.target.value)}>
              {codecs?.videoCodecs.map(c => <option key={c.id} value={c.id}>
                  {c.label}
                </option>)}
            </select>
          </label>
          <label>
            Audio codec
            <select value={audioCodec} onChange={e => setAudioCodec(e.target.value)}>
              {codecs?.audioCodecs.map(c => <option key={c.id} value={c.id}>
                  {c.label}
                </option>)}
            </select>
          </label>
          <label>
            Container
            <select value={container} onChange={e => setContainer(e.target.value)}>
              {codecs?.containers.map(c => <option key={c.id} value={c.id}>
                  {c.label}
                </option>)}
            </select>
          </label>
          {videoCodec !== "copy" && <>
              <div className="ratemode">
                <button type="button" className={`rm-opt ${rateMode === "crf" ? "on" : ""}`} onClick={() => setRateMode("crf")}>
                  Quality (CRF)
                </button>
                <button type="button" className={`rm-opt ${rateMode === "pct" ? "on" : ""}`} onClick={() => setRateMode("pct")}>
                  % of source bitrate
                </button>
              </div>
              {rateMode === "crf" ? <label>
                  Quality (CRF {crf}) — lower is better
                  <input type="range" min="0" max="51" value={crf} onChange={e => setCrf(Number(e.target.value))} />
                </label> : <label>
                  Target: {bitratePct}% of source bitrate
                  {estKbps != null && <span className="est">
                      ≈ {fmtRate(estKbps * 1000)}
                      {analysis?.videoBitRate ? ` (source ${fmtRate(analysis.videoBitRate)})` : ""}{" "}
                      · capped at {fmtRate(estKbps * 1200)}
                    </span>}
                  {estKbps == null && <span className="est">
                      source bitrate unknown — the server will estimate from the
                      file size
                    </span>}
                  <input type="range" min="10" max="100" step="5" value={bitratePct} onChange={e => setBitratePct(Number(e.target.value))} />
                  <div className="rm-scale">
                    <span>smaller file</span>
                    <span>50%</span>
                    <span className="mid">100% = original</span>
                  </div>
                </label>}
            </>}

          <label>
            Run on
            <select value={target} onChange={e => setTarget(e.target.value)}>
              <option value="server">Server (local CPU)</option>
              <option value="auto">Auto — best node (HW accel)</option>
              {onlineNodes.map(n => <option key={n.id} value={n.id}>
                  Node: {n.name}
                </option>)}
            </select>
          </label>
          {target !== "server" && onlineNodes.length === 0 && <div className="err">No nodes online — start a CodecTrans-node.</div>}

          <button className="primary" onClick={onStart} disabled={busy || uploading || !uploadedFile}>
            {busy ? "Working…" : target === "server" ? "Transcode" : "Dispatch"}
          </button>
          {error && <div className="err">{error}</div>}
        </section>

        {/* right: progress + history */}
        <section className="card">
          <h2>3 · Jobs</h2>
          {activeJob && <div className="active">
              <div className="row">
                <span className={`status ${activeJob.status}`}>{STATUS_LABEL[activeJob.status] || activeJob.status}</span>
                {activeJob.accelerated && <span className="accel" title={`HW: ${activeJob.effectiveVideoCodec}`}>
                    ⚡ {activeJob.effectiveVideoCodec}
                  </span>}
                <span className="pct">{progress}%</span>
              </div>
              <div className="bar">
                <div className={`fill ${activeJob.status === "error" ? "bad" : ""}`} style={{
              width: `${activeJob.status === "error" ? 100 : progress}%`
            }} />
              </div>
              <div className="meta">
                <span>by {activeJob.executedBy || "server"}</span>
                {activeJob.outputFile && (activeJob.status === "done" || activeJob.status === "error") && <span>
                    {activeJob.outputFile} · {fmtSize(activeJob.outputSize)}
                  </span>}
              </div>
              {activeJob.status === "queued" && <button className="rm-del" title={t("\uD050\uC5D0\uC11C \uCDE8\uC18C (\uC544\uC9C1 \uB178\uB4DC\uAC00 \uC548 \uAC00\uC838\uAC04 \uB300\uAE30 \uC7A1\uB9CC \uCDE8\uC18C \uAC00\uB2A5)")} onClick={async () => {
            try {
              await api.jobCancel(activeJob.id);
              setActiveJob(null);
              setJobs(await api.jobs());
            } catch (e) {
              setError(e.message);
            }
          }}>{t("\u2715 \uCDE8\uC18C")}</button>}
              {activeJob.error && <div className="err">{activeJob.error}</div>}
            </div>}

          {jobs.filter(j => j.status === "queued" && !j.libPath).length > 0 && <div className="row" style={{
          justifyContent: "space-between",
          marginBottom: 6
        }}>
              <span className="meta">
                {jobs.filter(j => j.status === "queued" && !j.libPath).length}{t("\uAC74 \uB300\uAE30 \uC911")}{jobs.some(j => j.status === "queued" && j.libPath) && t(" (\uC790\uB3D9\uD654 \uD050 \uC81C\uC678)")}
              </span>
              {jobs.every(j => !j.libPath || j.status !== "queued") && <button className="rm-del" title={t("\uB300\uAE30(queued) \uC7A1\uC744 \uC804\uBD80 \uC9C0\uC6C0 \u2014 \uC2E4\uD589 \uC911/\uC644\uB8CC/\uC5D0\uB7EC \uAE30\uB85D\uC740 \uC720\uC9C0")} onClick={async () => {
            const n = jobs.filter(j => j.status === "queued").length;
            if (!window.confirm(t("\uB300\uAE30 \uC911\uC778 \uC7A1 ") + n + t("\uAC74\uC744 \uC804\uBD80 \uCDE8\uC18C\uD560\uAE4C\uC694?"))) return;
            try {
              await api.jobsClear();
              if (activeJob?.status === "queued") setActiveJob(null);
              setJobs(await api.jobs());
            } catch (e) {
              setError(e.message);
            }
          }}>{t("\uD83D\uDDD1 \uD050 \uBE44\uC6B0\uAE30")}</button>}
            </div>}

          <div className="history">
            {jobs.filter(j => !j.libPath).length === 0 && <div className="empty">No jobs yet.</div>}
            {jobs.filter(j => !j.libPath).map(j => <div key={j.id} className="job">
                <div className="row">
                  <span className={`status ${j.status}`}>{STATUS_LABEL[j.status] || j.status}</span>
                  {j.accelerated && <span className="accel" title={`HW: ${j.effectiveVideoCodec}`}>⚡</span>}
                  <span className="jname" title={j.inputFile || j.outputFile}>
                    {(j.inputFile || j.outputFile || "").split("/").pop() || j.id.slice(0, 8)}
                  </span>
                  {j.libPath && <span className="chip c-av1" title={t("\uB77C\uC774\uBE0C\uB7EC\uB9AC \uC790\uB3D9\uD654: ") + j.libPath}>{t("\uD83D\uDCDA \uC790\uB3D9\uD654")}</span>}
                  <span className="jmeta">
                    {j.executedBy} · {j.effectiveVideoCodec} → {j.container}
                    {j.outputSize != null ? ` · ${fmtSize(j.outputSize)}` : ""}
                    {j.outputFile ? <span className="jout" title={t("\uACB0\uACFC \uD30C\uC77C: ") + j.outputFile}>
                        {" → "}
                        {j.outputFile.split("/").pop()}
                      </span> : null}
                  </span>
                </div>
                {(j.status === "running" || j.status === "dispatched") && <div className="row" style={{
              marginTop: 4,
              gap: 6
            }}>
                    <div className="bar" style={{
                flex: 1,
                height: 6
              }}>
                      <div className="fill" style={{
                  width: `${Math.round((j.progress || 0) * 100)}%`
                }} />
                    </div>
                    <span className="prog-pct">
                      {Math.round((j.progress || 0) * 100)}%
                    </span>
                  </div>}
                {j.status === "done" && <a className="dl" href={api.downloadUrl(j.outputFile)} download>
                    ⬇ download
                  </a>}
                {j.status === "queued" && <button className="rm-del" title={t("\uD050\uC5D0\uC11C \uCDE8\uC18C (\uB178\uB4DC\uAC00 \uAC00\uC838\uAC00\uAE30 \uC804 \uB300\uAE30 \uC7A1\uB9CC \uAC00\uB2A5)")} onClick={async () => {
              try {
                await api.jobCancel(j.id);
                if (activeJob?.id === j.id) setActiveJob(null);
                setJobs(await api.jobs());
              } catch (e) {
                setError(e.message);
              }
            }}>
                    ✕
                  </button>}
                {j.error && <div className="err">{j.error}</div>}
              </div>)}
          </div>
        </section>
      </div>}

      {/* ═══ 대시보드 — server CPU/RAM + node registry ═══ */}
      {tab === "dashboard" && <>
          <section className="card stats">
            <h2>{t("\uC11C\uBC84")}</h2>
            <div className="statgrid">
              <div className="stat">
                <div className="stat-top">
                  <span className="stat-label">CPU</span>
                  <span className="stat-val">
                    {stats?.cpuPct != null ? `${Math.round(stats.cpuPct)}%` : "—"}
                  </span>
                </div>
                <div className="bar">
                  <div className="bar-fill" style={{
                width: `${Math.min(100, stats?.cpuPct ?? 0)}%`
              }} />
                </div>
                {stats?.cores != null && <div className="meta">
                    <span>{stats.cores} cores</span>
                  </div>}
              </div>
              <div className="stat">
                <div className="stat-top">
                  <span className="stat-label">RAM</span>
                  <span className="stat-val">
                    {stats?.ramPct != null ? `${Math.round(stats.ramPct)}%` : "—"}
                  </span>
                </div>
                <div className="bar">
                  <div className="bar-fill" style={{
                width: `${Math.min(100, stats?.ramPct ?? 0)}%`
              }} />
                </div>
                {stats?.ramTotal != null && <div className="meta">
                    <span>
                      {fmtSize(stats.ramUsed)} / {fmtSize(stats.ramTotal)}
                    </span>
                  </div>}
              </div>
            </div>
          </section>
      <section className="card nodes">
        <h2>
          Nodes ({shownNodes.length})
          {offlineNodes.length > 0 && <span className="meta" style={{
            marginLeft: 8,
            fontWeight: 400
          }}>
              · {offlineNodes.length} offline
            </span>}
        </h2>

        {/* registry filters: online default (offline hidden), HW accel backend, codec family */}
        <div className="nodefilters">
          <div className="seg">
            {["online", "offline", "all"].map(s => <button key={s} className={nodeFilter.status === s ? "on" : ""} onClick={() => setNodeFilter(f => ({
              ...f,
              status: s
            }))}>
                {s === "online" ? `Online (${onlineNodes.length})` : s === "offline" ? `Offline (${offlineNodes.length})` : "All"}
              </button>)}
          </div>
          <select value={nodeFilter.backend} onChange={e => setNodeFilter(f => ({
            ...f,
            backend: e.target.value
          }))}>
            <option value="">All backends</option>
            {backendOptions.map(b => <option key={b} value={b}>
                {b}
              </option>)}
          </select>
          <select value={nodeFilter.family} onChange={e => setNodeFilter(f => ({
            ...f,
            family: e.target.value
          }))}>
            <option value="">All codecs</option>
            {familyOptions.map(f => <option key={f} value={f}>
                {f}
              </option>)}
          </select>
        </div>

        {/* server finds a node: enter the IP:port it printed in its terminal */}
        <div className="search">
          <input value={searchIp} onChange={e => setSearchIp(e.target.value)} placeholder="node IP (e.g. 192.168.x.x)" />
          <input value={searchPort} onChange={e => setSearchPort(e.target.value)} placeholder="port" className="port" />
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

        {nodes.length === 0 && <div className="empty">
            No nodes yet. Run <code>CodecTransNode</code> and search it above.
          </div>}
        {nodes.length > 0 && shownNodes.length === 0 && <div className="empty">
            No nodes match the current filter.
            <button className="linkish" onClick={() => setNodeFilter({
            status: "online",
            backend: "",
            family: ""
          })}>
              Reset
            </button>
          </div>}
        {shownNodes.map(n => {
          // live stats line (from the node's heartbeat; old binaries → hidden)
          const sp = [];
          if (n.cpuPct != null) sp.push(`CPU ${Math.round(n.cpuPct)}%`);
          if (n.ramUsedMb != null && n.ramTotalMb != null) sp.push(`RAM ${fmtSize(n.ramUsedMb * 1048576)} / ${fmtSize(n.ramTotalMb * 1048576)}`);
          if (n.gpuTemp != null) sp.push(`GPU ${n.gpuTemp}°C${n.gpuUtil != null ? ` · ${n.gpuUtil}%` : ""}`);
          return <div key={n.id} className="node">
              <div className="row">
                <span className={`status ${n.status === "online" ? "done" : ""}`}>{n.status}</span>
                <span className="jname" title={n.name === (n.displayName || n.name) ? undefined : t("\uB4F1\uB85D\uBA85: ") + n.name}>
                  {n.displayName || n.name}
                </span>
                {n.currentJob && <span className="jmeta">busy: {n.currentJob.slice(0, 8)}…</span>}
                <span className="jmeta">{n.ffmpegVersion.split(" ").slice(0, 3).join(" ")}</span>
                {n.maxJobs > 1 && <span className="chip c-vp8" title={t("\uB3D9\uC2DC \uC778\uCF54\uB529 \uC218 (\uB178\uB4DC \uC124\uC815)")}>×{n.maxJobs}</span>}
                {(n.enabledBackends || []).length > 0 && <span className="chip c-h264" title={t("\uC6CC\uCEE4\uB85C \uD5C8\uC6A9\uB41C \uAC00\uC18D \uBC31\uC5D4\uB4DC (\uCCB4\uD06C \uC548 \uD55C \uBC31\uC5D4\uB4DC\uB294 \uBC30\uC815\uB418\uC9C0 \uC54A\uC74C)")}>
                    {n.enabledBackends.join(" · ")}
                  </span>}
                {n.allowSoftware === false && <span className="chip c-vp9" title={t("\uC18C\uD504\uD2B8\uC6E8\uC5B4 \uC778\uCF54\uB529 \uD3F4\uBC31 \uBE44\uD65C\uC131 \u2014 \uC774 \uB178\uB4DC\uB294 HW \uC7A1\uB9CC \uBC1B\uC74C")}>{t("HW\uB9CC")}</span>}
                <button className="gear" title={t("\uB178\uB4DC \uC124\uC815 \u2014 \uC774\uB984 \xB7 \uC6CC\uCEE4 \uC790\uC6D0 \xB7 \uB3D9\uC2DC \uC791\uC5C5")} onClick={() => openNodeCfg(n)}>
                  ⚙
                </button>
              </div>
              {(() => {
              const live = jobs.filter(j => j.nodeId === n.id && (j.status === "running" || j.status === "dispatched"));
              if (live.length === 0) return null;
              return <div className="nodejobs">
                    {live.map(j => {
                  const pct = Math.round((j.progress || 0) * 100);
                  return <div key={j.id} className="nodejob">
                          <div className="row" style={{
                      gap: 6
                    }}>
                            <span className="jmeta" title={j.libPath || j.inputFile}>
                              {j.libPath ? "📚 " : ""}
                              {(j.inputFile || "").split("/").pop() || j.id.slice(0, 8)}
                            </span>
                            <span className="jmeta">
                              {j.effectiveVideoCodec || j.videoCodec} →{" "}
                              {j.container}
                            </span>
                            <span className="prog-pct">{pct}%</span>
                          </div>
                          <div className="bar" style={{
                      height: 5
                    }}>
                            <div className="fill" style={{
                        width: pct + "%"
                      }} />
                          </div>
                        </div>;
                })}
                  </div>;
            })()}
              <AccelPanel accels={n.accels} flat={n.hwEncoders} />
              {sp.length > 0 && <div className="nodestats">{sp.join("  ·  ")}</div>}
            </div>;
        })}

        {discovered.length > 0 && <>
            <h3>Seen on the LAN (mDNS, not yet registered)</h3>
            {discovered.map(d => <div key={d.name} className="node">
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
              </div>)}
          </>}
      </section>

      {/* running + queued jobs — manual test AND library automation,
          each with its own live progress */}
      {(() => {
        const liveJobs = jobs.filter(j => ["queued", "dispatched", "running"].includes(j.status));
        if (liveJobs.length === 0) return null;
        return <section className="card nodes">
            <h2>{t("Jobs \u2014 \uC9C4\uD589/\uB300\uAE30 (")}{liveJobs.length})</h2>
            <div className="hint">{t("\uC9C0\uAE08 \uB3C4\uB294 \uC791\uC5C5 \uC804\uBD80 \u2014 \uD14C\uC2A4\uD2B8 \uD0ED \uC218\uB3D9 \uC7A1\uACFC \uB77C\uC774\uBE0C\uB7EC\uB9AC \uC790\uB3D9\uD654 \uC7A1\uC774 \uD568\uAED8 \uBCF4\uC774\uACE0, \uAC01 \uC7A1\uB9C8\uB2E4 \uC9C4\uD589\uB3C4\uAC00 \uCD94\uC801\uB429\uB2C8\uB2E4.")}</div>
            {liveJobs.map(j => {
            const pct = Math.round((j.progress || 0) * 100);
            const live = j.status === "running" || j.status === "dispatched";
            return <div key={j.id} className="nodejob" style={{
              marginBottom: 8
            }}>
                  <div className="row" style={{
                gap: 6,
                alignItems: "center"
              }}>
                    <span className={`status ${j.status}`}>
                      {STATUS_LABEL[j.status] || j.status}
                    </span>
                    <span className="jname" title={j.libPath || j.inputFile}>
                      {j.libPath ? "📚 " : ""}
                      {(j.inputFile || j.outputFile || "").split("/").pop() || j.id.slice(0, 8)}
                    </span>
                    <span className="jmeta">
                      {j.executedBy || "—"} · {j.effectiveVideoCodec || j.videoCodec} →{" "}
                      {j.container}
                    </span>
                    {live && <span className="prog-pct">{pct}%</span>}
                    <button className="mini" title={live ? t("\uC778\uCF54\uB529 \uC911\uC9C0 \u2014 \uB178\uB4DC\uAC00 10\uCD08 \uC548\uC5D0 ffmpeg\uB97C \uC885\uB8CC\uD569\uB2C8\uB2E4") : t("\uD050\uC5D0\uC11C \uC81C\uAC70")} onClick={() => api.jobCancel(j.id).catch(() => {})}>
                      ✕
                    </button>
                  </div>
                  {live && <div className="bar" style={{
                height: 5
              }}>
                      <div className="fill" style={{
                  width: pct + "%"
                }} />
                    </div>}
                </div>;
          })}
          </section>;
      })()}

      {/* node worker-settings modal (gear) */}
      {nodeCfg && <div className="overlay" onClick={() => setNodeCfg(null)}>
          <div className="cfgmodal" onClick={e => e.stopPropagation()}>
            <h3>{t("\uB178\uB4DC \uC124\uC815 \u2014")}{nodeCfg.name || nodeCfg.regName}
            </h3>
            <label className="cfglabel">{t("\uD45C\uC2DC \uC774\uB984")}<input value={nodeCfg.name} placeholder={nodeCfg.regName} onChange={e => patchNodeCfg({
              name: e.target.value
            })} />
              <span className="meta">{t("\uBE44\uC6CC\uB450\uBA74 \uB4F1\uB85D\uBA85 (")}{nodeCfg.regName}{t(")\uC744 \uC501\uB2C8\uB2E4")}</span>
            </label>
            <div className="cfglabel">{t("\uC6CC\uCEE4\uB85C \uC0AC\uC6A9\uD560 \uC790\uC6D0")}<span className="meta">{t("\uCCB4\uD06C\uB97C \uB044\uBA74 \uADF8 \uBC31\uC5D4\uB4DC\uB85C \uC7A1\uC744 \uBC30\uC815\uD558\uC9C0 \uC54A\uC2B5\uB2C8\uB2E4 \u2014 \uC608: iGPU \uAC00\uC18D\uC774 dGPU \uC778\uCF54\uB529\uACFC CPU\uB97C \uB450\uACE0 \uACBD\uC7C1\uD574 \uBCD1\uBAA9\uC774 \uB420 \uB54C iGPU \uB044\uAE30")}</span>
            </div>
            <div className="cfgchecks">
              {nodeCfgBackends(nodeCfg).map(([b, devs]) => <label key={b}>
                  <input type="checkbox" checked={nodeCfg.backends.has(b)} onChange={() => toggleNodeBackend(b)} />
                  <code>{b}</code>
                  <span className="meta">{devs.join(" / ")}</span>
                </label>)}
              <label>
                <input type="checkbox" checked={nodeCfg.allowSw} onChange={e => patchNodeCfg({
                allowSw: e.target.checked
              })} />{t("\uC18C\uD504\uD2B8\uC6E8\uC5B4 \uC778\uCF54\uB529 (HW\uB85C \uBABB \uC4F0\uB294 \uCF54\uB371\uC758 \uCD5C\uC885 \uD3F4\uBC31)")}</label>
            </div>
            <label className="cfglabel">{t("\uB3D9\uC2DC \uC778\uCF54\uB529 \uC218")}<input type="number" className="num" min={1} max={16} value={nodeCfg.maxJobs} onChange={e => patchNodeCfg({
              maxJobs: Math.max(1, Math.min(16, Number(e.target.value) || 1))
            })} />
              <span className="meta">{t("\uC608: 2-\uC2AC\uB77C\uC774\uC2A4 NVENC GPU\uB294 2~3\uAC1C \uB3D9\uC2DC \uC778\uCF54\uB529\uAE4C\uC9C0 \uC131\uB2A5 \uC190\uD574\uAC00 \uC801\uC74C")}</span>
            </label>
            <div className="cfgactions">
              <span className="meta">{nodeCfg.msg}</span>
              <button onClick={() => setNodeCfg(null)}>{t("\uCDE8\uC18C")}</button>
              <button className="primary" onClick={saveNodeCfg}>{t("\uC800\uC7A5")}</button>
            </div>
          </div>
        </div>}
        </>}

      {/* ═══ 라이브러리 — per-library sheets + integrated layout ═══ */}
      {tab === "library" && <>
          <section className="card lib2">
            <h2>Library</h2>
            <div className="hint">{t("\uB4F1\uB85D\uB41C \uD3F4\uB354 \uC804\uCCB4\uB97C")}<b>read-only</b>{t("\uB85C \uC2A4\uCE94\uD569\uB2C8\uB2E4 \u2014 \uD30C\uC77C\uC740 \uC808\uB300 \uAC74\uB4DC\uB9AC\uC9C0 \uC54A\uC2B5\uB2C8\uB2E4.")}<b>{t("\uBE44\uD65C\uC131 \uD3F4\uB354")}</b>{t("\uC758 \uC601\uC0C1\uC740 \uBAA9\uB85D\uC5D0 \uD45C\uC2DC\uB418\uC9C0\uB9CC \uC790\uB3D9 \uD050\uC5D0 \uB4E4\uC5B4\uAC00\uC9C0 \uC54A\uC2B5\uB2C8\uB2E4. \uC544\uB798")}<b>{t("\uC2DC\uD2B8")}</b>{t("\uC5D0\uC11C \uB77C\uC774\uBE0C\uB7EC\uB9AC\uBCC4\uB85C \uB300\uC0C1 \uC601\uC0C1\uC744 \uD655\uC778\uD558\uACE0, \uAC01 \uB77C\uC774\uBE0C\uB7EC\uB9AC\uC758 \uCC98\uB9AC \uBC29\uC2DD\uC744 \uBC14\uB85C \uC870\uC815\uD558\uC138\uC694.")}</div>
            <div className="search">
              <button onClick={() => doScan()} disabled={scanning || folders.length === 0} title={folders.length === 0 ? t("\uB4F1\uB85D\uB41C \uD3F4\uB354\uAC00 \uC5C6\uC2B5\uB2C8\uB2E4 \u2014 \uD3F4\uB354 \uAD00\uB9AC\uC5D0\uC11C \uBA3C\uC800 \uCD94\uAC00\uD558\uC138\uC694") : undefined}>
                {scanning ? "Scanning…" : folders.length === 0 ? t("\uD3F4\uB354 \uCD94\uAC00 \uD6C4 \uC2A4\uCE94") : t("\uC804\uCCB4 \uB2E4\uC2DC \uC2A4\uCE94")}
              </button>
              {scanning && <span className="scanprog" title={t("\uC2A4\uCE94 \uBC94\uC704: ") + (scanProg?.scope || "…")}>
                  <span className="scanprog-txt">
                    {scanProg?.total ? t("\uC2A4\uCE94 \uC911\u2026 ") + scanProg.scanned + "/" + scanProg.total : t("\uC2A4\uCE94 \uC900\uBE44\u2026")}
                    {scanProg?.scope ? ` · ${scanProg.scope}` : ""}
                  </span>
                  <span className="bar" style={{
              flex: 1,
              height: 6
            }}>
                    <span className="fill" style={{
                width: scanProg?.total > 0 ? `${Math.round(scanProg.scanned / scanProg.total * 100)}%` : "0%"
              }} />
                  </span>
                </span>}
              {!scanning && scanProg?.active === false && <span className="meta dim">{t("\uB9C8\uC9C0\uB9C9 \uC2A4\uCE94:")}{scanProg.scanned}{t("\uAC1C \uD655\uC778")}{scanProg.finished_queued ? t(", \uD050\uC789 ") + scanProg.finished_queued : ""}
                  {scanProg.finished_skipped ? t(", \uC2A4\uD0B5 ") + scanProg.finished_skipped : ""}
                </span>}
            </div>
            {/* library sheets — per-library tabs (전체 + each registered folder) */}
            <div className="libsheets">
              <button className={`libsheet${libSel === "all" ? " on" : ""}`} onClick={() => pickLib("all")} title={t("\uC778\uCF54\uB529 \uB300\uC0C1 (\uC2A4\uD0B5\xB7\uBE44\uC601\uC0C1 \uC81C\uC678) ") + libEntriesTarget(library.entries) + t(" / \uC804\uCCB4 ") + library.entries.length}>{t("\uC804\uCCB4")}<span className="cnt">{libEntriesTarget(library.entries)}</span>
              </button>
              {folders.map(f => <button key={f.id} className={`libsheet${libSel === f.id ? " on" : ""}${f.enabled ? "" : " off"}`} onClick={() => pickLib(f.id)} title={`${f.path}${f.enabled ? "" : t(" \u2014 \uBE44\uD65C\uC131")}`}>
                  {libName(f)}
                  <span className="cnt">{libEntriesTarget(libEntries(f))}</span>
                  {!f.enabled && <span className="offdot" title={t("\uBE44\uD65C\uC131 \u2014 \uD3F4\uB354 \uAD00\uB9AC\uC5D0\uC11C \uD65C\uC131\uD654")}>
                      ·
                    </span>}
                </button>)}
              <span className="libsheets-sep" />
              <button className={`libsheet${libSel === "skipped" ? " on" : ""}`} onClick={() => pickLib("skipped")} title={t("\uC2A4\uD0B5 \uBAA9\uB85D \u2014 \uB85C\uC9C1/\uD3F4\uB354 \uC124\uC815\uC5D0 \uB530\uB77C \uCC98\uB9AC\uD558\uC9C0 \uC54A\uACE0 \uAC74\uB108\uB700 (\uC7AC\uC2A4\uCE94\uC5D0\uB3C4 \uC720\uC9C0, \u27F3\uC73C\uB85C \uC7AC\uD050\uC789)")}>{t("\u23ED \uC2A4\uD0B5")}<span className="cnt">
                  {library.entries.filter(e => e.status === "skipped").length}
                </span>
              </button>
              <button className={`libsheet${libSel === "notvideo" ? " on" : ""}`} onClick={() => pickLib("notvideo")} title={t("\uBE44\uC601\uC0C1 \uBAA9\uB85D \u2014 \uC601\uC0C1 \uC2A4\uD2B8\uB9BC\uC774 \uC5C6\uAC70\uB098 \uAE38\uC774\uAC00 \uC5C6\uB294 \uD30C\uC77C (\uC378\uB124\uC77C mjpeg \uB4F1). \uC808\uB300 \uD050\uC5D0 \uB4E4\uC5B4\uAC00\uC9C0 \uC54A\uC74C")}>{t("\uD83D\uDDBC \uBE44\uC601\uC0C1")}<span className="cnt">
                  {library.entries.filter(e => e.status === "notvideo").length}
                </span>
              </button>
            </div>
            {/* the selected library's config row */}
            {selFolder && <div className="libconf">
                <button className={`enablebtn${selFolder.enabled ? " on" : ""}`} onClick={() => {
            const turningOn = !selFolder.enabled;
            // Enabling a folder starts its scan immediately — new
            // files are found and auto-queued without a manual scan.
            patchFolder(selFolder.id, x => ({
              ...x,
              enabled: !x.enabled
            })).then(() => {
              if (turningOn) doScan(selFolder.id);
            });
          }}>
                  {selFolder.enabled ? t("\u23F8 \uBE44\uD65C\uC131\uD654") : t("\u25B6 \uD65C\uC131\uD654")}
                </button>
                <label className="logicpick">{t("\uB85C\uC9C1")}<select value={selFolder.logicId || ""} onChange={e => patchFolder(selFolder.id, x => ({
              ...x,
              logicId: e.target.value || null
            }))} title={t("\uB85C\uC9C1\uC744 \uC4F0\uBA74 \uADF8 \uADF8\uB798\uD504\uAC00 \uC6B0\uC120 \u2014 \uC544\uBB34 \uC561\uC158\uC5D0\uB3C4 \uC548 \uB9DE\uC740 \uD30C\uC77C\uB9CC \uC544\uB798 \uC77C\uAD04 \uC124\uC815\uC73C\uB85C \uCC98\uB9AC")}>
                    <option value="">{t("\uC5C6\uC74C \u2014 \uC544\uB798 \uC124\uC815\uC73C\uB85C \uC77C\uAD04 \uC801\uC6A9")}</option>
                    {logics.map(l => <option key={l.id} value={l.id}>
                        {l.name}
                      </option>)}
                  </select>
                </label>
                {selFolder.logicId ? <div className="logic-note" title={t("\uCF54\uB371\xB7\uBE44\uD2B8\uB808\uC774\uD2B8\xB7\uAC80\uC99D\xB7\uB300\uCE58\uAE4C\uC9C0 \uC804\uBD80 \uB85C\uC9C1 \uADF8\uB798\uD504\uC758 \uC778\uCF54\uB529/\uD6C4\uCC98\uB9AC \uB178\uB4DC\uAC00 \uACB0\uC815\uD569\uB2C8\uB2E4")}>
                    <span className="logic-badge">
                      {logics.find(l => l.id === selFolder.logicId)?.name || t("\uB85C\uC9C1")}
                    </span>{t("\uB85C\uC9C1\uC774 \uC801\uC6A9 \uC911 \u2014 \uCC98\uB9AC \uBC29\uC2DD\uC740")}<b>{t("\uB85C\uC9C1 \uD0ED")}</b>{t("\uC758 \uADF8\uB798\uD504\uB85C\uB9CC \uC870\uC815\uB429\uB2C8\uB2E4")}</div> : <>
                    <label>{t("\uBE44\uB514\uC624")}<select value={selFolder.target.videoCodec || ""} onChange={e => patchTarget(selFolder.id, {
                videoCodec: e.target.value || null
              })}>
                        <option value="">{t("\uCF54\uB371 \uC5C6\uC74C (\uD050\uC789 \uC548 \uD568)")}</option>
                        {(codecs?.videoCodecs || []).filter(c => c.id !== "copy").map(c => <option key={c.id} value={c.id}>
                              {c.label}
                            </option>)}
                      </select>
                    </label>
                    <label>{t("\uC624\uB514\uC624")}<select value={selFolder.target.audioCodec || "aac"} onChange={e => patchTarget(selFolder.id, {
                audioCodec: e.target.value
              })}>
                        {(codecs?.audioCodecs || []).map(c => <option key={c.id} value={c.id}>
                            {c.label}
                          </option>)}
                      </select>
                    </label>
                    <label>{t("\uCEE8\uD14C\uC774\uB108")}<select value={selFolder.target.container || "mp4"} onChange={e => patchTarget(selFolder.id, {
                container: e.target.value
              })}>
                        {(codecs?.containers || []).map(c => <option key={c.id} value={c.id}>
                            {c.label}
                          </option>)}
                      </select>
                    </label>
                    <div className="rateval">
                      <div className="ratemode">
                        <button type="button" className={`rm-opt ${(selFolder.target.mode || "pct") === "crf" ? "on" : ""}`} onClick={() => patchTarget(selFolder.id, {
                  mode: "crf"
                })}>
                          CRF
                        </button>
                        <button type="button" className={`rm-opt ${(selFolder.target.mode || "pct") !== "crf" ? "on" : ""}`} onClick={() => patchTarget(selFolder.id, {
                  mode: "pct"
                })}>
                          %
                        </button>
                      </div>
                      <input type="number" className="num" min={(selFolder.target.mode || "pct") === "crf" ? 0 : 10} max={(selFolder.target.mode || "pct") === "crf" ? 51 : 100} value={(selFolder.target.mode || "pct") === "crf" ? selFolder.target.crf ?? 23 : selFolder.target.bitratePct ?? 70} onChange={e => patchTarget(selFolder.id, (selFolder.target.mode || "pct") === "crf" ? {
                crf: Number(e.target.value)
              } : {
                bitratePct: Number(e.target.value)
              })} />
                    </div>
                    <div className="postenc">
                      <label className="postenc-opt" title={t("\uC778\uCF54\uB529 \uC644\uB8CC \uD6C4 ffprobe \uAC80\uC99D \u2014 \uC601\uC0C1 \uC2A4\uD2B8\uB9BC \uC874\uC7AC + \uC7AC\uC0DD\uC2DC\uAC04 \uD655\uC778. \uC2E4\uD328\uD558\uBA74 \uCD9C\uB825 \uC0AD\uC81C \xB7 \uC5D0\uB7EC \uD45C\uC2DC")}>
                        <input type="checkbox" checked={!!selFolder.target.verifyOutput} onChange={e => patchTarget(selFolder.id, {
                  verifyOutput: e.target.checked
                })} />{t("\u2713 \uCD9C\uB825 \uAC80\uC99D")}</label>
                      <label className="postenc-opt" title={t("\uCD9C\uB825\uC774 \uC6D0\uBCF8\uC758 \uACBD\uB85C\uB97C \uB300\uCCB4 \u2014 \uC6D0\uBCF8\uC740 \uC606\uC5D0 .orig\uC73C\uB85C \uBC31\uC5C5")}>
                        <input type="checkbox" checked={!!selFolder.target.replaceOriginal} onChange={e => patchTarget(selFolder.id, {
                  replaceOriginal: e.target.checked
                })} />{t("\u27F2 \uC6D0\uBCF8 \uB300\uCE58")}</label>
                    </div>
                  </>}
              </div>}
            {sheetBase.length > 0 && <div className="libsummary">
                {(() => {
            const es = sheetBase;
            const done = es.filter(e => e.status === "done");
            const err = es.filter(e => e.status === "error");
            const faild = es.filter(e => e.status === "failed");
            const act = es.filter(e => libJob(e));
            const remaining = libEntriesTarget(es) - done.length;
            const saved = done.reduce((a, e) => a + Math.max(0, (e.size || 0) - (e.newSize || 0)), 0);
            const shrunk = done.filter(e => (e.newSize || 0) < (e.size || 0));
            const chip = (key, cls, label, count, title) => <button type="button" className={`ls-item ls-btn${cls ? " " + cls : ""}${libFilter === key ? " on" : ""}`} onClick={() => setLibFilter(libFilter === key ? null : key)} title={(title || label) + (libFilter === key ? t(" \u2014 \uD574\uC81C\uD558\uB824\uBA74 \uB2E4\uC2DC \uD074\uB9AD") : t(" \u2014 \uD074\uB9AD\uD558\uBA74 \uC774 \uBAA9\uB85D\uB9CC \uD45C\uC2DC"))}>
                      <b>{count}</b> {label}
                    </button>;
            return <>
                      {remaining > 0 && chip("remaining", "", t("\uB0A8\uC740 \uD30C\uC77C"), remaining, t("\uC644\uB8CC\xB7\uC2A4\uD0B5\xB7\uBE44\uC601\uC0C1 \uC81C\uC678 \u2014 \uC544\uC9C1 \uCC98\uB9AC \uB300\uAE30/\uC9C4\uD589/\uC624\uB958"))}
                      {chip("done", "ok", t("\uC644\uB8CC"), done.length)}
                      {act.length > 0 && chip("active", "run", t("\uC9C4\uD589 \uC911"), act.length)}
                      {err.length > 0 && chip("error", "bad", t("\uC624\uB958"), err.length)}
                      {faild.length > 0 && chip("failed", "faild", t("\uBCF4\uAD00\uD568"), faild.length, t("\uC2E4\uD328 \uBCF4\uAD00\uD568\uC73C\uB85C \uC774\uB3D9\uB41C \uD30C\uC77C \u2014 \uC2E4\uD328 \uBCF4\uAD00\uD568 \uD0ED\uC5D0\uC11C \uBCF5\uC6D0/\uC0AD\uC81C"))}
                      {saved > 0 && <span className="ls-item save" title={shrunk.length + t("\uAC1C \uD30C\uC77C \uC6D0\uBCF8 \uB300\uBE44 \uC6A9\uB7C9 \uAC10\uC18C\uBD84")}>
                          💾 <b>{fmtSize(saved)}</b>{t("\uC808\uC57D")}</span>}
                      {libFilter && <span className="ls-item filterhint">{t("\uD544\uD130 \uC911 \u2014 \uB2E4\uC2DC \uD074\uB9AD\uD558\uBA74 \uD574\uC81C")}</span>}
                    </>;
          })()}
              </div>}
            {visibleEntries.length > 0 ? <div className="libtable">
                <div className="libhead row">
                  <Th k="name" className="c-name">
                    File
                  </Th>
                  <Th k="folder" className="c-folder">
                    Folder
                  </Th>
                  <Th k="codec" className="c-codec">
                    Codec
                  </Th>
                  <span className="c-spec">Specs</span>
                  <Th k="size" className="c-size">
                    Size
                  </Th>
                  <Th k="duration" className="c-dur">
                    Duration
                  </Th>
                  <Th k="status" className="c-status">
                    Status
                  </Th>
                </div>
                {visibleEntries.map(e => <div key={(e.folder ? e.folder + "/" : "") + e.path} className="librow row">
                    <span className="c-name" title={e.path}>
                      <span className="jname">{e.name}</span>
                      {e.status === "done" && e.reencoded && <span className="jout">→ {e.reencoded}</span>}
                    </span>
                    <span className="c-folder jmeta" title={folderPath(e.folder)}>
                      {e.folder ? shortPath(folderPath(e.folder)) : t("(\uC784\uC2DC)")}
                    </span>
                    <span className="c-codec" title={`${e.videoCodec || "?"}${e.audioCodec ? ` / ${e.audioCodec}` : ""}` + (e.status === "done" && e.newCodec ? t("  \u2192  \uCD9C\uB825: ") + e.newCodec : "")}>
                      {e.videoCodec || "?"}
                      {e.audioCodec ? <span className="dim"> / {e.audioCodec}</span> : ""}
                      {e.status === "done" && e.newCodec && <span className="enc-new">
                          {" → "}
                          <b>{e.newCodec}</b>
                        </span>}
                    </span>
                    <span className="c-spec jmeta">
                      {e.width ? `${e.width}×${e.height}` : "—"}
                      {e.fps ? ` · ${fmtFps(e.fps)}fps` : ""}
                      {e.videoBitRate ? ` · ${fmtRate(e.videoBitRate)}` : ""}
                    </span>
                    <span className="c-size jmeta">
                      {e.status === "done" && e.newSize != null && e.size > 0 ? <span title={t("\uC6D0\uBCF8 \u2192 \uACB0\uACFC")}>
                          <span className="dim">{fmtSize(e.size)}</span>
                          {" → "}
                          {fmtSize(e.newSize)}
                          <span className={"szdelta " + (e.newSize <= e.size ? "down" : "up")}>
                            {e.newSize <= e.size ? `−${Math.max(1, Math.round((1 - e.newSize / e.size) * 100))}%` : `+${Math.round((e.newSize / e.size - 1) * 100)}%`}
                          </span>
                        </span> : fmtSize(e.size)}
                    </span>
                    <span className="c-dur jmeta">
                      {e.duration ? fmtDur(e.duration) : "—"}
                    </span>
                    <span className="c-status">
                      {libJob(e) ? (() => {
                const j = libJob(e);
                const pct = Math.round((j.progress || 0) * 100);
                return <span className="progcell" title={`${j.status} → ${j.effectiveVideoCodec || j.videoCodec} (${j.executedBy || "?"})`}>
                              <span className="prog">
                                <i style={{
                      width: pct + "%"
                    }} />
                              </span>
                              <span className="prog-pct">{pct}%</span>
                            </span>;
              })() : <span className="statusrow">
                          <span className={`status ${e.status === "done" ? "done" : e.status === "error" ? "error" : e.status === "failed" ? "failed" : e.status === "queued" ? "running" : e.status === "notvideo" ? "notvideo" : e.status === "skipped" ? "skipped" : ""}`} title={e.status === "done" ? t("\uC6D0\uBCF8: ") + fmtSize(e.size) + " " + [e.videoCodec, e.audioCodec].filter(Boolean).join("/") + t(" \u2192 \uACB0\uACFC: ") + (e.newSize != null ? fmtSize(e.newSize) : "?") + " " + (e.newCodec || "") + " (" + (e.reencoded || "?") + ")" : e.status === "error" || e.status === "failed" || e.status === "skipped" || e.status === "notvideo" ? e.error : undefined}>
                            {e.status === "notvideo" ? t("\uBE44\uC601\uC0C1") : e.status}
                          </span>
                          {(e.status === "done" || e.status === "error" || e.status === "skipped") && <button className="mini" title={t("\uC7AC\uC778\uCF54\uB529 \u2014 \uB2E4\uC2DC \uD050\uC5D0 \uB123\uAE30")} onClick={() => doRequeue(e)}>
                              ⟳
                            </button>}
                        </span>}
                      {e.status === "error" && e.error && <div className="errline" title={e.error}>
                          {e.error}
                        </div>}
                    </span>
                  </div>)}
              </div> : <div className="empty">
                {libFilter ? t("\uD604\uC7AC \uD544\uD130\uC5D0 \uD574\uB2F9\uD558\uB294 \uD30C\uC77C\uC774 \uC5C6\uC2B5\uB2C8\uB2E4 \u2014 \uCE69\uC744 \uB2E4\uC2DC \uD074\uB9AD\uD574 \uD574\uC81C\uD558\uC138\uC694.") : folders.some(f => f.enabled) ? t("\uC120\uD0DD\uD55C \uC2DC\uD2B8\uC5D0 \uC601\uC0C1\uC774 \uC5C6\uAC70\uB098 \uC544\uC9C1 \uC2A4\uCE94\uD558\uC9C0 \uC54A\uC558\uC2B5\uB2C8\uB2E4.") : t("\uD65C\uC131\uD654\uB41C \uD3F4\uB354\uAC00 \uC5C6\uC2B5\uB2C8\uB2E4 \u2014 \uC544\uB798 \uD3F4\uB354 \uAD00\uB9AC\uC5D0\uC11C \uCD94\uAC00\uD558\uACE0 [\uD65C\uC131\uD654]\uD558\uC138\uC694.")}
              </div>}
          </section>

          <section className="card auto">
            <h2>{t("\uD3F4\uB354 \uAD00\uB9AC")}</h2>
            <div className="hint">{t("\uAC10\uC2DC\uD560 \uD3F4\uB354(\uB77C\uC774\uBE0C\uB7EC\uB9AC)\uB97C \uCD94\uAC00/\uC81C\uAC70\uD569\uB2C8\uB2E4. \uC0C8 \uD3F4\uB354\uB294")}<b>{t("\uBE44\uD65C\uC131 \uC0C1\uD0DC\uB85C \uCD94\uAC00")}</b>{t("\uB429\uB2C8\uB2E4 \u2014 \uB85C\uC9C1/\uC778\uCF54\uB529 \uC124\uC815(\uC704 \uC2DC\uD2B8\uC5D0\uC11C)\uC744 \uB9C8\uCE58\uACE0")}<b>{t("[\uD65C\uC131\uD654]")}</b>{t("\uB97C \uB20C\uB7EC\uC57C \uD050\uC789\uC774 \uC2DC\uC791\uB429\uB2C8\uB2E4.")}</div>
            <div className="search">
              <input value={newFolder} onChange={e => setNewFolder(e.target.value)} onKeyDown={e => e.key === "Enter" && addFolder()} placeholder={t("\uD3F4\uB354 \uACBD\uB85C (\uC608: /mnt/nas/Videos)")} />
              <button onClick={addFolder} disabled={!newFolder.trim()}>{t("\uCD94\uAC00")}</button>
              <button onClick={openTree} title={t("\uD30C\uC77C\uC2DC\uC2A4\uD15C \uD2B8\uB9AC\uB85C \uD3F4\uB354 \uCC3E\uAE30")}>{t("\uD83D\uDD0D \uD3F4\uB354 \uCC3E\uC544\uBCF4\uAE30")}</button>
            </div>
            {folders.length === 0 && <div className="empty">{t("\uB4F1\uB85D\uB41C \uD3F4\uB354\uAC00 \uC5C6\uC2B5\uB2C8\uB2E4. \uC704 \uC785\uB825\uCC3D\uC5D0 \uD3F4\uB354 \uACBD\uB85C\uB97C \uB123\uACE0 \uCD94\uAC00\uD558\uC138\uC694.")}</div>}
            {folders.map(f => <div key={f.id} className={"folder" + (f.enabled ? "" : " off")}>
                <div className="row">
                  <button className={`enablebtn mini${f.enabled ? " on" : ""}`} onClick={() => patchFolder(f.id, x => ({
              ...x,
              enabled: !x.enabled
            }))} title={f.enabled ? t("\uBE44\uD65C\uC131\uD654 \u2014 \uD050\uC789 \uC911\uC9C0") : t("\uD65C\uC131\uD654 \u2014 \uC2A4\uCE94 \uC2DC \uC790\uB3D9 \uD050\uC789")}>
                    {f.enabled ? t("\u23F8 \uBE44\uD65C\uC131\uD654") : t("\u25B6 \uD65C\uC131\uD654")}
                  </button>
                  <code className="fpath" title={f.path}>
                    {f.path}
                  </code>
                  {f.logicId && <span className="logic-badge" title={t("\uB85C\uC9C1 \uC801\uC6A9 \uC911")}>
                      {logics.find(l => l.id === f.logicId)?.name || t("\uB85C\uC9C1")}
                    </span>}
                  <button className="mini" title={f.enabled ? t("\uC774 \uD3F4\uB354\uB9CC \uC2A4\uCE94 \u2014 \uC0C8 \uC601\uC0C1\uC740 \uB85C\uC9C1/\uC778\uCF54\uB529 \uC124\uC815\uB300\uB85C \uD050\uC789") : t("\uC774 \uD3F4\uB354\uB9CC \uC2A4\uCE94 \u2014 \uBE44\uD65C\uC131 \uD3F4\uB354\uB77C \uBAA9\uB85D\uC5D0\uB9CC \uB4E4\uC5B4\uAC00\uACE0 \uD050\uC5D0\uB294 \uC548 \uB4E4\uC5B4\uAC10")} disabled={scanning} onClick={() => doScan(f.id)}>
                    {scanning && scanFolder === f.id ? t("\uC2A4\uCE94 \uC911\u2026") : t("\uC2A4\uCE94")}
                  </button>
                  <button className="rm-del" title={t("\uD3F4\uB354 \uC81C\uAC70")} onClick={() => removeFolder(f.id)}>
                    ✕
                  </button>
                </div>
              </div>)}
            {folderMsg && <div className="meta ok-text">{folderMsg}</div>}
          </section>
        </>}

                  {/* ═══ 로직 — ComfyUI-style node graph automation ═══ */}
      {tab === "logic" && <>
          <section className="card auto">
            <h2>{t("\uC790\uB3D9\uD654 \uB85C\uC9C1")}</h2>
            <div className="hint">{t("\uB178\uB4DC \uADF8\uB798\uD504(\uC6CC\uD06C\uD50C\uB85C) \uBC29\uC2DD\uC785\uB2C8\uB2E4.")}<b>{t("\uC67C\uCABD \uD314\uB808\uD2B8\uC5D0\uC11C \uB178\uB4DC\uB97C \uD074\uB9AD")}</b>{t("\uD574 \uCE94\uBC84\uC2A4\uC5D0 \uCD94\uAC00\uD558\uACE0, \uC18C\uCF13(\u25CF)\uC744 \uB4DC\uB798\uADF8\uD574 \uC640\uC774\uC5B4\uB97C \uC774\uC5B4\uC8FC\uC138\uC694. \uC0C8 \uC601\uC0C1\uC774")}{" "}
              <b>{t("\uC2DC\uC791")}</b>{t("\uC5D0\uC11C \uB4E4\uC5B4\uC640 \uC640\uC774\uC5B4\uB97C \uB530\uB77C\uAC00\uB2E4\uAC00")}<b>{t("\uCCAB \uB3C4\uCC29\uD55C \uC561\uC158 \uB178\uB4DC")}</b>{t("\uC5D0\uC11C \uCC98\uB9AC\uB429\uB2C8\uB2E4. \uC870\uAC74 \uB178\uB4DC\uB294")}{" "}
              <b style={{
            color: "#35d07f"
          }}>{t("\u2713(\uB9DE\uC73C\uBA74)")}</b> /{" "}
              <b style={{
            color: "#e05555"
          }}>{t("\u2717(\uC548 \uB9DE\uC73C\uBA74)")}</b>{t("\uB450 \uAC08\uB798 \u2014 \uC9C1\uB82C\uC740 AND, \uB098\uB780\uD788\uB294 OR. \uC5B4\uB5A4 \uC561\uC158\uC5D0\uB3C4 \uBABB \uAC00\uBA74 \uD3F4\uB354 \uAE30\uBCF8 \uC124\uC815\uC744 \uC501\uB2C8\uB2E4.")}</div>
            <div className="gtoolbar">
              <label className="scopesel">{t("\uB85C\uC9C1")}<select value={activeLogic} onChange={e => selectLogic(e.target.value)}>
                  {logics.map(l => <option key={l.id} value={l.id}>
                      {l.name}
                    </option>)}
                  {!activeLogic && <option value="">{t("\uFF0B \uC0C8 \uB85C\uC9C1 (\uBBF8\uC800\uC7A5)")}</option>}
                </select>
              </label>
              <input className="logicname" value={logicName} placeholder={t("\uB85C\uC9C1 \uC774\uB984")} onChange={e => patchLogicName(e.target.value)} title={t("\uB85C\uC9C1 \uC774\uB984 \u2014 \uC800\uC7A5 \uBC84\uD2BC\uC73C\uB85C \uD568\uAED8 \uC801\uC6A9\uB429\uB2C8\uB2E4")} />
              <button onClick={() => selectLogic("")} disabled={!activeLogic} title={t("\uBE48 \uCE94\uBC84\uC2A4\uB85C \uC0C8 \uB85C\uC9C1 \uB9CC\uB4E4\uAE30")}>{t("\uC0C8 \uB85C\uC9C1")}</button>
              <button className={"savebtn" + (logicDirty ? " save-dirty" : "")} onClick={saveLogic} title={t("\uD604\uC7AC \uCE94\uBC84\uC2A4(\uB178\uB4DC\xB7\uC640\uC774\uC5B4\xB7\uC774\uB984)\uB97C \uC774 \uB85C\uC9C1\uC73C\uB85C \uC800\uC7A5 \u2014 \uC800\uC7A5 \uC804\uC5D0\uB294 \uC11C\uBC84\uC5D0 \uBC18\uC601\uB418\uC9C0 \uC54A\uC2B5\uB2C8\uB2E4")}>{t("\uD83D\uDCBE \uC800\uC7A5")}{logicDirty ? " *" : ""}
              </button>
              {logicDirty && <span className="meta dim" style={{
            color: "#e6b455"
          }}>{t("\uBCC0\uACBD\uC0AC\uD56D \uBBF8\uC800\uC7A5")}</span>}
              <button className="rm-del" onClick={deleteLogic} disabled={!activeLogic} title={t("\uC120\uD0DD\uB41C \uB85C\uC9C1 \uC0AD\uC81C")}>{t("\uC0AD\uC81C")}</button>
              <span className="meta dim">{t("\uB178\uB4DC: \uD5E4\uB354\uB85C \uB4DC\uB798\uADF8 \xB7 \uC81C\uBAA9 \uB354\uBE14\uD074\uB9AD = \uC774\uB984 \uBCC0\uACBD \xB7 \uC640\uC774\uC5B4 \uD074\uB9AD = \uB04A\uAE30 \xB7 \uCE94\uBC84\uC2A4 \uB4DC\uB798\uADF8 = \uC774\uB3D9 \xB7 \uD720 = \uD655\uB300/\uCD95\uC18C \u2014 \uC800\uC7A5\uB41C \uB85C\uC9C1\uC740")}{" "}
                <b>{t("\uB77C\uC774\uBE0C\uB7EC\uB9AC \uD0ED")}</b>{t("\uC5D0\uC11C \uD3F4\uB354\uC5D0 \uBC30\uC815\uD569\uB2C8\uB2E4")}</span>
              {blockMsg && <span className="meta ok-text">{blockMsg}</span>}
            </div>
            <div className="logic-flex">
              <aside className="gpals">
                <div className="gpal-sec">{t("\uC2DC\uC791")}</div>
                <Pal kind="start" disabled={graph.nodes.some(n => n.kind === "start")}>{t("\u25B6 \uC2DC\uC791")}<span>{t("\uC0C8 \uC601\uC0C1\uC774 \uBC1C\uACAC\uB418\uBA74")}</span>
                </Pal>
                <div className="gpal-sec">{t("\uD83D\uDCF9 \uC601\uC0C1 \uC870\uAC74")}</div>
                <Pal kind="cond" preset={{
              kind: "srcCodec"
            }}>{t("\uC6D0\uBCF8 \uCF54\uB371 =")}<span>h264 / hevc / av1 …</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "sizeGt"
            }}>{t("\uD30C\uC77C \uD06C\uAE30 >")}<span>{t("MB \uB2E8\uC704\uB85C")}</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "sizeLt"
            }}>{t("\uD30C\uC77C \uD06C\uAE30 <")}<span>{t("MB \uB2E8\uC704\uB85C")}</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "heightGte",
              value: 1080
            }}>{t("\uD574\uC0C1\uB3C4 \uC138\uB85C \u2265")}<span>720(HD) / 1080(FHD) / 1440(QHD) / 2160(4K)</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "bitrateGt"
            }}>{t("\uC6D0\uBCF8 \uBE44\uD2B8\uB808\uC774\uD2B8 >")}<span>{t("Mbps \uB2E8\uC704\uB85C")}</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "bitrateLt"
            }}>{t("\uC6D0\uBCF8 \uBE44\uD2B8\uB808\uC774\uD2B8 <")}<span>{t("Mbps \uB2E8\uC704\uB85C")}</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "nameContains"
            }}>{t("\uC774\uB984\uC5D0 \uD3EC\uD568")}<span>{t("\uC608: 4K, raw, .mkv")}</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "srcHealth"
            }}>{t("\uD5EC\uC2A4\uCCB4\uD06C")}<span>{t("\uD504\uB808\uC784 \uB514\uCF54\uB4DC \u2014 \uC804\uCCB4 / \uC55E\uBD80\uBD84 / \uBB34\uC791\uC704(\uC55E+\uB79C\uB364), \u2713 \uD1B5\uACFC / \u2717 \uC2E4\uD328")}</span>
                </Pal>
                <div className="gpal-sec">{t("\uD83D\uDDA5 \uB178\uB4DC(\uC11C\uBC84) \uC870\uAC74")}</div>
                <Pal kind="cond" preset={{
              kind: "nodeHasHw"
            }}>{t("HW \uAC00\uC18D \uC9C0\uC6D0")}<span>{t("\uCF54\uB371 + GPU/CPU \uAD6C\uBD84 \u2014 \uC608: \u201Cav1\uC744 GPU \uAC00\uC18D\uC73C\uB85C \uD558\uB294 \uB178\uB4DC\uAC00 \uC788\uC73C\uBA74\u201D")}</span>
                </Pal>
                <div className="gpal-sec">{t("\uD83E\uDDE9 \uD50C\uB7EC\uADF8\uC778 \uC870\uAC74")}</div>
                <Pal kind="cond" preset={{
              kind: "field"
            }}>{t("\uD544\uB4DC \uC870\uAC74")}<span>{t("\uC784\uC758 \uD504\uB85C\uBE0C \uD544\uB4DC \xD7 \uC5F0\uC0B0\uC790 \uBE44\uAD50 \u2014 \uD06C\uAE30/\uAE38\uC774/\uBE44\uD2B8\uB808\uC774\uD2B8/\uD574\uC0C1\uB3C4/fps/\uC774\uB984/\uCF54\uB371\u2026")}</span>
                </Pal>
                <Pal kind="cond" preset={{
              kind: "cmd"
            }}>{t("cmd \uC870\uAC74")}<span>{t("\uC178 \uC2A4\uD06C\uB9BD\uD2B8 \uC2E4\uD589 \u2014 exit 0 = \u2713 (\uD0C0\uC784\uC544\uC6C3 \uCD08 \uC9C0\uC815, \uC2A4\uD06C\uB9BD\uD2B8\uC5D0\uC11C \uBB50\uB4E0 \uAC80\uC0AC)")}</span>
                </Pal>
                <div className="gpal-sec">{t("\u2699 \uC561\uC158")}</div>
                <Pal kind="act" preset={{
              kind: "encode"
            }}>{t("\uC778\uCF54\uB529 \uC2DC\uC791")}<span>{t("\uCF54\uB371 \xB7 CRF \uB610\uB294 \uC6D0\uBCF8 %")}</span>
                </Pal>
                <Pal kind="act" preset={{
              kind: "failToFolder"
            }}>{t("\uC2E4\uD328 \uCC98\uB9AC")}<span>{t("\uC6D0\uBCF8\uC744 \uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC774\uB3D9 (\uC138\uD305 \uD0ED \uC9C0\uC815)")}</span>
                </Pal>
                <Pal kind="act" preset={{
              kind: "skip"
            }}>{t("\uAC74\uB108\uB6F0\uAE30")}<span>{t("\uC778\uCF54\uB529\uD558\uC9C0 \uC54A\uC74C")}</span>
                </Pal>
                <Pal kind="act" preset={{
              kind: "postCmd"
            }}>
                  postCmd
                  <span>{t("\uC778\uCF54\uB529 \uD6C4 \uC178 \uC2A4\uD06C\uB9BD\uD2B8 \uC2E4\uD589 \u2014 CT_OUTPUT/CT_INPUT/CT_STATUS env, \uC2E4\uD328\uD574\uB3C4 \uCCB4\uC778 \uACC4\uC18D")}</span>
                </Pal>
                <div className="gpal-sec">{t("\u2B07 \uC778\uCF54\uB529 \uC774\uD6C4 (\uC778\uCF54\uB529 \uCD9C\uB825 \uD3EC\uD2B8\uC5D0 \uC5F0\uACB0)")}</div>
                <Pal kind="act" preset={{
              kind: "verify"
            }}>{t("\uCD9C\uB825 \uAC80\uC99D")}<span>{t("\uCD9C\uB825 \uD30C\uC77C ffprobe \u2014 \uC804\uCCB4 / N% / N\uC5D0 1 \uC0D8\uD50C")}</span>
                </Pal>
                <Pal kind="act" preset={{
              kind: "replace"
            }}>{t("\uC6D0\uBCF8 \uB300\uCE58")}<span>{t("\uCD9C\uB825\uC774 \uC6D0\uBCF8 \uACBD\uB85C\uB97C \uB300\uCCB4 (\uC6D0\uBCF8\uC740 .orig \uBC31\uC5C5)")}</span>
                </Pal>
                <Pal kind="act" preset={{
              kind: "saveHere"
            }}>{t("\uC6D0\uBCF8 \uC704\uCE58\uC5D0 \uC800\uC7A5")}<span>{t("\uCD9C\uB825\uC744 \uC6D0\uBCF8 \uC606\uC5D0 \uB454\uB2E4 (\uAE30\uBCF8 \uB3D9\uC791\uC744 \uB178\uB4DC\uB85C \uBA85\uC2DC)")}</span>
                </Pal>
                <Pal kind="act" preset={{
              kind: "requeue"
            }}>{t("\uD050\uB85C \uB418\uB3CC\uB9AC\uAE30")}<span>{t("\uCCB4\uC778 \uC2E4\uD589 \uD6C4 \uB2E4\uC2DC \uD050\uC5D0 (\uC7AC\uC2A4\uCE94 \uC2DC \uC7AC\uCC98\uB9AC)")}</span>
                </Pal>
              </aside>
              <div className="gcanvas" ref={canvasRef} onMouseDown={e => onCanvasDown(e)} onTouchStart={e => onCanvasDown(asPointer(e))} onDragOver={e => e.preventDefault()} onDrop={onCanvasDrop}>
                <div className="gworld" style={{
              transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})`
            }}>
                  <svg className="gwires" width="10" height="10" style={{
                overflow: "visible"
              }}>
                    {graph.edges.map((e, i) => {
                  const a = graph.nodes.find(n => n.id === e.from);
                  const b = graph.nodes.find(n => n.id === e.to);
                  if (!a || !b) return null;
                  const [x1, y1] = [a.x + NODE_W, e.fromPort === "false" ? a.y + 33 : a.y + 15];
                  const [x2, y2] = [b.x, b.y + 15];
                  const d = wirePath(x1, y1, x2, y2);
                  const cls = e.fromPort === "false" ? "false" : e.fromPort === "true" ? "true" : "base";
                  return <g key={i}>
                          <path d={d} className={`gwire ${cls}`} />
                          <path d={d} className="ghit" onClick={() => removeEdgeAt(i)} />
                        </g>;
                })}
                    {pending && (() => {
                  const a = graph.nodes.find(n => n.id === pending.from);
                  if (!a) return null;
                  const x1 = a.x + NODE_W;
                  const y1 = pending.port === "false" ? a.y + 33 : a.y + 15;
                  return <path d={wirePath(x1, y1, pending.x, pending.y)} className="gwire tmp" />;
                })()}
                  </svg>
                  {graph.nodes.map(n => {
                const isCond = n.kind === "condition";
                const isAct = n.kind === "action";
                const mode = n.action?.target?.mode || "pct";
                const ch = n.action?.target?.choices || [];
                const atgt = n.action?.target || {};
                // choice-level rate wins; fall back to the target's rate
                const pMode = ch[0]?.mode || atgt.mode || "pct";
                return <div key={n.id} className={`gnode kind-${n.kind}${dragId === n.id ? " dragging" : ""}`} style={{
                  left: n.x,
                  top: n.y
                }}>
                        <div className="ghead" onMouseDown={e => onNodeDown(e, n)} onTouchStart={e => onNodeDown(asPointer(e), n)}>
                          <span className={"gchip " + (n.kind === "start" ? "c-start" : isCond ? "c-cond" : "c-act")}>
                            {n.kind === "start" ? t("\uC2DC\uC791") : isCond ? t("\uC870\uAC74") : t("\uC561\uC158")}
                          </span>
                          <span className="gtitle" title={n.label ? t("\uB354\uBE14\uD074\uB9AD: \uC774\uB984 \uBCC0\uACBD") : t("\uB354\uBE14\uD074\uB9AD: \uC774\uB984 \uBD99\uC774\uAE30")} onDoubleClick={() => {
                      const t = prompt(t("\uB178\uB4DC \uC774\uB984 (\uBE44\uC6B0\uBA74 \uC790\uB3D9 \uC774\uB984)"), n.label || "");
                      if (t !== null) patchNode(n.id, {
                        label: t.trim() || undefined
                      });
                    }}>
                            {nodeTitle(n)}
                          </span>
                          <span className="gfill" />
                          <label className="gchk" title={t("\uD65C\uC131/\uBE44\uD65C\uC131")} onMouseDown={e => e.stopPropagation()}>
                            <input type="checkbox" checked={!!n.enabled} onChange={() => patchNode(n.id, {
                        enabled: !n.enabled
                      })} />
                          </label>
                          {n.kind !== "start" && <button className="gdel" title={t("\uB178\uB4DC \uC0AD\uC81C")} onMouseDown={e => e.stopPropagation()} onClick={() => removeNode(n.id)}>
                              ✕
                            </button>}
                        </div>
                        <div className="gbody">
                          {n.kind === "start" && <div className="gsub">{t("\uC2A4\uCE94\uC5D0\uC11C \uC0C8 \uC601\uC0C1\uC774 \uBC1C\uACAC\uB418\uBA74")}</div>}
                          {isCond && <>
                              <select value={n.condition?.kind || "always"} onChange={e => patchCond(n.id, {
                        kind: e.target.value
                      })}>
                                <option value="always">{t("\uD56D\uC0C1")}</option>
                                <option value="srcCodec">{t("\uC6D0\uBCF8 \uCF54\uB371 =")}</option>
                                <option value="sizeGt">{t("\uD30C\uC77C \uD06C\uAE30 >")}</option>
                                <option value="sizeLt">{t("\uD30C\uC77C \uD06C\uAE30 <")}</option>
                                <option value="heightGte">{t("\uD574\uC0C1\uB3C4 \uC138\uB85C \u2265")}</option>
                                <option value="bitrateGt">{t("\uC6D0\uBCF8 \uBE44\uD2B8\uB808\uC774\uD2B8 >")}</option>
                                <option value="bitrateLt">{t("\uC6D0\uBCF8 \uBE44\uD2B8\uB808\uC774\uD2B8 <")}</option>
                                <option value="nameContains">{t("\uC774\uB984\uC5D0 \uD3EC\uD568")}</option>
                                <option value="srcHealth">{t("\uD5EC\uC2A4\uCCB4\uD06C (\uD504\uB808\uC784 \uB514\uCF54\uB4DC)")}</option>
                                <option value="nodeHasHw">{t("\uB178\uB4DC\uAC00 HW \uAC00\uC18D \uC9C0\uC6D0")}</option>
                                <option value="field">{t("\uD83E\uDDE9 \uD544\uB4DC \uC870\uAC74 (\uC784\uC758 \uD544\uB4DC \xD7 \uC5F0\uC0B0\uC790)")}</option>
                                <option value="cmd">{t("\uD83E\uDDE9 cmd \uC870\uAC74 (\uC2A4\uD06C\uB9BD\uD2B8 \uC2E4\uD589, exit 0 = \u2713)")}</option>
                              </select>
                              {n.condition?.kind === "srcHealth" && <>
                                  <label>{t("\uAC80\uC0AC \uAE4A\uC774")}<select value={n.condition?.healthMode || "front"} onChange={e => patchCond(n.id, {
                            healthMode: e.target.value
                          })}>
                                      <option value="front">{t("\uC55E\uBD80\uBD84 (N% \uD504\uB808\uC784)")}</option>
                                      <option value="random">{t("\uBB34\uC791\uC704 (\uC55E\uBD80\uBD84 + \uB79C\uB364\uC704\uCE58, \uAC01 N%)")}</option>
                                      <option value="all">{t("\uC804\uCCB4 (\uD30C\uC77C \uC804\uCCB4 \uB514\uCF54\uB4DC)")}</option>
                                    </select>
                                  </label>
                                  {(n.condition?.healthMode || "front") !== "all" && <label>
                                      N
                                      <input type="number" className="num" min={1} max={100} value={n.condition?.healthValue ?? 10} onChange={e => patchCond(n.id, {
                            healthValue: Number(e.target.value)
                          })} />
                                      %
                                    </label>}
                                  <div className="gsub" title={t("\uAC01 \uD30C\uC77C\uC744 ffmpeg \uB514\uCF54\uB4DC\uB85C \uAC80\uC0AC \u2014 \uC9C0\uC815\uD55C \uBC94\uC704 \uC5B4\uB514\uB4E0 \uB514\uCF54\uB529 \uC624\uB958\uAC00 \uC788\uC73C\uBA74 \uC2E4\uD328. \uBAA8\uB4E0 \uD30C\uC77C \uAC80\uC0AC (\uC0D8\uD50C\uB9C1 \uC5C6\uC74C)")}>{t("\u2713 \uD1B5\uACFC / \u2717 \uC2E4\uD328 \u2014 \u2717\uC5D0")}<b>{t("\uC2E4\uD328 \uCC98\uB9AC")}</b>{t("\uB178\uB4DC\uB97C \uC5F0\uACB0\uD558\uBA74 \uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC774\uB3D9")}</div>
                                </>}
                              {n.condition?.kind === "srcCodec" && <div className="chips codecpick">
                                  {[["h264", "H.264"], ["hevc", "HEVC"], ["av1", "AV1"], ["vp9", "VP9"], ["mpeg4", "MPEG-4"]].map(([id, label]) => {
                          const cur = n.condition?.codecs || (n.condition?.codec ? [n.condition.codec] : []);
                          const on = cur.includes(id);
                          return <button key={id} type="button" className={"chipbtn chip " + (CODEC_CHIPS[id]?.cls || "c-other") + (on ? " on" : "")} onClick={() => patchCond(n.id, {
                            codecs: on ? cur.filter(x => x !== id) : [...cur, id]
                          })}>
                                        {label}
                                      </button>;
                        })}
                                  <span className="gsub" title={t("\uC5EC\uB7EC \uAC1C \uC120\uD0DD \uAC00\uB2A5 \u2014 \uC120\uD0DD\uD55C \uCF54\uB371 \uC911 \uD558\uB098\uC640 \uC77C\uCE58\uD558\uBA74 \u2713. \uC544\uBB34\uAC83\uB3C4 \uC548 \uACE0\uB974\uBA74 \uC5B4\uB5A4 \uCF54\uB371\uC774\uB4E0 \uD1B5\uACFC")}>{t("\uC5EC\uB7EC \uC120\uD0DD \uAC00\uB2A5 (\uC548 \uACE0\uB974\uBA74 \uC804\uBD80)")}</span>
                                </div>}
                              {(n.condition?.kind === "sizeGt" || n.condition?.kind === "sizeLt") && <span className="lunit">
                                  <input type="number" className="num" min="0" value={n.condition?.value ?? ""} onChange={e => patchCond(n.id, {
                          value: e.target.value === "" ? null : Number(e.target.value)
                        })} />
                                  MB
                                </span>}
                              {n.condition?.kind === "heightGte" && <>
                                  <select value={[720, 1080, 1440, 2160].includes(Number(n.condition?.value)) ? String(n.condition?.value) : ""} onChange={e => e.target.value && patchCond(n.id, {
                          value: Number(e.target.value)
                        })} title={t("\uC138\uB85C \uD574\uC0C1\uB3C4 \uAE30\uC900 (1080+ = 1080px \uC774\uC0C1)")}>
                                    <option value="">{t("\uC9C1\uC811 \uC785\uB825\u2026")}</option>
                                    <option value="720">{t("720 (HD \uC774\uC0C1)")}</option>
                                    <option value="1080">{t("1080 (FHD \uC774\uC0C1)")}</option>
                                    <option value="1440">{t("1440 (QHD \uC774\uC0C1)")}</option>
                                    <option value="2160">{t("2160 (4K \uC774\uC0C1)")}</option>
                                  </select>
                                  <span className="lunit">
                                    <input type="number" className="num" min="100" step="10" value={n.condition?.value ?? ""} onChange={e => patchCond(n.id, {
                            value: e.target.value === "" ? null : Number(e.target.value)
                          })} />
                                    px
                                  </span>
                                </>}
                              {(n.condition?.kind === "bitrateGt" || n.condition?.kind === "bitrateLt") && <span className="lunit">
                                  <input type="number" className="num" min="0" value={n.condition?.value ?? ""} onChange={e => patchCond(n.id, {
                          value: e.target.value === "" ? null : Number(e.target.value)
                        })} />
                                  Mbps
                                </span>}
                              {n.condition?.kind === "nameContains" && <input type="text" className="txt" placeholder={t("\uC608: 4K, raw, .mkv")} value={n.condition?.pattern || ""} onChange={e => patchCond(n.id, {
                        pattern: e.target.value || null
                      })} />}
                              {n.condition?.kind === "nodeHasHw" && <>
                                  <select value={n.condition?.hwFamily || "av1"} onChange={e => patchCond(n.id, {
                          hwFamily: e.target.value
                        })}>
                                    {["h264", "h265", "av1", "vp9"].map(fam => <option key={fam} value={fam}>
                                        {fam}
                                      </option>)}
                                  </select>
                                  <select value={n.condition?.accelClass || ""} onChange={e => patchCond(n.id, {
                          accelClass: e.target.value || null
                        })}>
                                    <option value="">{t("\uAC00\uC18D\uAE30: \uC5B4\uB5A4 \uAC83\uC774\uB4E0")}</option>
                                    <option value="gpu">{t("GPU \uC804\uC6A9 (nvenc/videotoolbox)")}</option>
                                    <option value="cpu">{t("iGPU \uC804\uC6A9 (qsv/vaapi)")}</option>
                                  </select>
                                </>}
                              {n.condition?.kind === "field" && <>
                                  <label>{t("\uD544\uB4DC")}<select value={n.condition?.field || "size"} onChange={e => patchCond(n.id, {
                            field: e.target.value
                          })}>
                                      <option value="size">{t("\uD30C\uC77C \uD06C\uAE30 (MB)")}</option>
                                      <option value="duration_s">{t("\uAE38\uC774 (\uCD08)")}</option>
                                      <option value="width">{t("\uD574\uC0C1\uB3C4 \uAC00\uB85C (px)")}</option>
                                      <option value="height">{t("\uD574\uC0C1\uB3C4 \uC138\uB85C (px)")}</option>
                                      <option value="fps">FPS</option>
                                      <option value="bitrate">{t("\uC601\uC0C1 \uBE44\uD2B8\uB808\uC774\uD2B8 (Mbps)")}</option>
                                      <option value="audio_bit_rate">{t("\uc624\ub514\uc624 \ube44\ud2b8\ub808\uc774\ud2b8 (Mbps)")}</option>
                                      <option value="name">{t("\uC774\uB984")}</option>
                                      <option value="codec">{t("\uC601\uC0C1 \uCF54\uB371")}</option>
                                      <option value="container">{t("\uCEE8\uD14C\uC774\uB108")}</option>
                                      <option value="pixel_fmt">{t("\uD53D\uC140 \uD3EC\uB9F7")}</option>
                                    </select>
                                  </label>
                                  <label>{t("\uC5F0\uC0B0\uC790")}<select value={n.condition?.op || "gte"} onChange={e => patchCond(n.id, {
                            op: e.target.value
                          })}>
                                      <option value="gt">{t("> \uD06C\uB2E4")}</option>
                                      <option value="gte">{t("\u2265 \uD06C\uAC70\uB098 \uAC19\uC74C")}</option>
                                      <option value="lt">{t("< \uC791\uB2E4")}</option>
                                      <option value="lte">{t("\u2264 \uC791\uAC70\uB098 \uAC19\uC74C")}</option>
                                      <option value="eq">{t("= \uAC19\uC74C")}</option>
                                      <option value="neq">{t("\u2260 \uB2E4\uB984")}</option>
                                      <option value="contains">{t("\uD3EC\uD568 (\uBB38\uC790\uC5F4)")}</option>
                                      <option value="startsWith">{t("\uC2DC\uC791 (\uBB38\uC790\uC5F4)")}</option>
                                      <option value="endsWith">{t("\uB05D\uB0A8 (\uBB38\uC790\uC5F4)")}</option>
                                    </select>
                                  </label>
                                  <label>{t("값")}{["name", "codec", "container", "pixel_fmt"].includes(n.condition?.field) ? <input type="text" value={n.condition?.pattern || ""} placeholder={t("예: hevc / mkv / 10bit")} onChange={e => patchCond(n.id, {
                            pattern: e.target.value || null
                          })} /> : <input type="number" step="any" value={n.condition?.value ?? 0} onChange={e => patchCond(n.id, {
                            value: e.target.value
                          })} />}
                                    <span>{t("\uC218\uCE58 \uD544\uB4DC = \uC22B\uC790 \xB7 \uBB38\uC790\uC5F4 \uD544\uB4DC = \uD14D\uC2A4\uD2B8 (\uCF54\uB371\uC740 h264/hevc/av1 \uACC4\uC5F4 \uC774\uB984\uC73C\uB85C \uC815\uADDC\uD654\uB428)")}</span>
                                  </label>
                                </>}
                              {n.condition?.kind === "cmd" && <>
                                  <label>{t("\uBA85\uB839\uC5B4")}<textarea rows={4} value={n.condition?.cmd || ""} placeholder={t("\uC608: test \"$CT_SIZE_MB\" -lt 2000 && grep -qiE \"sample|trailer\" \"$CT_FILE\"")} onChange={e => patchCond(n.id, {
                            cmd: e.target.value
                          })} />
                                    <span>{t("\uc178 \uba85\ub839 \u2014 exit 0 = \u2713. env: CT_FILE CT_NAME CT_DIR CT_SIZE_MB CT_DURATION_S CT_CODEC CT_WIDTH CT_HEIGHT CT_FPS CT_BITRATE_MBPS CT_CONTAINER CT_PIXFMT \u2014 \uc2a4\ud06c\ub9bd\ud2b8\ub294 \uc808\ub300 \uacbd\ub85c\ub85c \ud638\ucd9c (PATH\ub85c\ub294 \uc548\ub428)")}</span>
                                  </label>
                                  <label>{t("\uD0C0\uC784\uC544\uC6C3 (\uCD08)")}<input type="number" min="1" max="300" value={n.condition?.cmdTimeout ?? 30} onChange={e => patchCond(n.id, {
                            cmdTimeout: e.target.value
                          })} />
                                    <span>{t("\uCD08\uACFC \uC2DC \u2717 (\uAE30\uBCF8 30, \uCD5C\uB300 300)")}</span>
                                  </label>
                                </>}
                            </>}
                          {isAct && <>
                              <select value={n.action?.kind || "encode"} onChange={e => patchAct(n.id, {
                        kind: e.target.value
                      })}>
                                <option value="encode">{t("\uC778\uCF54\uB529 \uC2DC\uC791")}</option>
                                <option value="skip">{t("\uAC74\uB108\uB6F0\uAE30")}</option>
                                <option value="failToFolder">{t("\uD83D\uDDC4 \uC2E4\uD328 \uCC98\uB9AC (\uBCF4\uAD00 \uD3F4\uB354\uB85C \uC774\uB3D9)")}</option>
                                <option value="verify">{t("\u2B07 \uCD9C\uB825 \uAC80\uC99D")}</option>
                                <option value="replace">{t("\u2B07 \uC6D0\uBCF8 \uB300\uCE58")}</option>
                                <option value="saveHere">{t("\u2B07 \uC6D0\uBCF8 \uC704\uCE58\uC5D0 \uC800\uC7A5")}</option>
                                <option value="requeue">{t("\u2B07 \uD050\uB85C \uB418\uB3CC\uB9AC\uAE30")}</option>
                                <option value="postCmd">{t("\u2699\uFE0F postCmd (\uC778\uCF54\uB529 \uD6C4 \uC2A4\uD06C\uB9BD\uD2B8)")}</option>
                              </select>
                              {n.action?.kind === "encode" && <>
                                  <label className="postenc-opt" title={t("HW \uAC00\uC18D\uC73C\uB85C \uC774 \uCF54\uB371\uC744 \uD560 \uC218 \uC5C6\uB294 \uB178\uB4DC\uB294 \uC544\uC608 \uBC1B\uC9C0 \uC54A\uC74C \u2014 \uC18C\uD504\uD2B8\uC6E8\uC5B4 \uD3F4\uBC31\uB3C4 \uC5C6\uC74C. \uC628\uB77C\uC778 \uB178\uB4DC \uC911 \uAC00\uB2A5\uD55C \uACF3\uC774 \uC5C6\uC73C\uBA74 \uD30C\uC77C\uC740 \uC5D0\uB7EC\uB85C \uD45C\uC2DC (\uB178\uB4DC\uAC00 \uC62C\uB77C\uC624\uBA74 \uC7AC\uC2A4\uCE94\uC73C\uB85C \uC7AC\uC2DC\uB3C4)")}>
                                    <input type="checkbox" checked={!!n.action?.hwOnly} onChange={e => patchAct(n.id, {
                            hwOnly: e.target.checked
                          })} />{t("\u26A1 HW \uC804\uC6A9 (\uBE44\uC9C0\uC6D0 \uB178\uB4DC\uC5D0 \uC548 \uBCF4\uB0C4)")}</label>
                                  {n.action?.hwOnly && (() => {
                          const a = n.action || {};
                          const codecs = a.target?.videoCodec ? [a.target.videoCodec] : (a.choices || []).map(c => c.videoCodec).filter(Boolean);
                          if (!codecs.length) return null; // 폴더 기본 — 코덱 불명
                          const online = nodes.filter(x => x.status === "online");
                          const missing = [...new Set(codecs.map(codecFamily))].filter(fam => !online.some(x => (x.hwEncoders || []).some(h => codecFamily(h) === fam)));
                          if (!missing.length) return null;
                          return <div className="warnchip" title={t("\uC628\uB77C\uC778 \uB178\uB4DC: ") + (online.map(x => x.name).join(", ") || "없음") + "\n" + missing.join("/") + t(" \uD558\uB4DC\uC6E8\uC5B4 \uC778\uCF54\uB354\uB97C \uAC00\uC9C4 \uB178\uB4DC\uAC00 \uC5C6\uC2B5\uB2C8\uB2E4 \u2014 \uC774 \uAC08\uB798\uB85C \uC628 \uD30C\uC77C\uC740 \uC5D0\uB7EC \uCC98\uB9AC\uB429\uB2C8\uB2E4.\n\u26A1 HW \uC804\uC6A9\uC744 \uD574\uC81C\uD558\uBA74 \uC18C\uD504\uD2B8\uC6E8\uC5B4 \uC778\uCF54\uB529(\uB290\uB9BC)\uC73C\uB85C \uC9C4\uD589\uD569\uB2C8\uB2E4.")}>
                                          ⚠ {missing.join("/")}{t("HW \uC778\uCF54\uB354 \uB178\uB4DC \uC5C6\uC74C \u2014 \uC5D0\uB7EC \uCC98\uB9AC\uB429\uB2C8\uB2E4")}</div>;
                        })()}
                                </>}
                              {n.action?.kind === "failToFolder" && <div className="phint">{t("\uC870\uAC74 \uB178\uB4DC\uC758")}<b>{t("\u2717(\uC2E4\uD328)")}</b>{t("\uAC08\uB798\uC5D0 \uC5F0\uACB0\uD558\uC138\uC694 \u2014 \uC774 \uD30C\uC77C\uC740 \uC778\uCF54\uB529\uD558\uC9C0 \uC54A\uACE0")}<b>{t("\uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354")}</b>{t("(\uC138\uD305 \uD0ED\uC5D0\uC11C \uC9C0\uC815)\uB85C \uC774\uB3D9\uB429\uB2C8\uB2E4.")}{" "}
                                  <code>{t("\uBCF4\uAD00\uD3F4\uB354/\uB77C\uC774\uBE0C\uB7EC\uB9AC\uBA85/\u2026")}</code>
                                </div>}
                              {n.action?.kind === "verify" && <>
                                  <label>{t("\uAC80\uC99D \uBC94\uC704")}<select value={n.action?.verifyMode || "all"} onChange={e => patchAct(n.id, {
                            verifyMode: e.target.value
                          })}>
                                      <option value="all">{t("\uC804\uCCB4 (\uBAA8\uB4E0 \uD30C\uC77C)")}</option>
                                      <option value="pct">{t("N% \uBB34\uC791\uC704 \uC0D8\uD50C")}</option>
                                      <option value="every">{t("N\uC5D0 1 (\uAC19\uC740 \uD30C\uC77C \uB9E4\uBC88 \uB3D9\uC77C)")}</option>
                                    </select>
                                  </label>
                                  {(n.action?.verifyMode === "pct" || n.action?.verifyMode === "every") && <label>
                                      N
                                      <input type="number" className="num" min={n.action?.verifyMode === "pct" ? 1 : 1} max={n.action?.verifyMode === "pct" ? 100 : 9999} value={n.action?.verifyValue ?? (n.action?.verifyMode === "pct" ? 50 : 3)} onChange={e => patchAct(n.id, {
                            verifyValue: Number(e.target.value)
                          })} />
                                    </label>}
                                  <label title={t("\uAC80\uC99D \uC2E4\uD328 \uC2DC: \uC5D0\uB7EC\uB85C \uD45C\uC2DC(\uAE30\uBCF8) \xB7 \uD050\uB85C \uB418\uB3CC\uB824 \uC7AC\uC2DC\uB3C4 \xB7 \uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC6D0\uBCF8 \uC774\uB3D9")}>{t("\uC2E4\uD328 \uC2DC")}<select value={n.action?.verifyOnFailure || "error"} onChange={e => patchAct(n.id, {
                            verifyOnFailure: e.target.value === "error" ? null : e.target.value
                          })}>
                                      <option value="error">{t("\uC5D0\uB7EC \uD45C\uC2DC")}</option>
                                      <option value="requeue">{t("\uD050\uB85C \uB418\uB3CC\uB9AC\uAE30 (\uC7AC\uC2DC\uB3C4)")}</option>
                                      <option value="failFolder">{t("\uD83D\uDDC4 \uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC774\uB3D9")}</option>
                                    </select>
                                  </label>
                                </>}
                              {n.action?.kind === "replace" && <div className="phint">{t("\uC778\uCF54\uB529 \uCD9C\uB825 \uD3EC\uD2B8\uC5D0 \uC5F0\uACB0 \u2014 \uCD9C\uB825\uC774 \uC6D0\uBCF8\uC758 \uACBD\uB85C\uB97C \uB300\uCCB4\uD558\uACE0, \uC6D0\uBCF8\uC740 \uC606\uC5D0 .orig\uC73C\uB85C \uBC31\uC5C5\uB429\uB2C8\uB2E4.")}</div>}
                              {n.action?.kind === "saveHere" && <div className="phint">{t("\uCD9C\uB825\uC744 \uC6D0\uBCF8 \uC606\uC5D0 \uB454 \uCC44\uB85C \uB9C8\uBB34\uB9AC (\uB77C\uC774\uBE0C\uB7EC\uB9AC \uC791\uC5C5\uC758 \uAE30\uBCF8 \uB3D9\uC791\uC744 \uADF8\uB798\uD504\uC5D0 \uBA85\uC2DC\uC801\uC73C\uB85C \uD45C\uC2DC).")}</div>}
                              {n.action?.kind === "requeue" && <div className="phint">{t("\uCCB4\uC778 \uC2E4\uD589 \uD6C4 \uD30C\uC77C\uC744 \uB2E4\uC2DC \uD050\uC5D0 \uB461\uB2C8\uB2E4 \u2014 \uC7AC\uC2A4\uCE94 \uC2DC \uAC19\uC740 \uB85C\uC9C1\uC774 \uC801\uC6A9\uB418\uBA74 \uC7AC\uC778\uCF54\uB529\uB418\uB2C8, \uB2E4\uB978 \uD3F4\uB354 \uB85C\uC9C1\uACFC \uC870\uD569\uD574 \uC0AC\uC6A9\uD558\uC138\uC694.")}</div>}
                              {n.action?.kind === "postCmd" && <>
                                  <label>{t("\uBA85\uB839\uC5B4")}<textarea rows={4} value={n.action?.cmd || ""} placeholder={t("\uC608: /usr/local/bin/notify-done.sh \"$CT_OUTPUT_NAME\" \"$CT_OUTPUT_SIZE_MB\"")} onChange={e => patchAct(n.id, {
                            cmd: e.target.value
                          })} />
                                    <span>{t("\uC778\uCF54\uB529\uC774 \uC131\uACF5\uD55C \uB4A4 \uCCB4\uC778 \uC21C\uC11C\uB300\uB85C \uC2E4\uD589 (\uC778\uCF54\uB529 \uCD9C\uB825 \uD3EC\uD2B8\uC5D0 \uC5F0\uACB0). env: CT_OUTPUT(\uCD9C\uB825 \uC808\uB300\uACBD\uB85C) CT_OUTPUT_NAME CT_OUTPUT_SIZE_MB CT_INPUT CT_INPUT_NAME CT_STATUS(ok/error) + \uC6D0\uBCF8 \uBA54\uD0C0 \uBCC0\uC218(CT_NAME CT_SIZE_MB CT_CODEC \u2026) \u2014 \uC2E4\uD328\uD574\uB3C4 \uCCB4\uC778\uC740 \uACC4\uC18D\uB418\uACE0 \uACBD\uACE0\uB9CC \uAE30\uB85D (\uBE44\uCE58\uBA85)")}</span>
                                  </label>
                                  <label>{t("\uD0C0\uC784\uC544\uC6C3 (\uCD08)")}<input type="number" min="1" max="300" value={n.action?.cmdTimeout ?? 30} onChange={e => patchAct(n.id, {
                            cmdTimeout: e.target.value
                          })} />
                                    <span>{t("\uCD08\uACFC \uC2DC \uAC15\uC81C \uC885\uB8CC + \uACBD\uACE0 (\uAE30\uBCF8 30, \uCD5C\uB300 300)")}</span>
                                  </label>
                                </>}
                              {n.action?.kind === "encode" && (ch.length ? <>
                                    <div className="gprio-head">
                                      <span>{t("\uC778\uCF54\uB354 \uC6B0\uC120\uC21C\uC704 (\uB178\uB4DC \uB2A5\uB825 \uC790\uB3D9)")}</span>
                                      <button type="button" className="gprio-mini" title={t("\uB2E8\uC77C \uCF54\uB371\uC73C\uB85C")} onClick={() => patchActTarget(n.id, {
                            videoCodec: ch[0]?.videoCodec || null,
                            mode: ch[0]?.mode || "pct",
                            crf: ch[0]?.crf,
                            bitratePct: ch[0]?.bitratePct,
                            choices: []
                          })}>{t("\uB2E8\uC77C")}</button>
                                    </div>
                                    {ch.map((c, i) => <div className="gchoice" key={i}>
                                        <span className="gprio-idx">{i + 1}</span>
                                        <select value={c.videoCodec || ""} onChange={e => patchChoice(n.id, i, {
                            videoCodec: e.target.value || null
                          })}>
                                          <option value="">{t("\uCF54\uB371")}</option>
                                          {(codecs?.videoCodecs || []).filter(c2 => c2.id !== "copy").map(c2 => <option key={c2.id} value={c2.id}>
                                                {c2.label}
                                              </option>)}
                                        </select>
                                        <select value={c.accel || ""} onChange={e => patchChoice(n.id, i, {
                            accel: e.target.value || null
                          })}>
                                          <option value="">{t("HW: \uC5B4\uB5A4 \uAC83")}</option>
                                          <option value="gpu">GPU</option>
                                          <option value="cpu">iGPU</option>
                                          <option value="nvenc">NVENC</option>
                                          <option value="videotoolbox">VideoToolbox</option>
                                          <option value="vaapi">VA-API</option>
                                          <option value="qsv">QuickSync</option>
                                          <option value="amf">AMF</option>
                                        </select>
                                        <button type="button" className="gprio-del" title={t("\uC120\uD0DD \uC0AD\uC81C")} onClick={() => patchChoiceDel(n.id, i)}>
                                          ✕
                                        </button>
                                      </div>)}
                                    <div className="rateval">
                                      <div className="ratemode">
                                        <button type="button" className={`rm-opt ${pMode === "crf" ? "on" : ""}`} onClick={() => patchAllChoiceRate(n.id, {
                              mode: "crf"
                            })}>
                                          CRF
                                        </button>
                                        <button type="button" className={`rm-opt ${pMode !== "crf" ? "on" : ""}`} onClick={() => patchAllChoiceRate(n.id, {
                              mode: "pct"
                            })}>
                                          %
                                        </button>
                                      </div>
                                      <input type="number" className="num" min={pMode === "crf" ? 0 : 10} max={pMode === "crf" ? 51 : 100} value={pMode === "crf" ? ch[0]?.crf ?? atgt.crf ?? 23 : ch[0]?.bitratePct ?? atgt.bitratePct ?? 70} onChange={e => patchAllChoiceRate(n.id, pMode === "crf" ? {
                            crf: Number(e.target.value)
                          } : {
                            bitratePct: Number(e.target.value)
                          })} />
                                    </div>
                                    <button type="button" className="gprio-add" onClick={() => patchChoiceAdd(n.id)}>{t("+ \uC778\uCF54\uB354 \uCD94\uAC00 (fallback)")}</button>
                                  </> : <>
                                    <label>{t("\uBE44\uB514\uC624")}<select value={n.action?.target?.videoCodec || ""} onChange={e => patchActTarget(n.id, {
                            videoCodec: e.target.value || null
                          })}>
                                        <option value="">{t("\uD3F4\uB354 \uAE30\uBCF8")}</option>
                                        {(codecs?.videoCodecs || []).filter(c => c.id !== "copy").map(c => <option key={c.id} value={c.id}>
                                              {c.label}
                                            </option>)}
                                      </select>
                                    </label>
                                    <label>{t("\uC624\uB514\uC624")}<select value={n.action?.target?.audioCodec || ""} onChange={e => patchActTarget(n.id, {
                            audioCodec: e.target.value || null
                          })}>
                                        <option value="">{t("\uC6D0\uBCF8 \uC720\uC9C0")}</option>
                                        <option value="aac">AAC</option>
                                        <option value="opus">Opus</option>
                                      </select>
                                    </label>
                                    <div className="rateval">
                                      <div className="ratemode">
                                        <button type="button" className={`rm-opt ${mode === "crf" ? "on" : ""}`} onClick={() => patchActTarget(n.id, {
                              mode: "crf"
                            })}>
                                          CRF
                                        </button>
                                        <button type="button" className={`rm-opt ${mode !== "crf" ? "on" : ""}`} onClick={() => patchActTarget(n.id, {
                              mode: "pct"
                            })}>
                                          %
                                        </button>
                                      </div>
                                      <input type="number" className="num" min={mode === "crf" ? 0 : 10} max={mode === "crf" ? 51 : 100} value={mode === "crf" ? n.action?.target?.crf ?? 23 : n.action?.target?.bitratePct ?? 70} onChange={e => patchActTarget(n.id, mode === "crf" ? {
                            crf: Number(e.target.value)
                          } : {
                            bitratePct: Number(e.target.value)
                          })} />
                                    </div>
                                    <button type="button" className="gprio-add" onClick={() => enablePriority(n.id)}>{t("\u2195 \uC778\uCF54\uB354 \uC6B0\uC120\uC21C\uC704 \uBAA9\uB85D")}</button>
                                  </>)}
                              {n.action?.kind === "encode" && <div className="postenc">
                                  <span className="phint" title={t("\uC778\uCF54\uB529 \uCD9C\uB825 \uD3EC\uD2B8(\uC6B0\uCE21)\uC5D0 \u2018\uCD9C\uB825 \uAC80\uC99D\u2019, \u2018\uC6D0\uBCF8 \uB300\uCE58\u2019, \u2018\uD050\uB85C \uB418\uB3CC\uB9AC\uAE30\u2019 \uAC19\uC740 \uB178\uB4DC\uB97C \uC774\uC5B4 \uBD99\uC774\uBA74 \uC644\uB8CC \uD6C4 \uADF8 \uC21C\uC11C\uB85C \uC2E4\uD589\uB429\uB2C8\uB2E4")}>{t("\u2B07 \uC644\uB8CC \uD6C4 \uC791\uC5C5 \u2014 \uC6B0\uCE21 \uCD9C\uB825 \uD3EC\uD2B8\uC5D0 \uAC80\uC99D\xB7\uB300\uCE58 \uB178\uB4DC\uB97C \uC5F0\uACB0\uD558\uC138\uC694")}</span>
                                </div>}
                            </>}
                        </div>
                        {n.kind !== "start" && <div className="gsock in" data-node-in={n.id} title={t("\uC785\uB825 \uC18C\uCF13")} />}
                        {n.kind === "start" && <div className="gsock out" style={{
                    top: 9
                  }} onMouseDown={e => onSocketDown(e, n, "")} onTouchStart={e => onSocketDown(asPointer(e), n, "")} title={t("\uCD9C\uB825 \u2014 \uC0C8 \uC601\uC0C1")} />}
                        {n.kind === "action" && <div className="gsock out" style={{
                    top: 9
                  }} onMouseDown={e => onSocketDown(e, n, "")} onTouchStart={e => onSocketDown(asPointer(e), n, "")} title={t("\uCD9C\uB825 \u2014 \u2018\uC778\uCF54\uB529 \uC774\uD6C4\u2019 \uB178\uB4DC(\uAC80\uC99D/\uB300\uCE58/\u2026)\uB97C \uC5F0\uACB0")} />}
                        {isCond && <>
                            <div className="gsock out t" style={{
                      top: 9
                    }} onMouseDown={e => onSocketDown(e, n, "true")} onTouchStart={e => onSocketDown(asPointer(e), n, "true")} title={t("\u2713 \uC870\uAC74\uC774 \uB9DE\uC73C\uBA74")} />
                            <div className="gsock out f" style={{
                      top: 27
                    }} onMouseDown={e => onSocketDown(e, n, "false")} onTouchStart={e => onSocketDown(asPointer(e), n, "false")} title={t("\u2717 \uC870\uAC74\uC774 \uC548 \uB9DE\uC73C\uBA74")} />
                            <div className="glab t" style={{
                      top: 9
                    }}>✓</div>
                            <div className="glab f" style={{
                      top: 27
                    }}>✗</div>
                          </>}
                      </div>;
              })}
                </div>
                <div className="gzoom">
                  <button onClick={() => zoomBy(1 / 1.2)} title={t("\uCD95\uC18C")}>−</button>
                  <span>{Math.round(view.k * 100)}%</span>
                  <button onClick={() => zoomBy(1.2)} title={t("\uD655\uB300")}>+</button>
                  <button onClick={() => setView({
                x: 20,
                y: 10,
                k: 1
              })} title={t("100%\uB85C \uC7AC\uC124\uC815")}>
                    ⟲
                  </button>
                </div>
              </div>
            </div>
          </section>
        </>}

      {/* ═══ 실패 보관함 — quarantined files (failed health checks) ═══ */}
      {tab === "failures" && <section className="card">
          <h2>{t("\uC2E4\uD328 \uBCF4\uAD00\uD568")}</h2>
          <div className="hint">
            <b>{t("\uD5EC\uC2A4\uCCB4\uD06C")}</b>{t("\uB610\uB294")}<b>{t("\uCD9C\uB825 \uAC80\uC99D")}</b>{t("\uC744 \uD1B5\uACFC\uD558\uC9C0 \uBABB\uD574")}{" "}
            <b>{t("\uC2E4\uD328 \uCC98\uB9AC")}</b>{t("\uB41C \uC601\uC0C1\uC774 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC774\uB3D9\uB429\uB2C8\uB2E4. \uAD6C\uC870:")}{" "}
            <code>
              {failures.failDir ? failures.failDir : t("\uC138\uD305\uC5D0\uC11C \uC9C0\uC815")}{t("/\uB77C\uC774\uBE0C\uB7EC\uB9AC\uD3F4\uB354\uBA85/\u2026")}</code>
            {!failures.failDir && <>
                {" "}{t("\u2014 \uC544\uC9C1")}<b>{t("\uC138\uD305 \uD0ED")}</b>{t("\uC5D0\uC11C \uBCF4\uAD00 \uD3F4\uB354\uB97C \uC9C0\uC815\uD558\uC9C0 \uC54A\uC558\uC2B5\uB2C8\uB2E4.")}</>}
          </div>
          <div className="search">
            <button onClick={loadFailures}>{t("\u21BB \uC0C8\uB85C\uACE0\uCE68")}</button>
            {failMsg && <span className="meta ok-text">{failMsg}</span>}
          </div>
          {failures.items.length > 0 ? <div className="libtable">
              <div className="libhead row failhead">
                {[["library", "Library"], ["name", "File"], ["size", "Size"], ["movedAt", "Fail date"], ["reason", "Reason"], ["originalPath", "Original path"], ["act", ""]].map(([k, label]) => k === "act" ? <span key={k} className="c-fact" /> : <span key={k} className={`libth c-f${k}`} onClick={() => setFailSort(s => s.key === k ? {
            key: k,
            dir: -s.dir
          } : {
            key: k,
            dir: 1
          })} title={t("\uD074\uB9AD: \uC624\uB984/\uB0B4\uB9BC \uC815\uB82C")}>
                        {label}
                        {failSort.key === k ? failSort.dir === 1 ? " ↑" : " ↓" : ""}
                      </span>)}
              </div>
              {(() => {
          const items = [...failures.items].sort((a, b) => {
            const get = x => {
              switch (failSort.key) {
                case "library":
                  return x.library || "";
                case "name":
                  return x.name || "";
                case "size":
                  return x.size || 0;
                case "movedAt":
                  return x.movedAt ?? x.mtime ?? 0;
                case "reason":
                  return x.reason || "";
                case "originalPath":
                  return x.originalPath || "";
                default:
                  return 0;
              }
            };
            const va = get(a);
            const vb = get(b);
            if (typeof va === "number" && typeof vb === "number") return (va - vb) * failSort.dir;
            return String(va).localeCompare(String(vb), "ko") * failSort.dir;
          });
          return items.map(it => <div key={it.path} className="librow row">
                    <span className="c-flibrary jmeta" title={it.library}>
                      {it.library}
                    </span>
                    <span className="c-fname jname" title={it.path}>
                      {it.name}
                    </span>
                    <span className="c-fsize jmeta">{fmtSize(it.size)}</span>
                    <span className="c-fmoved jmeta" title={String(it.movedAt)}>
                      {it.movedAt ? new Date(it.movedAt).toLocaleString("ko-KR", {
                month: "2-digit",
                day: "2-digit",
                hour: "2-digit",
                minute: "2-digit"
              }) : "—"}
                    </span>
                    <span className="c-freason jmeta" title={it.reason || ""}>
                      {it.reason || "—"}
                    </span>
                    <span className="c-forig jmeta" title={it.originalPath || ""}>
                      {it.originalPath ? shortPath(it.originalPath) : "—"}
                    </span>
                    <span className="c-fact">
                      <button className="mini" title={t("\uC6D0\uBCF8 \uC704\uCE58\uB85C \uBCF5\uC6D0 (\uC2A4\uCE94 \uC2DC \uB2E4\uC2DC \uD050\uC789)")} onClick={() => restoreFailure(it.path)}>{t("\u293A \uBCF5\uC6D0")}</button>
                      <button className="mini bad" title={t("\uC0AD\uC81C (\uC6D0\uBCF8\uC73C\uB85C \uB3CC\uC544\uAC00\uC9C0 \uC54A\uC74C)")} onClick={() => deleteFailure(it.path)}>{t("\u2715 \uC0AD\uC81C")}</button>
                    </span>
                  </div>);
        })()}
            </div> : <div className="empty">
              {failures.failDir ? t("\uBCF4\uAD00\uB41C \uC2E4\uD328 \uD30C\uC77C\uC774 \uC5C6\uC2B5\uB2C8\uB2E4 \u2014 \uC6D0\uBCF8\uC774 \uADF8\uB300\uB85C \uC788\uC2B5\uB2C8\uB2E4.") : t("\uBCF4\uAD00 \uD3F4\uB354\uAC00 \uC9C0\uC815\uB418\uC9C0 \uC54A\uC558\uC2B5\uB2C8\uB2E4. \uC138\uD305 \uD0ED\uC5D0\uC11C \uC9C0\uC815\uD558\uC138\uC694.")}
            </div>}
        </section>}

      {/* ═══ 세팅 — server settings + fail (quarantine) folder ═══ */}
      {tab === "settings" && <section className="card settings">
          <h2>{t("\uC138\uD305")}</h2>
          <div className="setblock">
            <h3>{t("\uC2E4\uD328 \uCC98\uB9AC")}</h3>
            <div className="hint">
              <b>{t("\uD5EC\uC2A4\uCCB4\uD06C")}</b>{t("(\uB85C\uC9C1\uC758 \u2717 \uAC08\uB798) \uB610\uB294")}<b>{t("\uCD9C\uB825 \uAC80\uC99D \uC2E4\uD328")}</b>{t("(\uC2E4\uD328 \uC2DC \u2192 \uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC774\uB3D9)\uC5D0 \uAC78\uB9B0 \uC601\uC0C1\uC744 \uBAA8\uC544\uB458 \uD3F4\uB354\uB97C \uC9C0\uC815\uD558\uC138\uC694. \uC774\uB3D9 \uAD6C\uC870:")}{" "}
              <code>{t("<\uBCF4\uAD00\uD3F4\uB354>/\uB77C\uC774\uBE0C\uB7EC\uB9AC\uD3F4\uB354\uBA85/\uC6D0\uBCF8 \uC0C1\uB300\uACBD\uB85C")}</code>{t("\u2014 \uC6D0\uBCF8 \uC704\uCE58\xB7\uC774\uC720\uB294 \uD30C\uC77C \uC606")}<code>.ctfail.json</code>{t("\uC5D0 \uAE30\uB85D\uB418\uACE0,")}{" "}
              <b>{t("\uC2E4\uD328 \uBCF4\uAD00\uD568")}</b>{t("\uD0ED\uC5D0\uC11C \uD655\uC778/\uBCF5\uC6D0/\uC0AD\uC81C\uD560 \uC218 \uC788\uC2B5\uB2C8\uB2E4.")}</div>
            <div className="search">
              <input value={failDirDraft ?? ""} onChange={e => setFailDirDraft(e.target.value)} onKeyDown={e => e.key === "Enter" && saveFailDir()} placeholder={t("\uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354 \uACBD\uB85C (\uC608: /mnt/nas/Failed)")} />
              <button onClick={() => openTree("faildir")} title={t("\uD30C\uC77C\uC2DC\uC2A4\uD15C \uD2B8\uB9AC\uB85C \uD3F4\uB354 \uCC3E\uAE30")}>{t("\uD83D\uDD0D \uD3F4\uB354 \uCC3E\uC544\uBCF4\uAE30")}</button>
              <button onClick={saveFailDir}>{t("\uC800\uC7A5")}</button>
              {failures.failDir && <button onClick={() => {
            setFailDirDraft("");
            api.settingsPost({
              failDir: null
            }).then(() => loadFailures());
          }} title={t("\uBCF4\uAD00 \uD3F4\uB354 \uD574\uC81C")}>{t("\uD574\uC81C")}</button>}
            </div>
            {settingsMsg && <div className="meta ok-text">{settingsMsg}</div>}
          </div>
          <div className="setblock">
            <h3>{t("\uC790\uB3D9 \uAC10\uC9C0")}</h3>
            <div className="hint">{t("\uB4F1\uB85D\uB41C \uD3F4\uB354\uC5D0")}<b>{t("\uC0C8 \uD30C\uC77C\uC774 \uCD94\uAC00")}</b>{t("\uB418\uBA74 \uC8FC\uAE30\uC801\uC73C\uB85C \uC7AC\uC2A4\uCE94\uD574 \uC790\uB3D9\uC73C\uB85C \uD050\uC5D0 \uB123\uC2B5\uB2C8\uB2E4. \uC2A4\uCE94/\uC778\uCF54\uB529\uC774 \uC9C4\uD589 \uC911\uC774\uBA74 \uADF8 \uD2F1\uC740 \uAC74\uB108\uB6F0\uACE0 \uB2E4\uC74C \uC8FC\uAE30\uC5D0 \uBC18\uC601\uB429\uB2C8\uB2E4. \uD3F4\uB354\uB97C")}<b>{t("\uD65C\uC131\uD654")}</b>{t("\uD558\uBA74 \uC989\uC2DC \uADF8 \uD3F4\uB354\uB9CC \uC2A4\uCE94\uB3C4 \uC2DC\uC791\uB429\uB2C8\uB2E4.")}</div>
            <div className="search">
              <select value={autoScanDraft ?? 0} onChange={e => setAutoScanDraft(Number(e.target.value))}>
                <option value={0}>{t("\uAEBC\uC9D0 (\uC218\uB3D9 \uC2A4\uCE94\uB9CC)")}</option>
                <option value={1}>{t("1\uBD84\uB9C8\uB2E4")}</option>
                <option value={5}>{t("5\uBD84\uB9C8\uB2E4")}</option>
                <option value={10}>{t("10\uBD84\uB9C8\uB2E4")}</option>
                <option value={30}>{t("30\uBD84\uB9C8\uB2E4")}</option>
              </select>
              <button onClick={() => api.settingsPost({
            autoScanMinutes: autoScanDraft ?? 0
          }).then(r => {
            setAutoScanDraft(r.autoScanMinutes ?? 0);
            flashBlock(t("\uC790\uB3D9 \uAC10\uC9C0 \uC8FC\uAE30 \uC800\uC7A5\uB428 \u2713"));
          })}>{t("\uC800\uC7A5")}</button>
            </div>
          </div>
          <div className="setblock">
            <h3>{t("언어 (Language)")}</h3>
            <div className="hint">{t("인터페이스 언어 — 서버 설정에 저장되어 모든 기기에서 동일하게 적용됩니다.")}</div>
            <div className="search">
              <select value={lang} onChange={e => {
            const v = e.target.value;
            setLang(v);
            setLangState(v);
            api.settingsPost({ language: v }).then(r => {
              const l2 = r.language === "en" ? "en" : "ko";
              setLang(l2);
              setLangState(l2);
            }).catch(() => {});
          }}>
                <option value="ko">한국어</option>
                <option value="en">English</option>
              </select>
            </div>
          </div>
          <div className="setblock">
            <h3>{t("\uC11C\uBC84 \uC815\uBCF4")}</h3>
            <div className="statgrid">
              <div className="stat">
                <div className="stat-top">
                  <span className="stat-label">{t("\uC11C\uBE44\uC2A4")}</span>
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
          </div>
        </section>}

      {/* ── filesystem tree browser (folder picker) — any tab ── */}
      {treeOpen && <div className="overlay" onClick={() => setTreeOpen(false)}>
          <div className="treemodal" onClick={e => e.stopPropagation()}>
            <div className="tm-head">
              <b>{t("\uD83D\uDD0D \uD3F4\uB354 \uCC3E\uC544\uBCF4\uAE30")}</b>
              <input className="tm-jump" value={treeJump} onChange={e => setTreeJump(e.target.value)} onKeyDown={e => e.key === "Enter" && treeJumpGo()} placeholder={t("\uACBD\uB85C\uB85C \uC810\uD504 (\uC608: /mnt/nas/Videos)")} />
              <button className="tm-x" onClick={() => setTreeOpen(false)}>
                ✕
              </button>
            </div>
            <div className="tm-crumbs">
              {treeCrumbs(treePath).map(c => <button key={c.path} className={`tm-crumb ${c.path === treePath ? "here" : ""}`} onClick={() => treeNav(c.path)}>
                  {c.label}
                </button>)}
            </div>
            <div className="tm-body">
              {treeErr && <div className="tm-err">⚠ {treeErr}</div>}
              {!tree && !treeErr && <div className="tm-err">{t("\uBD88\uB7EC\uC624\uB294 \uC911\u2026")}</div>}
              {tree && <>
                  {tree.parent !== treePath && <div className="tm-row up" onClick={treeUp}>
                      ⬆ <span>{tree.parent}</span>
                    </div>}
                  {(tree.dirs || []).map(d => <div key={d.path} className="tm-row dir" title={d.path} onClick={() => treeNav(d.path)}>
                      📁 <span>{d.name}</span>
                    </div>)}
                  {(tree.files || []).map(f => <div key={f.path} className={`tm-row file ${f.video ? "video" : ""}`} title={f.path}>
                      {f.video ? "🎬" : "📄"} <span>{f.name}</span>
                      <span className="tm-size">{fmtSize(f.size || 0)}</span>
                    </div>)}
                  {(!tree.dirs || tree.dirs.length === 0) && (!tree.files || tree.files.length === 0) && <div className="tm-err">{t("\uBE44\uC5B4 \uC788\uC2B5\uB2C8\uB2E4")}</div>}
                </>}
            </div>
            <div className="tm-foot">
              <span className="tm-pick" title={treePath}>
                {treePath}
              </span>
              {treeMode === "faildir" ? <button onClick={() => {
            setFailDirDraft(treePath);
            setTreeOpen(false);
            setSettingsMsg("");
          }} disabled={!treePath} title={t("\uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC120\uD0DD (\uC138\uD305 \uD0ED\uC758 \uC800\uC7A5 \uBC84\uD2BC\uC73C\uB85C \uD655\uC815)")}>{t("\uD83D\uDDC4 \uC774 \uD3F4\uB354\uB97C \uC2E4\uD328 \uBCF4\uAD00 \uD3F4\uB354\uB85C \uC120\uD0DD")}</button> : <button onClick={addTreeFolder} disabled={!treePath || folders.some(f => f.path === treePath)}>{t("+ \uC774 \uD3F4\uB354 \uCD94\uAC00")}</button>}
            </div>
          </div>
        </div>}
    </div>;
}