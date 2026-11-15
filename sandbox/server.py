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
import glob as globmod
import json
import os
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time

WORKSPACE = os.environ.get("HERMIT_WORKSPACE", "/workspace")
TMP_DIR = os.path.join(WORKSPACE, ".tmp")
MAX_LINES = 200          # lines returned to the model
HEAD_LINES = 100
TAIL_LINES = 100
MAX_LINE_CHARS = 2000    # one minified file must not eat the context
MAX_BYTES = 20_000       # soft cap before spilling to a file
MAX_READ_BYTES = 8 * 1024 * 1024
MAX_MATCHES = 100
MAX_GLOB = 500
MAX_PROCS = 8
SOCKET_PATH = None


def _spill(text, name):
    os.makedirs(TMP_DIR, exist_ok=True)
    safe = "".join(c if c.isalnum() or c in "-_." else "_" for c in name)[:80]
    path = os.path.join(TMP_DIR, safe + ".log")
    with open(path, "w", encoding="utf-8", errors="replace") as f:
        f.write(text)
    return path


def _path(value, must_exist=False):
    """Resolve under /workspace. A path that escapes through .. or a symlink
    is rejected, not followed."""
    if not isinstance(value, str) or not value.strip():
        raise ValueError("path must be a non-empty string")
    root = os.path.realpath(WORKSPACE)
    p = os.path.realpath(value if os.path.isabs(value) else os.path.join(root, value))
    if p != root and not p.startswith(root + os.sep):
        raise ValueError(f"{value} is outside /workspace")
    if must_exist and not os.path.exists(p):
        raise ValueError(f"{value} does not exist")
    return p


def _text(path):
    with open(path, "r", encoding="utf-8", errors="replace", newline="") as f:
        return f.read()


def _write_text(path, text):
    """Atomic replace: a crash mid-write must not leave half a file."""
    parent = os.path.dirname(path) or WORKSPACE
    os.makedirs(parent, exist_ok=True)
    mode = None
    try:
        mode = stat.S_IMODE(os.stat(path).st_mode)
    except FileNotFoundError:
        pass
    fd, tmp = tempfile.mkstemp(dir=parent, prefix=".hermit-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="") as f:
            f.write(text)
        if mode is not None:
            os.chmod(tmp, mode)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def _replace(text, old, new, replace_all):
    if not isinstance(old, str) or not old:
        raise ValueError("old must be a non-empty string")
    if not isinstance(new, str):
        raise ValueError("new must be a string")
    count = text.count(old)
    if count == 0:
        raise ValueError("old text not found")
    if count > 1 and not replace_all:
        raise ValueError(f"old text occurs {count} times; add context or pass replace_all=true")
    return (text.replace(old, new) if replace_all else text.replace(old, new, 1),
            count if replace_all else 1)


def write_file(req_id, args):
    path = _path(args.get("path") or "", must_exist=False)
    content = args.get("content")
    if not isinstance(content, str):
        raise ValueError("content must be a string")
    _write_text(path, content)
    return {"ok": True, "path": path, "bytes": len(content.encode("utf-8"))}


def edit_file(req_id, args):
    path = _path(args.get("path") or "", must_exist=True)
    if os.path.isdir(path):
        raise ValueError("path is a directory")
    text = _text(path)
    new_text, n = _replace(text, args.get("old"), args.get("new"), bool(args.get("replace_all")))
    _write_text(path, new_text)
    return {"ok": True, "path": path, "replacements": n}


def patch_file(req_id, args):
    """Several exact replacements in one call. All or nothing: a typo in the
    third hunk must not leave the first two applied."""
    path = _path(args.get("path") or "", must_exist=True)
    edits = args.get("edits")
    if not isinstance(edits, list) or not edits:
        raise ValueError("edits must be a non-empty array of {old,new}")
    text = _text(path)
    for i, e in enumerate(edits, 1):
        if not isinstance(e, dict):
            raise ValueError(f"edit {i} must be an object")
        text, _ = _replace(text, e.get("old"), e.get("new"), bool(e.get("replace_all")))
    _write_text(path, text)
    return {"ok": True, "path": path, "edits": len(edits)}


def glob_files(req_id, args):
    pattern = args.get("pattern")
    if not isinstance(pattern, str) or not pattern.strip():
        raise ValueError("pattern is required")
    if os.path.isabs(pattern):
        raise ValueError("pattern must be relative to the base path")
    base = _path(args.get("path") or ".", must_exist=True)
    if not os.path.isdir(base):
        raise ValueError("path is not a directory")
    limit = min(max(int(args.get("limit") or 200), 1), MAX_GLOB)
    found = []
    for rel in globmod.glob(pattern, root_dir=base, recursive=True, include_hidden=False):
        full = _path(os.path.join(base, rel))
        if os.path.isfile(full):
            found.append(os.path.relpath(full, WORKSPACE))
    found.sort()
    return {"ok": True, "matches": found[:limit], "count": len(found),
            "truncated": len(found) > limit}


def grep_files(req_id, args):
    pattern = args.get("pattern")
    if not isinstance(pattern, str) or not pattern:
        raise ValueError("pattern is required")
    base = _path(args.get("path") or ".", must_exist=True)
    limit = min(max(int(args.get("limit") or MAX_MATCHES), 1), MAX_MATCHES)
    cmd = ["rg", "--json", "--line-number", "--max-columns", "500",
           "--max-filesize", "2M", "-m", "20"]
    if args.get("case_sensitive") is False:
        cmd.append("-i")
    if isinstance(args.get("glob"), str) and args["glob"]:
        cmd += ["--glob", args["glob"]]
    cmd += ["--", pattern, base]

    try:
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, errors="replace", start_new_session=True)
    except FileNotFoundError:
        raise ValueError("ripgrep is not installed in the sandbox")
    matches, truncated = [], False
    try:
        for line in proc.stdout:
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") != "match":
                continue
            data = ev.get("data") or {}
            if len(matches) >= limit:
                truncated = True
                proc.terminate()
                break
            path = (data.get("path") or {}).get("text") or ""
            line_no = data.get("line_number")
            text = "".join((data.get("lines") or {}).get("text") or "").rstrip("\n")
            matches.append({"path": os.path.relpath(path, WORKSPACE) if path else "",
                            "line": line_no, "text": text})
        _, err = proc.communicate(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        _, err = proc.communicate()
        raise ValueError("grep timed out")
    if proc.returncode not in (0, 1) and not matches:
        raise ValueError(f"grep failed: {(err or '').strip()[:500]}")
    return {"ok": True, "matches": matches, "count": len(matches), "truncated": truncated}


def list_dir(req_id, args):
    path = _path(args.get("path") or ".", must_exist=True)
    if not os.path.isdir(path):
        raise ValueError("path is not a directory")
    hidden = bool(args.get("hidden"))
    limit = min(max(int(args.get("limit") or 200), 1), MAX_GLOB)
    entries = []
    for name in sorted(os.listdir(path)):
        if not hidden and name.startswith("."):
            continue
        if len(entries) >= limit:
            break
        full = os.path.join(path, name)
        try:
            st = os.lstat(full)
        except OSError:
            continue
        kind = "dir" if os.path.isdir(full) else ("link" if os.path.islink(full) else "file")
        entries.append({"name": name, "type": kind,
                        "size": st.st_size if kind == "file" else None})
    return {"ok": True, "path": path, "entries": entries,
            "truncated": len(entries) >= limit}


PROCS = {}
PROC_LOCK = threading.Lock()
PROC_NEXT = 0


def process_manage(req_id, args):
    global PROC_NEXT
    action = args.get("action")
    with PROC_LOCK:
        if action == "start":
            cmd = args.get("cmd")
            if not isinstance(cmd, list) or not cmd or not all(isinstance(c, str) for c in cmd):
                raise ValueError("cmd must be a non-empty array of strings")
            running = sum(1 for p in PROCS.values() if p["proc"].poll() is None)
            if running >= MAX_PROCS:
                raise ValueError(f"already {MAX_PROCS} processes running; kill one first")
            cwd = _path(args.get("cwd") or WORKSPACE, must_exist=True)
            PROC_NEXT += 1
            pid = PROC_NEXT
            os.makedirs(TMP_DIR, exist_ok=True)
            log = os.path.join(TMP_DIR, f"proc-{pid}.log")
            handle = open(log, "ab", buffering=0)
            try:
                proc = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.DEVNULL,
                                        stdout=handle, stderr=subprocess.STDOUT,
                                        start_new_session=True)
            except FileNotFoundError as e:
                handle.close()
                raise ValueError(str(e))
            handle.close()
            PROCS[pid] = {"proc": proc, "cmd": cmd, "cwd": cwd, "log": log,
                          "started": time.strftime("%Y-%m-%d %H:%M:%S")}
            return {"ok": True, "id": pid, "pid": proc.pid, "log": log}

        if action == "list":
            out = []
            for pid, p in PROCS.items():
                rc = p["proc"].poll()
                out.append({"id": pid, "pid": p["proc"].pid, "cmd": p["cmd"],
                            "cwd": p["cwd"], "running": rc is None, "exit": rc,
                            "started": p["started"], "log": p["log"]})
            return {"ok": True, "processes": out}

        if action == "output":
            pid = int(args.get("id") or 0)
            p = PROCS.get(pid)
            if not p:
                raise ValueError(f"no process #{pid}")
            lines = min(max(int(args.get("lines") or 100), 1), 1000)
            size = 0
            try:
                size = os.path.getsize(p["log"])
                with open(p["log"], "rb") as f:
                    f.seek(max(0, size - 256_000))
                    raw = f.read()
                text = raw.decode("utf-8", "replace")
            except FileNotFoundError:
                text = ""
            tail = "\n".join(text.splitlines()[-lines:])
            rc = p["proc"].poll()
            return {"ok": True, "id": pid, "running": rc is None, "exit": rc,
                    "output": tail, "bytes": size}

        if action == "kill":
            pid = int(args.get("id") or 0)
            p = PROCS.get(pid)
            if not p:
                raise ValueError(f"no process #{pid}")
            proc = p["proc"]
            if proc.poll() is None:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    proc.wait(timeout=2)
            return {"ok": True, "id": pid, "exit": proc.returncode}

    raise ValueError("action must be start, list, output or kill")


HELPER = r"""
import json, os, socket

def call(tool, **kwargs):
    path = os.environ.get("HERMIT_TOOL_SOCKET")
    if not path:
        raise RuntimeError("tool socket is not set")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(path)
        f = s.makefile("rwb")
        f.write((json.dumps({"id": "code", "tool": tool, "args": kwargs,
                             "nested": True}) + "\n").encode())
        f.flush()
        line = f.readline()
    finally:
        s.close()
    if not line:
        raise RuntimeError("sandbox closed the connection")
    return json.loads(line)
"""


def execute_code(req_id, args):
    if os.environ.get("HERMIT_NESTED"):
        raise ValueError("execute_code cannot call itself")
    code = args.get("code")
    if not isinstance(code, str) or not code.strip():
        raise ValueError("code is required")
    timeout = min(max(float(args.get("timeout_ms") or 60_000) / 1000.0, 1.0), 600.0)
    os.makedirs(TMP_DIR, exist_ok=True)
    helper = os.path.join(TMP_DIR, "hermit.py")
    with open(helper, "w", encoding="utf-8") as f:
        f.write(HELPER)
    script = os.path.join(TMP_DIR, f"code-{str(req_id).replace('/', '-')}.py")
    with open(script, "w", encoding="utf-8") as f:
        f.write(code)
    env = dict(os.environ)
    env["HERMIT_TOOL_SOCKET"] = SOCKET_PATH or ""
    env["HERMIT_NESTED"] = "1"
    env["PYTHONPATH"] = TMP_DIR + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    proc = subprocess.Popen([sys.executable, script], cwd=WORKSPACE, env=env,
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, errors="replace",
                            start_new_session=True)
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        out, err = proc.communicate()
        err = (err or "") + f"\n[timeout after {timeout:.0f}s]"
    stdout, meta_out = clip(out, f"{req_id}-code-out")
    stderr, meta_err = clip(err, f"{req_id}-code-err")
    trunc = None
    if meta_out or meta_err:
        trunc = {"full_path": (meta_out or meta_err)["full_path"],
                 "lines": (meta_out or {}).get("lines", 0) + (meta_err or {}).get("lines", 0)}
    return {"ok": proc.returncode == 0, "exit": proc.returncode,
            "stdout": stdout, "stderr": stderr, "truncated": trunc}


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
    if not os.path.isabs(path):
        path = os.path.join(WORKSPACE, path)
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
    if req.get("nested") and tool == "execute_code":
        return {"id": req_id, "ok": False, "error": "execute_code cannot call itself"}
    try:
        if tool == "bash":
            return {"id": req_id, **bash(str(req_id), args)}
        if tool == "read":
            return {"id": req_id, **read_file(str(req_id), args)}
        if tool == "write":
            return {"id": req_id, **write_file(str(req_id), args)}
        if tool == "edit":
            return {"id": req_id, **edit_file(str(req_id), args)}
        if tool == "patch":
            return {"id": req_id, **patch_file(str(req_id), args)}
        if tool == "glob":
            return {"id": req_id, **glob_files(str(req_id), args)}
        if tool == "grep":
            return {"id": req_id, **grep_files(str(req_id), args)}
        if tool == "list":
            return {"id": req_id, **list_dir(str(req_id), args)}
        if tool == "process_manage":
            return {"id": req_id, **process_manage(str(req_id), args)}
        if tool == "execute_code":
            return {"id": req_id, **execute_code(str(req_id), args)}
    except Exception as e:  # surface any tool failure to the model, never crash
        return {"id": req_id, "ok": False, "error": f"{type(e).__name__}: {e}"}
    return {"id": req_id, "ok": False, "error": f"unknown tool {tool!r}"}


def serve(sock_path):
    global SOCKET_PATH
    SOCKET_PATH = sock_path
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

    r = dispatch({"id": "t6", "tool": "write", "args": {"path": "a/b.txt", "content": "one two\n"}})
    assert r["ok"] and os.path.exists(os.path.join(WORKSPACE, "a/b.txt")), r
    r = dispatch({"id": "t7", "tool": "edit", "args": {"path": "a/b.txt", "old": "two", "new": "2"}})
    assert r["ok"] and "one 2" in open(os.path.join(WORKSPACE, "a/b.txt")).read(), r
    r = dispatch({"id": "t8", "tool": "patch", "args": {"path": "a/b.txt",
                  "edits": [{"old": "one", "new": "1"}, {"old": "2", "new": "two"}]}})
    assert r["ok"] and "1 two" in open(os.path.join(WORKSPACE, "a/b.txt")).read(), r
    r = dispatch({"id": "t9", "tool": "patch", "args": {"path": "a/b.txt",
                  "edits": [{"old": "1", "new": "x"}, {"old": "missing", "new": "y"}]}})
    assert r["ok"] is False and "not found" in r["error"], r
    assert "1 two" in open(os.path.join(WORKSPACE, "a/b.txt")).read(), "partial patch was written"

    r = dispatch({"id": "t10", "tool": "glob", "args": {"pattern": "**/*.txt"}})
    assert r["ok"] and "a/b.txt" in r["matches"], r
    r = dispatch({"id": "t11", "tool": "grep", "args": {"pattern": "two", "path": "a"}})
    assert r["ok"] and r["matches"] and r["matches"][0]["line"] == 1, r
    r = dispatch({"id": "t12", "tool": "list", "args": {"path": "a"}})
    assert r["ok"] and r["entries"][0]["name"] == "b.txt", r
    r = dispatch({"id": "t13", "tool": "write", "args": {"path": "../escape", "content": "x"}})
    assert r["ok"] is False and "outside" in r["error"], r

    r = dispatch({"id": "t14", "tool": "process_manage",
                  "args": {"action": "start", "cmd": ["bash", "-c", "echo hi; sleep 0.3"]}})
    assert r["ok"], r
    pid = r["id"]
    time.sleep(0.1)
    r = dispatch({"id": "t15", "tool": "process_manage", "args": {"action": "output", "id": pid}})
    assert r["ok"] and "hi" in r["output"], r
    r = dispatch({"id": "t16", "tool": "process_manage", "args": {"action": "kill", "id": pid}})
    assert r["ok"], r

    r = dispatch({"id": "t17", "tool": "execute_code",
                  "args": {"code": "print(2 + 2)"}})
    assert r["ok"] and "4" in r["stdout"], r
    r = dispatch({"id": "t18", "tool": "execute_code",
                  "args": {"code": "import hermit\nprint(hermit.call('read', path='a/b.txt')['content'])"}})
    # No live socket in selftest: the helper must fail cleanly, not hang.
    assert r["ok"] is False and "tool socket" in (r["stderr"] or "").lower(), r

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
