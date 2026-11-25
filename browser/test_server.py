"""Integration tests against the Obscura executable in the browser image."""

import asyncio
import base64
import json
import tempfile
import unittest
from pathlib import Path
from urllib.parse import quote

from server import Browser, MAX_REQUEST, MAX_RESPONSE


class BrowserIntegration(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        directory = self.enterContext(tempfile.TemporaryDirectory())
        self.socket = str(Path(directory) / "browser.sock")
        self.browser = Browser()
        listener = await asyncio.start_unix_server(
            self.browser.handle, path=self.socket, limit=MAX_REQUEST
        )
        await self.enterAsyncContext(listener)
        self.addAsyncCleanup(self.close_sessions)

    async def close_sessions(self):
        for session in self.browser.sessions.values():
            await session.close()

    async def rpc(self, tool, args):
        reader, writer = await asyncio.open_unix_connection(
            self.socket, limit=MAX_RESPONSE + 1024
        )
        try:
            request = {"id": "test", "tool": tool, "args": args}
            writer.write(json.dumps(request).encode() + b"\n")
            await writer.drain()
            response = json.loads(await asyncio.wait_for(reader.readline(), 15))
            self.assertEqual(response["id"], "test")
            self.assertTrue(response["ok"], response)
            return response
        finally:
            writer.close()
            await writer.wait_closed()

    async def call(self, tool, chat_id=100, **arguments):
        return await self.rpc("mcp_call", {
            "chat_id": chat_id, "tool": f"browser_{tool}", "arguments": arguments,
        })

    async def test_interaction_and_rendering_stay_in_the_callers_session(self):
        listed = await self.rpc("mcp_list", {})
        tools = {tool["name"] for tool in listed["servers"][0]["tools"]}
        self.assertTrue({"browser_snapshot", "browser_tab_new", "browser_screenshot", "browser_pdf"} <= tools)

        page = """<title>Hermit browser</title>
            <button id="action" onclick="this.textContent='clicked'">Click me</button>"""
        self.assertFalse((await self.call("navigate", url=f"data:text/html,{quote(page)}"))["is_error"])
        self.assertFalse((await self.call("click", selector="#action"))["is_error"])
        self.assertIn("clicked", str(await self.call("evaluate", expression="document.querySelector('#action').textContent")))
        self.assertIn("Hermit browser", str(await self.call("snapshot")))
        self.assertNotIn("Hermit browser", str(await self.call("snapshot", chat_id=101)))

        screenshot = (await self.call("screenshot"))["content"][0]
        self.assertTrue(base64.b64decode(screenshot["data"]).startswith(b"\x89PNG\r\n\x1a\n"))
        pdf = (await self.call("pdf"))["content"][0]["resource"]
        self.assertTrue(base64.b64decode(pdf["blob"]).startswith(b"%PDF-"))

    async def test_browser_blocks_local_files_and_loopback(self):
        for url in ("file:///etc/passwd", "http://127.0.0.1/"):
            with self.subTest(url=url):
                self.assertTrue((await self.call("navigate", url=url))["is_error"])


if __name__ == "__main__":
    unittest.main()
