#!/usr/bin/env python3
"""E2E: plugin field/cmd conditions through the real scan pipeline (local smoke server)."""
import json, os, shutil, subprocess, time, urllib.request, urllib.error

BASE = "http://127.0.0.1:8411"
ROOT = "/tmp/ct_vids"
CT_BASE = "/tmp/ct_base_e2e"

def api(path, body=None):
    url = BASE + path
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method="POST" if data else "GET")
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        return {"__http": e.code, "body": e.read().decode()[:400]}

def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True).returncode == 0

# --- test videos in per-scenario subdirs ---
sh(f"rm -rf {ROOT}")
for d in ("A", "B", "C", "D"):
    os.makedirs(f"{ROOT}/{d}", exist_ok=True)
    dur = "2" if d in "AB" else "1"
    sh(f'ffmpeg -y -f lavfi -i testsrc=duration={dur}:size=320x240:rate=10 -f lavfi -i sine=frequency=440:duration={dur} '
       f'-c:v libx264 -preset ultrafast -b:v 200k -c:a aac -b:a 64k {ROOT}/{d}/v_{d}.mp4 2>/dev/null')
for d in "ABCD":
    f = f"{ROOT}/{d}/v_{d}.mp4"
    print(f, "OK" if os.path.exists(f) and os.path.getsize(f) > 1000 else "MISSING")

# --- start smoke server ---
sh(f"rm -rf {CT_BASE}")
env = dict(os.environ, CT_PORT="8411", CT_BASE=CT_BASE, CT_REDIS="redis://127.0.0.1:6379",
           CT_FRONTEND="/path/to/CodecTrans/CodecTrans-server/frontend/dist",
           CT_MAX_QUEUED="10")
srv = subprocess.Popen(["/path/to/CodecTrans/CodecTrans-server/backend/target/release/CodecTrans"],
                       env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(4)

def target():
    return {"videoCodec": "hevc", "container": "mp4", "mode": "pct", "bitratePct": 70}

def graph_for(cond):
    return {"nodes": [
        {"id": "s", "kind": "start", "x": 0, "y": 0},
        {"id": "c", "kind": "condition", "condition": cond, "x": 300, "y": 0},
        {"id": "enc", "kind": "action", "action": {"kind": "encode", "target": target()}, "x": 600, "y": -100},
        {"id": "skip", "kind": "action", "action": {"kind": "skip"}, "x": 600, "y": 100},
    ], "edges": [
        {"from": "s", "to": "c"},
        {"from": "c", "fromPort": "true", "to": "enc"},
        {"from": "c", "fromPort": "false", "to": "skip"},
    ]}

def libkey(e):
    return (e.get("folder") + "/" if e.get("folder") else "") + e["path"]

def requeue_folder(fid):
    lib = api("/api/library")
    for e in lib.get("entries", []):
        if e.get("folder") == fid:
            api("/api/library/requeue", {"key": libkey(e)})
    time.sleep(1)

def statuses(folder_id):
    lib = api("/api/library")
    return sorted((e["name"], e["status"]) for e in lib.get("entries", []) if e.get("folder") == folder_id)

results = {}
try:
    # register 4 folders, all referencing the named logic (by UUID)
    lr = api("/api/logic", {"name": "E2E", "graph": graph_for({"kind": "always"})})
    LID = lr["logic"]["id"]
    print("logic id:", LID)
    api("/api/library/folders", {"folders": [
        {"id": "fA", "path": f"{ROOT}/A", "enabled": True, "target": target(), "logicId": LID},
        {"id": "fB", "path": f"{ROOT}/B", "enabled": True, "target": target(), "logicId": LID},
        {"id": "fC", "path": f"{ROOT}/C", "enabled": True, "target": target(), "logicId": LID},
        {"id": "fD", "path": f"{ROOT}/D", "enabled": True, "target": target(), "logicId": LID},
    ]})

    # A: field size gte 999999 -> no match -> skipped
    api("/api/logic", {"id": LID, "name": "E2E", "graph": graph_for({"kind": "field", "field": "size", "op": "gte", "value": 999999})})
    api("/api/library/scan", {})
    time.sleep(10)
    results["A_field_nomatch"] = statuses("fA")
    print("A (size gte 999999, expect skipped):", results["A_field_nomatch"])

    # B: field size gte 0 -> match -> queued (no node online)
    api("/api/logic", {"id": LID, "name": "E2E", "graph": graph_for({"kind": "field", "field": "size", "op": "gte", "value": 0})})
    requeue_folder("fB")
    api("/api/library/scan", {})
    time.sleep(10)
    results["B_field_allmatch"] = statuses("fB")
    print("B (size gte 0, expect queued):", results["B_field_allmatch"])

    # C: cmd condition — no marker -> skipped; with marker -> queued
    api("/api/logic", {"id": LID, "name": "E2E", "graph": graph_for({"kind": "cmd", "cmd": "test -f /tmp/ct_e2e_marker", "cmdTimeout": 10})})
    sh("rm -f /tmp/ct_e2e_marker")
    requeue_folder("fC")
    api("/api/library/scan", {})
    time.sleep(10)
    c1 = statuses("fC")
    print("C1 (cmd, no marker, expect skipped):", c1)
    sh("touch /tmp/ct_e2e_marker")
    # re-evaluate: requeue fC entries then scan
    def libkey(e):
        return (e.get("folder") + "/" if e.get("folder") else "") + e["path"]
    requeue_folder("fC")
    api("/api/library/scan", {})
    time.sleep(10)
    c2 = statuses("fC")
    results["C_cmd"] = {"no_marker": c1, "with_marker": c2}
    print("C2 (cmd, marker, expect queued):", c2)

    # D: field name contains 'C' (only v_C in folder D? no — D has v_D). Use height lte 100 -> no (240px) -> skipped;
    # then gte 200 -> queued. Tests numeric text-free path + alias-free canonical name.
    api("/api/logic", {"id": LID, "name": "E2E", "graph": graph_for({"kind": "field", "field": "height", "op": "lte", "value": 100})})
    requeue_folder("fD")
    api("/api/library/scan", {})
    time.sleep(10)
    d1 = statuses("fD")
    print("D1 (height lte 100, 240px file, expect skipped):", d1)
    requeue_folder("fD")
    api("/api/logic", {"id": LID, "name": "E2E", "graph": graph_for({"kind": "field", "field": "height", "op": "gte", "value": 200})})
    api("/api/library/scan", {})
    time.sleep(10)
    d2 = statuses("fD")
    results["D_height"] = {"lte100": d1, "gte200": d2}
    print("D2 (height gte 200, expect queued):", d2)

    print("\nSUMMARY:", json.dumps(results, ensure_ascii=False, indent=1))

    # --- verification ---
    def st(sc, sub):
        return [x[1] for x in results[sc][sub]]
    def flat(k):
        return [x[1] for x in results[k]]
    checks = [
        ("A_field_nomatch.skip",     flat("A_field_nomatch"),  ["skipped"]),
        ("B_field_allmatch.encode",  flat("B_field_allmatch"), ["pending"]),
        ("C_cmd.no_marker.skip",     st("C_cmd", "no_marker"),   ["skipped"]),
        ("C_cmd.with_marker.encode", st("C_cmd", "with_marker"), ["pending"]),
        ("D_height.lte100.skip",     st("D_height", "lte100"),   ["skipped"]),
        ("D_height.gte200.encode",   st("D_height", "gte200"),   ["pending"]),
    ]
    ok = True
    for name, got, want in checks:
        good = got == want
        ok = ok and good
        print(f"{'PASS' if good else 'FAIL'}  {name}: got={got} want={want}")
    print("ALL SCENARIOS PASSED" if ok else "SOME SCENARIOS FAILED")
    VERIFY_OK = ok
finally:
    srv.terminate()
    print("server stopped")
if not VERIFY_OK:
    import sys
    sys.exit(1)
