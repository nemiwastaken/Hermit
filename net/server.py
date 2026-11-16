#!/usr/bin/env python3
"""Hermit net: the only container that talks to the internet.

Same protocol as the sandbox: one JSON object per line, one back.
    -> {"id":"e42/7","tool":"web_fetch","args":{"url":"https://example.com"}}
    <- {"id":"e42/7","ok":true,"status":200,"title":"...","content":"...","chars":42}

Two read-only tools. web_fetch returns the main text of a page as markdown,
paged by offset. web_search asks DuckDuckGo for URLs. There is no browser:
a page that needs JavaScript or a login comes back empty, and that is a
deliberate limit, not a failure to retry.

The container mounts no workspace and no behavior, so a page can only ever
see what the model chooses to send it. Every URL is checked against private
networks on each redirect hop, and the connection is pinned to the address
that was checked, so DNS cannot point at the LAN between check and connect.
"""

import argparse
import html
import http.client
import ipaddress
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urljoin, urlsplit

import trafilatura

MAX_BYTES = 3_000_000        # a page, not a disk image
TIMEOUT = 20.0               # one HTTP request
MAX_REDIRECTS = 5
DEFAULT_CHARS = 6_000        # one tool result must fit the core's 8k clamp
MAX_CHARS = 7_000
SEARCH_TIMEOUT = 30          # ddgs has no overall cap of its own
MAX_QUERY = 500
MAX_URL = 2_000
MCP_FILE = os.environ.get("HERMIT_MCP_FILE", "/etc/hermit/mcp.json")
MCP_TIMEOUT = 30.0
MCP_SERVERS = {}
MCP_LIST = None
MCP_LOCK = threading.Lock()

# A plain desktop Firefox is less likely to get a bot page than a curl-looking UA.
USER_AGENT = "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0"
HEADERS = {
    "User-Agent": USER_AGENT,
    "Accept": "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.1",
    "Accept-Language": "ru,en;q=0.8",
}

# Always blocked, whatever DNS says.
BLOCKED_HOSTNAMES = frozenset({"metadata.google.internal", "metadata.goog"})
_BLOCKED_SUFFIXES = (".local", ".internal", ".localhost")


def _resolve(host, port):
    infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    seen, out = set(), []
    for info in infos:
        ip = ipaddress.ip_address(info[4][0].split("%", 1)[0])
        if ip not in seen:
            seen.add(ip)
            out.append(ip)
    return out


def _public(ip):
    if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped:
        ip = ip.ipv4_mapped
    return ip.is_global


def resolve_target(url, allow_private=False):
    """Check a URL and return (parts, ip_to_connect). Raises ValueError."""
    parts = urlsplit(url.strip())
    if parts.scheme.lower() not in ("http", "https"):
        raise ValueError("only http and https URLs are allowed")
    if not parts.hostname:
        raise ValueError("url has no host")
    if parts.username or parts.password:
        raise ValueError("url must not include credentials")
    try:
        port = parts.port or (443 if parts.scheme.lower() == "https" else 80)
    except ValueError:
        raise ValueError("url has an invalid port")

    host = parts.hostname.lower()
    if not allow_private:
        if host in BLOCKED_HOSTNAMES or host == "localhost" or host.endswith(_BLOCKED_SUFFIXES):
            raise ValueError(f"{host} is not reachable from this container")

    try:
        ips = [ipaddress.ip_address(host)]
    except ValueError:
        try:
            ips = _resolve(host, port)
        except OSError as e:
            raise ValueError(f"cannot resolve {host}: {e}")
    if not ips:
        raise ValueError(f"cannot resolve {host}")

    if not allow_private:
        for ip in ips:
            if not _public(ip):
                raise ValueError(f"{host} resolves to a private address")
    return parts, str(ips[0])


class _HTTP(http.client.HTTPConnection):
    def __init__(self, host, ip, port, timeout):
        super().__init__(host, port, timeout=timeout)
        self._ip = ip

    def connect(self):
        self.sock = socket.create_connection((self._ip, self.port), self.timeout)


class _HTTPS(http.client.HTTPSConnection):
    def __init__(self, host, ip, port, timeout, context):
        super().__init__(host, port, timeout=timeout, context=context)
        self._ip = ip

    def connect(self):
        sock = socket.create_connection((self._ip, self.port), self.timeout)
        self.sock = self._context.wrap_socket(sock, server_hostname=self.host)


def _post_once(url, payload, extra_headers, allow_private=False):
    """POST with the same SSRF checks and IP pinning as GET."""
    parts, ip = resolve_target(url, allow_private)
    port = parts.port or (443 if parts.scheme.lower() == "https" else 80)
    path = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    body = json.dumps(payload, separators=(",", ":")).encode()
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "User-Agent": USER_AGENT,
        **extra_headers,
    }
    if parts.scheme.lower() == "https":
        conn = _HTTPS(parts.hostname, ip, port, MCP_TIMEOUT, ssl.create_default_context())
    else:
        conn = _HTTP(parts.hostname, ip, port, MCP_TIMEOUT)
    try:
        conn.request("POST", path, body=body, headers=headers)
        resp = conn.getresponse()
        return resp.status, resp.headers, resp.read(MAX_BYTES + 1), url
    finally:
        conn.close()


def _post_json(url, payload, extra_headers, allow_private=False):
    for hop in range(4):
        status, headers, body, final = _post_once(url, payload, extra_headers, allow_private)
        location = headers.get("location")
        if status in (301, 302, 303, 307, 308) and location:
            if hop >= 3:
                raise ValueError("more than 3 redirects")
            url = urljoin(url, location)
            continue
        if len(body) > MAX_BYTES:
            raise ValueError("MCP response is larger than 3 MB")
        return status, headers, body, final
    raise ValueError("redirect loop")


def _rpc(raw, ctype):
    text = raw.decode("utf-8", "replace")
    if "text/event-stream" in (ctype or ""):
        last = None
        for line in text.splitlines():
            line = line.strip()
            if not line.startswith("data:"):
                continue
            data = line[5:].strip()
            if not data or data == "[DONE]":
                continue
            try:
                value = json.loads(data)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict) and ("result" in value or "error" in value):
                last = value
        if last is None:
            raise ValueError("MCP event stream had no response")
        return last
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        raise ValueError(f"MCP returned non-JSON: {text[:200]!r}")


def _mcp_headers(raw):
    out = {}
    for key, value in (raw or {}).items():
        if not isinstance(key, str) or not isinstance(value, str):
            continue
        if key.lower() in ("host", "content-length", "connection"):
            continue
        out[key] = value
    return out


def _mcp_config():
    try:
        with open(MCP_FILE, encoding="utf-8") as f:
            raw = f.read().strip()
    except FileNotFoundError:
        return []
    if not raw:
        return []
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        raise ValueError(f"{MCP_FILE}: {e}")
    if not isinstance(data, list):
        raise ValueError(f"{MCP_FILE}: expected an array of servers")
    out = []
    for i, cfg in enumerate(data):
        if not isinstance(cfg, dict):
            raise ValueError(f"{MCP_FILE}: server {i + 1} must be an object")
        name = str(cfg.get("name") or f"server{i + 1}")
        url = str(cfg.get("url") or "")
        if not url:
            raise ValueError(f"{MCP_FILE}: server {name} has no url")
        out.append({
            "name": name,
            "url": url,
            "headers": _mcp_headers(cfg.get("headers")),
            "approval": bool(cfg.get("approval")),
            # Opt-in for a server on the LAN. Off by default: the same SSRF
            # rule as web_fetch must not be bypassed by a stray config line.
            "allow_private": bool(cfg.get("allow_private")),
        })
    return out


class _Mcp:
    """Minimal Streamable HTTP client: initialize, tools/list, tools/call.
    Only remote HTTP servers: the net container cannot see a local process."""

    def __init__(self, cfg):
        self.name = cfg["name"]
        self.url = cfg["url"]
        self.headers = cfg["headers"]
        self.approval = cfg["approval"]
        self.allow_private = cfg.get("allow_private", False)
        self.session = None
        self.tools = None
        self.ready = False
        self.next_id = 0
        self.protocol = "2025-06-18"

    def _post(self, payload, want=True):
        headers = dict(self.headers)
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        if self.ready:
            headers["MCP-Protocol-Version"] = self.protocol
        status, resp_headers, body, final = _post_json(
            self.url, payload, headers, self.allow_private
        )
        session = resp_headers.get("Mcp-Session-Id")
        if session:
            self.session = session
        if status >= 400:
            raise ValueError(f"HTTP {status} from {final}: {body[:300]!r}")
        if not want:
            return None
        return _rpc(body, resp_headers.get("Content-Type", ""))

    def _rpc(self, method, params=None):
        self.next_id += 1
        payload = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            payload["params"] = params
        response = self._post(payload)
        if "error" in response:
            raise ValueError(f"MCP error: {response['error']}")
        return response.get("result") or {}

    def start(self):
        if self.ready:
            return
        self._post({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": self.protocol,
                "capabilities": {},
                "clientInfo": {"name": "hermit", "version": "0.1"},
            },
        })
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"}, want=False)
        self.ready = True

    def list_tools(self):
        if self.tools is not None:
            return self.tools
        self.start()
        tools, cursor = [], None
        for _ in range(10):
            result = self._rpc("tools/list", {"cursor": cursor} if cursor else {})
            tools.extend(result.get("tools") or [])
            cursor = result.get("nextCursor")
            if not cursor:
                break
        self.tools = tools
        return tools

    def call(self, tool, arguments):
        self.start()
        params = {"name": tool, "arguments": arguments}
        try:
            return self._rpc("tools/call", params)
        except ValueError as e:
            # A restarted server forgets the session; one fresh handshake is
            # enough, and only when the failure looks like a dead session.
            if self.session and ("HTTP 400" in str(e) or "HTTP 404" in str(e)):
                self.ready = False
                self.session = None
                self.start()
                return self._rpc("tools/call", params)
            raise


def _mcp_probe(cfg):
    server = _Mcp(cfg)
    MCP_SERVERS[server.name] = server
    item = {"name": server.name, "approval": server.approval, "tools": []}
    try:
        for tool in server.list_tools():
            item["tools"].append({
                "name": str(tool.get("name") or ""),
                "description": str(tool.get("description") or ""),
                "inputSchema": tool.get("inputSchema") or {
                    "type": "object", "properties": {},
                },
            })
    except Exception as e:
        item["error"] = f"{type(e).__name__}: {e}"
    return item


def mcp_list(args):
    global MCP_LIST
    with MCP_LOCK:
        if MCP_LIST is not None:
            return {"ok": True, "servers": MCP_LIST}
        configs = _mcp_config()
        # One slow server must not hold up the others at startup.
        with ThreadPoolExecutor(max_workers=max(1, min(4, len(configs)))) as pool:
            servers = list(pool.map(_mcp_probe, configs))
        # Cache only a fully healthy list: a server that was down gets another
        # chance on the next refresh instead of staying broken until a restart.
        if all("error" not in s for s in servers):
            MCP_LIST = servers
        return {"ok": True, "servers": servers}


def mcp_call(args):
    name = args.get("server")
    tool = args.get("tool")
    if not isinstance(name, str) or not isinstance(tool, str) or not tool:
        raise ValueError("server and tool are required")
    server = MCP_SERVERS.get(name)
    if server is None:
        for cfg in _mcp_config():
            if cfg["name"] == name:
                server = MCP_SERVERS[name] = _Mcp(cfg)
                break
    if server is None:
        raise ValueError(f"unknown MCP server {name!r}")
    arguments = args.get("arguments") or {}
    if not isinstance(arguments, dict):
        raise ValueError("arguments must be an object")
    result = server.call(tool, arguments)
    return {
        "ok": True,
        "server": name,
        "tool": tool,
        "is_error": bool(result.get("isError")),
        "content": result.get("content") or [],
    }


def _get_once(url, allow_private):
    parts, ip = resolve_target(url, allow_private)
    port = parts.port or (443 if parts.scheme.lower() == "https" else 80)
    path = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    if parts.scheme.lower() == "https":
        conn = _HTTPS(parts.hostname, ip, port, TIMEOUT, ssl.create_default_context())
    else:
        conn = _HTTP(parts.hostname, ip, port, TIMEOUT)
    try:
        conn.request("GET", path, headers=HEADERS)
        resp = conn.getresponse()
        return resp.status, resp.headers, resp.read(MAX_BYTES + 1)
    finally:
        conn.close()


def fetch(url, allow_private=False):
    """GET with a manual redirect loop: every hop goes through resolve_target."""
    for hop in range(MAX_REDIRECTS + 1):
        status, headers, body = _get_once(url, allow_private)
        location = headers.get("location")
        if status in (301, 302, 303, 307, 308) and location:
            if hop >= MAX_REDIRECTS:
                raise ValueError(f"more than {MAX_REDIRECTS} redirects")
            url = urljoin(url, location)
            continue
        if status >= 400:
            raise ValueError(f"HTTP {status} from {url}")
        if len(body) > MAX_BYTES:
            raise ValueError(f"page is larger than {MAX_BYTES} bytes")
        return status, headers, body, url
    raise ValueError("redirect loop")


_TITLE = re.compile(r"<title[^>]*>(.*?)</title>", re.I | re.S)


_META_CHARSET = re.compile(rb'<meta[^>]+charset=["\']?([\w-]+)', re.I)


def _decode(body, headers):
    """Response charset first, then <meta charset>, then UTF-8. Russian sites
    still ship windows-1251, often with no charset in the header."""
    charset = headers.get_content_charset()
    if not charset:
        match = _META_CHARSET.search(body[:4096])
        charset = match.group(1).decode("ascii", "ignore") if match else "utf-8"
    try:
        return body.decode(charset, "replace")
    except LookupError:
        return body.decode("utf-8", "replace")


def _title(text):
    match = _TITLE.search(text[:200_000])
    return " ".join(html.unescape(match.group(1)).split()) if match else ""


def _textual(ctype):
    return (
        ctype.startswith("text/")
        or ctype in ("application/json", "application/xml", "application/xhtml+xml")
        or ctype.endswith(("+json", "+xml"))
    )


def _squash(text):
    # trafilatura already spaces paragraphs; raw files often have runs of blank lines.
    return re.sub(r"\n{3,}", "\n\n", text.replace("\r\n", "\n")).strip()


def _page_text(ctype, text, url):
    if ctype in ("text/html", "application/xhtml+xml"):
        text = trafilatura.extract(
            text,
            url=url,
            output_format="markdown",
            include_links=True,
            include_formatting=True,
            favor_recall=True,
        )
        if not text:
            raise ValueError(
                "no readable text on the page (it may need JavaScript, a login or a browser)"
            )
        return text
    if _textual(ctype):
        return text
    raise ValueError(f"unsupported content type {ctype or 'unknown'}")


def web_fetch(args, allow_private=False):
    url = args.get("url")
    if not isinstance(url, str) or not url.strip():
        raise ValueError("url is required")
    url = url.strip()
    if len(url) > MAX_URL:
        raise ValueError("url is too long")
    offset = max(int(args.get("offset") or 0), 0)
    max_chars = min(max(int(args.get("max_chars") or DEFAULT_CHARS), 100), MAX_CHARS)

    status, headers, body, final_url = fetch(url, allow_private)
    ctype = (headers.get("content-type") or "").split(";", 1)[0].strip().lower()
    decoded = _decode(body, headers)
    text = _squash(_page_text(ctype, decoded, final_url))
    total = len(text)
    chunk = text[offset : offset + max_chars]
    nxt = offset + len(chunk)
    return {
        "ok": True,
        "status": status,
        "url": final_url,
        "title": _title(decoded),
        "content": chunk,
        "chars": total,
        "offset": offset,
        "next": nxt if nxt < total else None,
    }


# ddgs can block in native code while holding the GIL, so it runs in a child
# process the parent can kill. Plain argv, no shell.
_SEARCH_CHILD = """
import json, sys
from ddgs import DDGS
query, limit = sys.argv[1], int(sys.argv[2])
with DDGS(timeout=10) as client:
    hits = client.text(query, max_results=limit)
out = [
    {
        "title": str(h.get("title") or ""),
        "url": str(h.get("href") or h.get("url") or ""),
        "snippet": str(h.get("body") or ""),
    }
    for h in hits
][:limit]
print(json.dumps(out))
"""


def web_search(args):
    query = args.get("query")
    if not isinstance(query, str) or not query.strip():
        raise ValueError("query is required")
    query = query.strip()
    if len(query) > MAX_QUERY:
        raise ValueError("query is too long")
    limit = min(max(int(args.get("limit") or 5), 1), 10)

    try:
        proc = subprocess.run(
            [sys.executable, "-c", _SEARCH_CHILD, query, str(limit)],
            capture_output=True,
            text=True,
            timeout=SEARCH_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        raise ValueError(f"search timed out after {SEARCH_TIMEOUT}s")
    if proc.returncode != 0:
        lines = (proc.stderr or "").strip().splitlines()
        raise ValueError(f"search failed: {lines[-1] if lines else 'unknown error'}")
    return {"ok": True, "results": json.loads(proc.stdout or "[]")}


def dispatch(req):
    req_id, tool, args = req.get("id"), req.get("tool"), req.get("args") or {}
    if not isinstance(args, dict):
        return {"id": req_id, "ok": False, "error": "args must be an object"}
    try:
        if tool == "web_fetch":
            return {"id": req_id, **web_fetch(args)}
        if tool == "web_search":
            return {"id": req_id, **web_search(args)}
        if tool == "mcp_list":
            return {"id": req_id, **mcp_list(args)}
        if tool == "mcp_call":
            return {"id": req_id, **mcp_call(args)}
    except ValueError as e:
        return {"id": req_id, "ok": False, "error": str(e)}
    except Exception as e:  # surface any failure to the model, never crash
        return {"id": req_id, "ok": False, "error": f"{type(e).__name__}: {e}"}
    return {"id": req_id, "ok": False, "error": f"unknown tool {tool!r}"}


def handle(conn):
    with conn:
        rf = conn.makefile("r", encoding="utf-8", errors="replace")
        for line in rf:
            line = line.strip()
            if not line:
                continue
            try:
                resp = dispatch(json.loads(line))
            except json.JSONDecodeError as e:
                resp = {"id": None, "ok": False, "error": f"invalid JSON: {e}"}
            conn.sendall((json.dumps(resp, ensure_ascii=False) + "\n").encode())


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
    print(f"net listening on {sock_path}", flush=True)

    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", default="/run/hermit/net.sock")
    serve(parser.parse_args().socket)
