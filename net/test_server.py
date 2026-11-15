#!/usr/bin/env python3
"""Tests for the net container. Run inside the image:
    docker run --rm --entrypoint python3 -v "$PWD/net:/app" hermit-net -m unittest -v
No real internet: a local HTTP server plays the page.
"""

import http.server
import ipaddress
import json
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


if __name__ == "__main__":
    unittest.main()
