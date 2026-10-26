# Backend audit backlog — 2026-10-26

Read-only audit of `CodecTrans-server/backend/src/main.rs` (isolated subagent,
line refs from the 6954-line pre-fix snapshot — re-locate before fixing).
20 findings. Ranked by real-world impact on this deployment (QNAP NAS, 4k+
file library, 5-minute auto-scan, multi-node).

> **Status note (2026-10-26):** #3 (health-decode timeout orphans ffmpeg) was
> independently root-caused from the live QNAP wedge (orphaned decodes pinning
> 4.2GB RSS / 18GB VmPeak on a RAM-starved host) and FIXED in the deploy that
> went out the same day: explicit `child.kill()` + reap on timeout, plus a
> 120-frame cap on spot-check windows. Do not re-fix; verify behavior post-deploy.

## CRITICAL

1. **`/api/download/:name` reads the whole output file into RAM** (`std::fs::read`, ~5433).
   Multi-GB output OOMs the process + pegs a core. Node-fetch path already
   streams (file_response, 1MiB chunks) — reuse it.

## HIGH

2. **Every job completion re-persists the entire catalog under both global
   locks** (finish_lib_job / node_report error path → persist_catalog:
   LIB_CFG_LOCK + store mutex held while cloning the catalog, serializing
   JSON, writing .bak+.tmp+rename AND library.json). Cost scales with library
   size → sustained CPU/disk spike + server-wide stalls.
   Fix: dirty flag + background debounced persist; never hold locks across
   serialization/disk I/O.
3. ~~Health-decode timeout doesn't kill ffmpeg~~ — **FIXED 2026-10-26** (see note).
4. **In-memory job store never pruned; `list()` clones the whole map** on every
   poll/count/duplicate-guard. Memory grows monotonically; each UI poll
   serializes all history.
   Fix: trim terminal jobs in self-heal (24h / last N); lock-held counters;
   paginate list_jobs.
5. **library.json written non-atomically** (plain `std::fs::write`). Power loss
   mid-write → truncated JSON → all folders/logics/fail-dir silently gone.
   Fix: serialize-first + .bak + rename (same pattern as persist_catalog) +
   .bak fallback on parse failure.
6. **10-min stuck-`dispatched` requeue can steal an in-flight job** — clock
   starts at created_at, not claim time; original node never told. Two encodes
   of the same source, second Replace deletes the original while the first is
   running → live data-loss race.
   Fix: dispatched_at at claim; age off that; clear Redis currentJob +
   invalidate late reports on re-claim.
7. **Server restart mid-node-job orphans the finished output and re-encodes**
   (job map in-memory only; unknown jobId in /result bails; entry reset to
   pending by self-heal → re-encode; orphaned outputs never cleaned).
   Fix: persist jobId→(lib_path,node,output) in Redis at claim; resolve unknown
   jobIds there; startup sweep of stale outputs/.
8. **Scan runs unbounded `sh -c` per file** (cmd conditions/postCmd, up to 2-4
   spawns per file × 3 graph re-evaluations, no semaphore; plus per-file
   list_nodes_redis in dispatch). 5000-file scan → fork/CPU storm.
   Fix: shared semaphore (~4), memoize cmd-condition result across
   h_true/h_false, cache online-node list a few seconds.

## MEDIUM

9. Health-check depth computed from the UNION of all graphs — one "all" logic
   forces full-file decodes everywhere. Compute from the file's own effective
   graph (same precedence as evaluate_graph).
10. active_job_count / duplicate-guard clone the whole job map per call
    (O(n) per file per scan). Also misses "dispatched" in the guard.
11. Crash mid-scan loses all per-file progress (persist only at the end) →
    next scan re-runs ffprobe + health decode for the whole library.
    Fix: rate-limited incremental persist during long scans.
12. node_report not validated against the reporting node; API open by default
    (no CT_API_TOKEN) → any LAN device can forge job completion states.
13. Multi-GB blocking file copies (move_to_fail_folder, failure_restore,
    rename fallback) run directly on async workers → worker-pool exhaustion.
    Fix: spawn_blocking.
14. Scan directory walk blocks a worker for the whole traversal
    (`std::thread::scope(...).join()` in the async handler); plan loop takes
    the store lock per candidate file.
15. outputs/ accumulates orphaned artifacts (.inprogress_*, staged results of
    lost jobs, user outputs) with no cleanup → slow disk-full.
16. fs_children: blocking read_dir + up to 5000 metadata syscalls in an async
    handler (NFS/SMB round-trips).

## LOW

17. wait_file_size: up to 2s of `std::thread::sleep` on async workers per
    completion. → tokio::time::sleep.
18. /api/health and /api/codecs spawn blocking ffmpeg per request. → cache
    behind OnceLock (build never changes at runtime).
19. Server-side transcode finalizer overwrites a `cancelled` job with
    done/error (cancel can't kill the child). → re-check status after wait;
    optionally kill ffmpeg on cancel.
20. Store mutex locked with `.unwrap()` everywhere — one panic poisons it and
    brinks all APIs until restart. → `unwrap_or_else(|p| p.into_inner())`
    consistently (LIB_CFG_LOCK already does).

## Top 10 to fix (ranked)

1. `download` endpoint whole-file RAM read (OOM on multi-GB)
2. per-completion full-catalog persist under both global locks
3. health-decode timeout orphan — **DONE (2026-10-26)**
4. unpruned in-memory job store + whole-map clones per poll
5. non-atomic library.json write (silent config wipe)
6. stuck-dispatched requeue stealing in-flight jobs (double encode + delete race)
7. restart orphaning node results (re-encode + outputs/ growth)
8. unbounded per-file shell execution in scans
9. health-depth from ALL graphs ("all" amplification)
10. O(n) job-map clones in dispatch/queue-cap paths
