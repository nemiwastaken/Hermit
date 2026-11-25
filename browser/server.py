"""Expose Obscura MCP tools over Hermit's Unix socket protocol."""

import asyncio
import contextlib
import json
import os
from collections import OrderedDict


SOCKET = "/run/hermit/browser.sock"
MAX_REQUEST = 256 * 1024
MAX_RESPONSE = 12 * 1024 * 1024
MAX_SESSIONS = 4
TOOL_TIMEOUT = 45


class Session:
    def __init__(self, process):
        self.process = process
        self.request_id = 0

    @classmethod
    async def start(cls):
        process = await asyncio.create_subprocess_exec(
            "obscura", "mcp",
            cwd="/tmp",
            env={"PATH": os.defpath + ":/usr/local/bin", "HOME": "/tmp"},
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            limit=MAX_RESPONSE,
        )
        session = cls(process)
        try:
            await session.rpc("initialize", {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "hermit", "version": "0.1"},
            })
            await session.send({"method": "notifications/initialized"})
        except (Exception, asyncio.CancelledError):
            await session.close()
            raise
        return session

    async def send(self, payload):
        line = json.dumps({"jsonrpc": "2.0", **payload}).encode() + b"\n"
        self.process.stdin.write(line)
        await self.process.stdin.drain()

    async def rpc(self, method, params=None):
        self.request_id += 1
        async with asyncio.timeout(TOOL_TIMEOUT):
            await self.send({"id": self.request_id, "method": method, "params": params or {}})
            line = await self.process.stdout.readline()
        if not line:
            raise ValueError("Obscura closed the browser session")
        reply = json.loads(line)
        if reply.get("id") != self.request_id:
            raise ValueError("Unexpected Obscura response")
        if "error" in reply:
            raise ValueError(reply["error"].get("message", "Obscura MCP error"))
        return reply["result"]

    async def close(self):
        if self.process.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                self.process.kill()
        self.process.stdin.close()
        # A full stdout pipe can keep asyncio's process transport open.
        while await self.process.stdout.read(64 * 1024):
            pass
        await self.process.wait()


class Browser:
    def __init__(self):
        self.sessions = OrderedDict()
        self.tools = None
        self.lock = asyncio.Lock()

    async def list_tools(self):
        if self.tools is None:
            probe = await Session.start()
            try:
                result = await probe.rpc("tools/list")
                self.tools = {
                    tool["name"]: tool for tool in result["tools"]
                    if tool["name"].startswith("browser_")
                }
            finally:
                await probe.close()
        return {"servers": [{"name": "obscura", "tools": list(self.tools.values())}]}

    async def call(self, args):
        chat_id, tool, arguments = args.get("chat_id"), args.get("tool"), args.get("arguments")
        if type(chat_id) is not int or not isinstance(arguments, dict):
            raise ValueError("Browser calls require a chat ID and an arguments object")
        if self.tools is None:
            await self.list_tools()
        if not isinstance(tool, str) or tool not in self.tools:
            raise ValueError("Unknown browser tool")
        session = self.sessions.pop(chat_id, None)
        if session is None:
            if len(self.sessions) >= MAX_SESSIONS:
                _, oldest = self.sessions.popitem(last=False)
                await oldest.close()
            session = await Session.start()
        try:
            result = await session.rpc("tools/call", {"name": tool, "arguments": arguments})
        except (Exception, asyncio.CancelledError):
            await session.close()
            raise
        self.sessions[chat_id] = session
        return {"is_error": bool(result.get("isError")), "content": result.get("content", [])}

    async def handle(self, reader, writer):
        request_id = None
        try:
            request = json.loads(await asyncio.wait_for(reader.readline(), timeout=5))
            request_id = request.get("id")
            async with self.lock:
                if request.get("tool") == "mcp_list":
                    result = await self.list_tools()
                elif request.get("tool") == "mcp_call":
                    result = await self.call(request["args"])
                else:
                    raise ValueError("Unknown browser operation")
            response = {"id": request_id, "ok": True, **result}
        except TimeoutError:
            response = {"id": request_id, "ok": False, "error": "Browser request timed out"}
        except Exception as error:
            response = {"id": request_id, "ok": False, "error": str(error)}
        try:
            writer.write(json.dumps(response, ensure_ascii=False).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), timeout=5)
        except (ConnectionError, TimeoutError):
            # Core may close the socket if it times out while we are busy.
            writer.transport.abort()
        finally:
            writer.close()


async def serve():
    browser = Browser()
    with contextlib.suppress(FileNotFoundError):
        os.unlink(SOCKET)
    server = await asyncio.start_unix_server(browser.handle, path=SOCKET, limit=MAX_REQUEST)
    os.chmod(SOCKET, 0o660)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(serve())
