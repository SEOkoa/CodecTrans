// Tiny API client for the CodecTrans backend (same-origin /api in production,
// proxied in dev by Vite).
//
// Optional server auth: when the server runs with CT_API_TOKEN set, every
// /api call needs `Authorization: Bearer <token>`. The token is entered once
// in the UI (a banner appears on the first 401) and kept in localStorage.

function getAuthToken() {
  try {
    return localStorage.getItem("ct_token") || "";
  } catch {
    return "";
  }
}

export function setAuthToken(t) {
  try {
    if (t) localStorage.setItem("ct_token", t);
    else localStorage.removeItem("ct_token");
  } catch {
    /* private mode — auth will simply not persist */
  }
}

// Any 401 flips the UI into "token required" mode (see App.jsx listener).
function flagUnauthorized(status) {
  if (status === 401) {
    try {
      window.dispatchEvent(new Event("ct-unauthorized"));
    } catch {
      /* non-browser context */
    }
  }
}

async function jfetch(path, opts = {}) {
  const headers = { ...(opts.headers || {}) };
  const t = getAuthToken();
  if (t && !headers.Authorization) headers.Authorization = `Bearer ${t}`;
  const res = await fetch(path, { ...opts, headers });
  let body = null;
  const text = await res.text();
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  if (!res.ok) {
    flagUnauthorized(res.status);
    const msg = body && body.error ? body.error : `HTTP ${res.status}`;
    const err = new Error(msg);
    err.status = res.status;
    throw err;
  }
  return body;
}

export const api = {
  health: () => jfetch("/api/health"),
  serverStats: () => jfetch("/api/server-stats"),
  codecs: () => jfetch("/api/codecs"),
  jobs: () => jfetch("/api/jobs"),
  job: (id) => jfetch(`/api/jobs/${id}`),
  // cancel a single queued job / wipe the whole queued backlog
  jobCancel: (id) => jfetch(`/api/jobs/${encodeURIComponent(id)}`, { method: "DELETE" }),
  jobsClear: () => jfetch("/api/jobs/clear", { method: "DELETE" }),
  nodes: (status = "all") =>
    jfetch(`/api/nodes?status=${encodeURIComponent(status)}`),
  // worker settings (dashboard gear): display name, enabled backends,
  // software fallback, max concurrent jobs.
  nodeConfig: (id, body) =>
    jfetch(`/api/nodes/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  discoveredNodes: () => jfetch("/api/nodes/discovered"),
  // "server finds a node": probe a node's local API (ip:port) and register it.
  searchNode: (ip, port) =>
    jfetch("/api/nodes/search", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ip, port: port ? Number(port) : undefined }),
    }),

  // Upload with real progress (XHR) + clear errors. Big video files take a
  // while over the network, so the UI shows a percentage instead of hanging.
  upload: (file, onProgress) =>
    new Promise((resolve, reject) => {
      const fd = new FormData();
      fd.append("file", file);
      const xhr = new XMLHttpRequest();
      xhr.open("POST", "/api/upload");
      const t = getAuthToken();
      if (t) xhr.setRequestHeader("Authorization", `Bearer ${t}`);
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable && onProgress) onProgress(e.loaded / e.total);
      };
      xhr.onload = () => {
        let body = null;
        try {
          body = xhr.responseText ? JSON.parse(xhr.responseText) : null;
        } catch {
          body = xhr.responseText;
        }
        if (xhr.status >= 200 && xhr.status < 300) {
          resolve(body);
        } else {
          flagUnauthorized(xhr.status);
          const msg =
            body && body.error ? body.error : `Upload failed (HTTP ${xhr.status})`;
          reject(new Error(msg));
        }
      };
      xhr.onerror = () =>
        reject(
          new Error(
            "Could not reach the server (network error). Is it running, and is the IP:port right?"
          )
        );
      xhr.ontimeout = () => reject(new Error("Upload timed out."));
      xhr.timeout = 0; // no timeout for large uploads
      xhr.send(fd);
    }),

  // local (server-side) transcode
  transcode: (payload) =>
    jfetch("/api/transcode", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    }),

  // dispatch to a node (auto HW-acceleration)
  dispatch: (payload) =>
    jfetch("/api/dispatch", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    }),

  downloadUrl: (name) => `/api/download/${encodeURIComponent(name)}`,

  // storage library (server scans a directory of videos, read-only)
  library: () => jfetch("/api/library"),
  libraryScan: (root, limit, folderId) =>
    jfetch("/api/library/scan", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        root: root || undefined,
        limit,
        folderId: folderId || undefined,
      }),
    }),
  // live (or last-run) library scan progress
  scanProgress: () => jfetch("/api/library/scan/progress"),
  // library automation settings (persisted server-side; ad-hoc roots)
  autoGet: () => jfetch("/api/library/auto"),
  autoPost: (cfg) =>
    jfetch("/api/library/auto", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(cfg),
    }),
  // library folders (add/remove list) + per-folder encoding target
  foldersGet: () => jfetch("/api/library/folders").then((r) => r.folders || []),
  requeue: (key) =>
    jfetch("/api/library/requeue", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key }),
    }),
  foldersPost: (folders) =>
    jfetch("/api/library/folders", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ folders }),
    }),
  // logic automation (node graph: {graph} — legacy {blocks} still accepted)
  blocksGet: () => jfetch("/api/library/blocks"),
  // filesystem tree browser (library folder picker): one directory level
  fsChildren: (path) =>
    jfetch("/api/fs/children?path=" + encodeURIComponent(path || "/")),
  blocksPost: (body) =>
    jfetch("/api/library/blocks", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  folderBlocksGet: (fid) => jfetch(`/api/library/folders/${fid}/blocks`),
  folderBlocksPost: (fid, body) =>
    jfetch(`/api/library/folders/${fid}/blocks`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  // saved named logics (the logic tab edits these; the library tab assigns
  // one per folder)
  logicsGet: () => jfetch("/api/logic").then((r) => r.logics || []),
  logicsPost: (body) =>
    jfetch("/api/logic", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  logicDelete: (id) => jfetch(`/api/logic/${id}`, { method: "DELETE" }),
  // server-level settings (currently: the fail/quarantine folder)
  settingsGet: () => jfetch("/api/settings"),
  settingsPost: (body) =>
    jfetch("/api/settings", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  // fail quarantine (보관함): list / restore / delete
  failuresGet: () => jfetch("/api/failures"),
  failureRestore: (path) =>
    jfetch("/api/failures/restore", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    }),
  failureDelete: (path) =>
    jfetch("/api/failures/delete", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path }),
    }),
};

export function fmtSize(bytes) {
  if (bytes == null) return "";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  let n = bytes;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n >= 100 || i === 0 ? 0 : 1)} ${u[i]}`;
}

export function fmtRate(bps) {
  if (bps == null) return "";
  if (bps >= 1000000) return `${(bps / 1000000).toFixed(1)} Mbps`;
  if (bps >= 1000) return `${Math.round(bps / 1000)} kbps`;
  return `${bps} bps`;
}

// seconds -> "H:MM:SS" (or "M:SS" under an hour)
export function fmtDur(sec) {
  if (sec == null || !isFinite(sec)) return "";
  const s = Math.round(sec);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  const pad = (x) => String(x).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(ss)}` : `${m}:${pad(ss)}`;
}
