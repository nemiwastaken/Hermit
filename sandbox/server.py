#!/usr/bin/env python3
"""Hermit sandbox: the only place that executes anything.

The core sends one JSON object per line, gets one JSON object back:
    -> {"id":"e42/7","tool":"bash","args":{"cmd":["ls","-la"],"cwd":"/workspace"}}
    <- {"id":"e42/7","ok":true,"exit":0,"stdout":"...","stderr":"","truncated":null}

No shell string ever crosses the socket: `cmd` is argv. A model that wants a
shell pipe has to ask for it explicitly (["bash","-lc","..."]), and it happens
inside this container, not in the core.
"""

import argparse
import json
import os
import socket
import subprocess
import sys
import threading

WORKSPACE = os.environ.get("HERMIT_WORKSPACE", "/workspace")
TMP_DIR = os.path.join(WORKSPACE, ".tmp")
MAX_LINES = 200          # lines returned to the model
HEAD_LINES = 100
TAIL_LINES = 100
MAX_LINE_CHARS = 2000    # one minified file must not eat the context
MAX_BYTES = 20_000       # soft cap before spilling to a file
MAX_READ_BYTES = 8 * 1024 * 1024


def _spill(text, name):
    os.makedirs(TMP_DIR, exist_ok=True)
    safe = "".join(c if c.isalnum() or c in "-_." else "_" for c in name)[:80]
    path = os.path.join(TMP_DIR, safe + ".log")
    with open(path, "w", encoding="utf-8", errors="replace") as f:
        f.write(text)
    return path


def clip(text, name):
    """Return (text_for_model, meta) where meta describes the full log."""
    text = text or ""
    lines = text.splitlines()
    if len(lines) <= MAX_LINES and len(text) <= MAX_BYTES:
        return "\n".join(l[:MAX_LINE_CHARS] for l in lines), None

    path = _spill(text, name)
    head = [l[:MAX_LINE_CHARS] for l in lines[:HEAD_LINES]]
    tail = [l[:MAX_LINE_CHARS] for l in lines[-TAIL_LINES:]]
    omitted = len(lines) - HEAD_LINES - TAIL_LINES
    body = "\n".join(head) + f"\n... [{omitted} lines omitted, full output: {path}] ...\n" + "\n".join(tail)
    return body, {"full_path": path, "lines": len(lines)}


def bash(req_id, args):
    cmd = args.get("cmd")
    if not isinstance(cmd, list) or not cmd or not all(isinstance(c, str) for c in cmd):
        raise ValueError("cmd must be a non-empty array of strings")
    cwd = args.get("cwd") or WORKSPACE
    timeout = min(max(float(args.get("timeout_ms") or 30_000) / 1000.0, 1.0), 600.0)

    try:
        p = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, errors="replace",
            timeout=timeout, stdin=subprocess.DEVNULL,
        )
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        out = e.stdout if isinstance(e.stdout, str) else ""
        err = (e.stderr if isinstance(e.stderr, str) else "") + f"\n[timeout after {timeout:.0f}s]"
        rc = None
    except FileNotFoundError as e:
        return {"ok": False, "exit": None, "stdout": "", "stderr": str(e), "truncated": None}

    stdout, meta_out = clip(out, f"{req_id}-out")
    stderr, meta_err = clip(err, f"{req_id}-err")
    trunc = None
    if meta_out or meta_err:
        trunc = {"full_path": (meta_out or meta_err)["full_path"],
                 "lines": (meta_out or {}).get("lines", 0) + (meta_err or {}).get("lines", 0)}
    return {"ok": rc == 0, "exit": rc, "stdout": stdout, "stderr": stderr, "truncated": trunc}


def read_file(req_id, args):
    path = args.get("path")
    if not isinstance(path, str) or not path:
        raise ValueError("path must be a string")
    if os.path.getsize(path) > MAX_READ_BYTES:
        raise ValueError(f"file is larger than {MAX_READ_BYTES} bytes, use bash (sed/head/rg)")
    with open(path, "rb") as f:
        head = f.read(4096)
    if b"\0" in head:
        raise ValueError("binary file, use bash (file/xxd)")
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        lines = f.readlines()

    offset = max(int(args.get("offset") or 0), 0)
    limit = min(max(int(args.get("limit") or 200), 1), 2000)
    chunk = lines[offset:offset + limit]
    content = "".join(l[:MAX_LINE_CHARS] + ("\n" if not l.endswith("\n") else "") for l in chunk)
    trunc = {"full_path": path, "lines": len(lines)} if offset + limit < len(lines) else None
    return {"ok": True, "path": path, "content": content,
            "lines": len(lines), "offset": offset, "truncated": trunc}


def dispatch(req):
    req_id, tool, args = req.get("id"), req.get("tool"), req.get("args") or {}
    if not isinstance(args, dict):
        return {"id": req_id, "ok": False, "error": "args must be an object"}
    try:
        if tool == "bash":
            return {"id": req_id, **bash(str(req_id), args)}
        if tool == "read":
            return {"id": req_id, **read_file(str(req_id), args)}
    except Exception as e:  # surface any tool failure to the model, never crash
        return {"id": req_id, "ok": False, "error": f"{type(e).__name__}: {e}"}
    return {"id": req_id, "ok": False, "error": f"unknown tool {tool!r}"}


def serve(sock_path):
    os.makedirs(os.path.dirname(sock_path), exist_ok=True)
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    os.chmod(sock_path, 0o660)
    srv.listen(16)
    print(f"sandbox listening on {sock_path}", flush=True)

    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


def handle(conn):
    with conn:
        rf = conn.makefile("r", encoding="utf-8", errors="replace")
        for line in rf:
            line = line.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError:
                resp = {"id": None, "ok": False, "error": "invalid json"}
            else:
                resp = dispatch(req)
            conn.sendall((json.dumps(resp, ensure_ascii=False) + "\n").encode())


def selftest():
    global WORKSPACE, TMP_DIR
    import tempfile
    WORKSPACE = tempfile.mkdtemp()
    TMP_DIR = os.path.join(WORKSPACE, ".tmp")

    r = dispatch({"id": "t1", "tool": "bash",
                  "args": {"cmd": ["bash", "-lc", "seq 1 500"], "cwd": WORKSPACE}})
    assert r["ok"] and r["exit"] == 0, r
    assert "omitted" in r["stdout"] and r["truncated"], r
    assert os.path.exists(r["truncated"]["full_path"]), r

    r = dispatch({"id": "t2", "tool": "bash", "args": {"cmd": ["bash", "-c", "exit 7"]}})
    assert r["ok"] is False and r["exit"] == 7, r

    r = dispatch({"id": "t3", "tool": "bash", "args": {"cmd": "ls -la"}})
    assert r["ok"] is False and "array of strings" in r["error"], r

    r = dispatch({"id": "t4", "tool": "read", "args": {"path": "/etc/hostname"}})
    assert r["ok"] and r["content"].strip(), r

    r = dispatch({"id": "t5", "tool": "nope", "args": {}})
    assert r["ok"] is False and "unknown tool" in r["error"], r

    print("selftest ok")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default="/run/hermit/sandbox.sock")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        return selftest()
    serve(a.socket)


if __name__ == "__main__":
    sys.exit(main())
