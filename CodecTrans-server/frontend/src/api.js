// Tiny API client for the CodecTrans backend (same-origin /api in production,
// proxied in dev by Vite).

async function jfetch(path, opts = {}) {
  const res = await fetch(path, opts);
  let body = null;
  const text = await res.text();
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  if (!res.ok) {
    const msg = body && body.error ? body.error : `HTTP ${res.status}`;
    throw new Error(msg);
  }
  return body;
}

export const api = {
  health: () => jfetch("/api/health"),
  serverStats: () => jfetch("/api/server-stats"),
  codecs: () => jfetch("/api/codecs"),
  jobs: () => jfetch("/api/jobs"),
  job: (id) => jfetch(`/api/jobs/${id}`),
  nodes: (status = "all") =>
    jfetch(`/api/nodes?status=${encodeURIComponent(status)}`),
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
  libraryScan: (root, limit) =>
    jfetch("/api/library/scan", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ root: root || undefined, limit }),
    }),
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
