#!/usr/bin/env python3
"""Tests for the net container. Run inside the image:
    docker run --rm --entrypoint python3 -v "$PWD/net:/app" hermit-net -m unittest -v
No real internet: a local HTTP server plays the page.
"""

import http.server
import ipaddress
import json
import os
import tempfile
import threading
import unittest
from unittest import mock

import server

PAGE = """<!doctype html>
<html><head><title>Проверка страницы</title></head>
<body>
<nav><a href="/a">Меню первое</a> <a href="/b">Меню второе</a></nav>
<article>
<h1>Заголовок статьи</h1>
<p>Первый абзац с достаточно длинным текстом, чтобы извлекатель считал его
основным содержимым страницы, а не обрывком навигации.</p>
<p>Второй абзац тоже содержит осмысленный текст про погоду и климат в городе,
чтобы страница выглядела как настоящая статья.</p>
</article>
<footer>Подвал сайта</footer>
</body></html>"""

PAGE_1251 = """<!doctype html>
<html><head><title>Проверка кодировки</title></head>
<body><article>
<h1>Страница в windows-1251</h1>
<p>Этот абзац набран в старой кодировке, которая до сих пор встречается на
русских сайтах, и заголовок должен читаться без кракозябр.</p>
</article></body></html>"""

PUBLIC_IP = "93.184.216.34"


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/page")
            self.end_headers()
            return
        if self.path == "/cp1251":
            body = PAGE_1251.encode("cp1251")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=windows-1251")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        body = PAGE.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class UrlGuard(unittest.TestCase):
    def test_private_metadata_and_bad_urls_are_rejected(self):
        for url in [
            "http://localhost:8080/",
            "http://127.0.0.1/",
            "http://10.1.2.3/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://metadata.google.internal/",
            "http://nas.local/",
            "file:///etc/passwd",
            "ftp://example.com/",
            "https://user:pass@example.com/",
        ]:
            with self.assertRaises(ValueError, msg=url):
                server.resolve_target(url)

    def test_a_public_address_is_allowed_without_dns(self):
        parts, ip = server.resolve_target(f"http://{PUBLIC_IP}/path")
        self.assertEqual(parts.hostname, PUBLIC_IP)
        self.assertEqual(ip, PUBLIC_IP)

    def test_a_name_that_resolves_to_the_lan_is_rejected(self):
        private = ipaddress.ip_address("10.0.0.9")
        with mock.patch.object(server, "_resolve", return_value=[private]):
            with self.assertRaises(ValueError):
                server.resolve_target("http://example.com/")

    def test_a_mixed_answer_is_rejected(self):
        # One public and one private address must not pass: the private one would be dialed.
        answer = [ipaddress.ip_address(PUBLIC_IP), ipaddress.ip_address("10.0.0.9")]
        with mock.patch.object(server, "_resolve", return_value=answer):
            with self.assertRaises(ValueError):
                server.resolve_target("http://example.com/")


class Net(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.port = cls.httpd.server_address[1]
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()
        cls.base = f"http://127.0.0.1:{cls.port}"

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()

    def fetch(self, args):
        return server.web_fetch(args, allow_private=True)

    def test_page_text_and_title(self):
        out = self.fetch({"url": f"{self.base}/page"})
        self.assertTrue(out["ok"])
        self.assertEqual(out["title"], "Проверка страницы")
        self.assertIn("Заголовок статьи", out["content"])
        self.assertIn("погоду и климат", out["content"])
        self.assertEqual(out["url"], f"{self.base}/page")
        self.assertIsNone(out["next"], "a short page is not paged")

    def test_a_windows_1251_page_is_decoded(self):
        out = self.fetch({"url": f"{self.base}/cp1251"})
        self.assertEqual(out["title"], "Проверка кодировки")
        self.assertIn("старой кодировке", out["content"])

    def test_offset_pages_do_not_overlap(self):
        whole = self.fetch({"url": f"{self.base}/page"})["content"]
        first = self.fetch({"url": f"{self.base}/page", "max_chars": 100})
        self.assertEqual(len(first["content"]), 100)
        self.assertEqual(first["next"], 100)
        second = self.fetch({"url": f"{self.base}/page", "offset": 100, "max_chars": 100})
        self.assertEqual(first["content"] + second["content"], whole[:200])
        self.assertEqual(second["offset"], 100)

    def test_every_redirect_hop_is_checked(self):
        with mock.patch.object(server, "resolve_target", wraps=server.resolve_target) as check:
            out = self.fetch({"url": f"{self.base}/redirect"})
        self.assertTrue(out["ok"])
        self.assertEqual(out["url"], f"{self.base}/page")
        self.assertEqual(check.call_count, 2, "the redirect target was not re-checked")

    def test_unknown_tool_is_an_error(self):
        out = server.dispatch({"id": "1", "tool": "bash", "args": {"cmd": ["ls"]}})
        self.assertFalse(out["ok"])
        self.assertIn("unknown tool", out["error"])

    def test_empty_url_and_query_are_rejected(self):
        self.assertFalse(server.dispatch({"id": "1", "tool": "web_fetch", "args": {}})["ok"])
        self.assertFalse(server.dispatch({"id": "1", "tool": "web_search", "args": {}})["ok"])


class Search(unittest.TestCase):
    def test_results_are_shaped_and_the_limit_reaches_the_child(self):
        hits = [{"title": f"t{i}", "url": f"https://e/{i}", "snippet": "b"} for i in range(3)]
        done = mock.Mock(returncode=0, stdout=json.dumps(hits), stderr="")
        with mock.patch.object(server.subprocess, "run", return_value=done) as run:
            out = server.web_search({"query": "погода", "limit": 3})
        self.assertTrue(out["ok"])
        self.assertEqual(len(out["results"]), 3)
        self.assertEqual(out["results"][0]["url"], "https://e/0")
        argv = run.call_args.args[0]
        self.assertEqual(argv[-1], "3")
        self.assertIn("погода", argv)

    def test_a_failed_search_is_reported(self):
        done = mock.Mock(returncode=1, stdout="", stderr="boom: rate limited")
        with mock.patch.object(server.subprocess, "run", return_value=done):
            with self.assertRaises(ValueError) as ctx:
                server.web_search({"query": "x"})
        self.assertIn("rate limited", str(ctx.exception))

    def test_query_is_clamped_before_the_child_runs(self):
        with self.assertRaises(ValueError):
            server.web_search({"query": "x" * (server.MAX_QUERY + 1)})


class Mcp(unittest.TestCase):
    def tearDown(self):
        server.MCP_LIST = None
        server.MCP_SERVERS.clear()

    def test_rpc_accepts_json_and_picks_the_sse_response(self):
        assert server._rpc(b'{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}', "application/json")[
            "result"
        ] == {"tools": []}
        sse = (
            b'event: message\ndata: {"jsonrpc":"2.0","method":"notifications/progress"}\n\n'
            b'event: message\ndata: {"jsonrpc":"2.0","id":2,"result":{"content":[]}}\n\n'
        )
        assert server._rpc(sse, "text/event-stream")["id"] == 2

    def test_config_reads_headers_and_drops_hop_by_hop(self):
        with tempfile.NamedTemporaryFile("w", delete=False, encoding="utf-8") as f:
            json.dump([{
                "name": "github",
                "url": "https://example.com/mcp",
                "approval": True,
                "allow_private": True,
                "headers": {"Authorization": "Bearer x", "Host": "evil", "Content-Length": "9"},
            }], f)
            path = f.name
        try:
            with mock.patch.object(server, "MCP_FILE", path):
                cfg = server._mcp_config()
        finally:
            os.unlink(path)
        self.assertEqual(cfg[0]["name"], "github")
        self.assertTrue(cfg[0]["approval"])
        self.assertTrue(cfg[0]["allow_private"])
        self.assertEqual(cfg[0]["headers"], {"Authorization": "Bearer x"})

    def test_empty_or_missing_config_means_no_tools(self):
        server.MCP_SERVERS.clear()
        with mock.patch.object(server, "MCP_FILE", "/nonexistent/hermit-mcp.json"):
            out = server.mcp_list({})
        self.assertTrue(out["ok"])
        self.assertEqual(out["servers"], [])

    def test_a_broken_server_is_probed_again_and_a_healthy_list_is_cached(self):
        server.MCP_LIST = None
        cfg = [{"name": "demo", "url": "http://x/", "headers": {},
                "approval": False, "allow_private": False}]
        broken = {"name": "demo", "approval": False, "tools": [], "error": "down"}
        with mock.patch.object(server, "_mcp_config", return_value=cfg), \
                mock.patch.object(server, "_mcp_probe", return_value=broken) as probe:
            server.mcp_list({})
            server.mcp_list({})
        self.assertEqual(probe.call_count, 2, "a broken server must not be cached")

        healthy = {"name": "demo", "approval": False, "tools": []}
        with mock.patch.object(server, "_mcp_config", return_value=cfg), \
                mock.patch.object(server, "_mcp_probe", return_value=healthy) as probe:
            server.mcp_list({})
            server.mcp_list({})
        self.assertEqual(probe.call_count, 1, "a healthy list is cached")

    def test_full_handshake_over_local_http(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            session = None

            def log_message(self, *args):
                pass

            def do_POST(self):
                n = int(self.headers.get("content-length") or 0)
                req = json.loads(self.rfile.read(n) or b"{}")
                method = req.get("method")
                if method == "notifications/initialized":
                    self.send_response(202)
                    self.send_header("content-length", "0")
                    self.end_headers()
                    return
                if method == "initialize":
                    out = {"jsonrpc": "2.0", "id": req["id"],
                           "result": {"protocolVersion": "2025-06-18"}}
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                    self.send_header("Mcp-Session-Id", "s1")
                elif method == "tools/list":
                    out = {"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [
                        {"name": "echo", "description": "e",
                         "inputSchema": {"type": "object", "properties": {}}}
                    ]}}
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                elif method == "tools/call":
                    out = {"jsonrpc": "2.0", "id": req["id"], "result": {
                        "content": [{"type": "text", "text": "hi"}]}}
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                else:
                    out = {"jsonrpc": "2.0", "id": req["id"], "error": {"message": "no"}}
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                raw = json.dumps(out).encode()
                if self.headers.get("accept", "").find("event-stream") >= 0 and method == "tools/call":
                    raw = b"data: " + raw + b"\n\n"
                self.send_header("content-length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        url = f"http://127.0.0.1:{srv.server_address[1]}/mcp"
        try:
            with mock.patch.object(
                server, "resolve_target",
                return_value=(server.urlsplit(url), "127.0.0.1"),
            ):
                mcp = server._Mcp({
                    "name": "demo", "url": url, "headers": {}, "approval": False,
                })
                tools = mcp.list_tools()
                result = mcp.call("echo", {"x": 1})
        finally:
            srv.shutdown()
            srv.server_close()
        self.assertEqual(tools[0]["name"], "echo")
        self.assertEqual(result["content"][0]["text"], "hi")

    def test_handshake_lists_tools_and_calls_one(self):
        mcp = server._Mcp({
            "name": "demo", "url": "https://e/mcp", "headers": {}, "approval": False,
        })
        replies = [
            {"result": {"protocolVersion": "2025-06-18"}},
            None,
            {"result": {"tools": [{"name": "echo"}], "nextCursor": None}},
            {"result": {"content": [{"type": "text", "text": "hi"}]}},
        ]
        with mock.patch.object(mcp, "_post", side_effect=replies) as post:
            tools = mcp.list_tools()
            result = mcp.call("echo", {"x": 1})
        self.assertEqual(tools[0]["name"], "echo")
        self.assertEqual(result["content"][0]["text"], "hi")
        methods = [call.args[0].get("method") for call in post.call_args_list]
        self.assertEqual(methods, ["initialize", "notifications/initialized",
                                   "tools/list", "tools/call"])

    def test_call_requires_server_and_tool(self):
        with self.assertRaises(ValueError):
            server.mcp_call({"server": "x"})
        with self.assertRaises(ValueError):
            server.mcp_call({"server": "missing", "tool": "t", "arguments": {}})


if __name__ == "__main__":
    unittest.main()
