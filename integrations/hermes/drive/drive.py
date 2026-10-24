"""Drive a Hermes gateway's chat sessions over its /api/ws JSON-RPC socket.

The gateway binds loopback, so --url is normally a `dumbpipe connect-tcp`
listener; the `drive` wrapper starts one when given a ticket. One connection
per invocation: a session outlives it, and `--session` (a stored id) resumes.
"""
import argparse
import asyncio
import json
import sys

from websockets.asyncio.client import connect


class Gateway:
    def __init__(self, ws):
        self.ws = ws
        self.n = 0
        self.events = asyncio.Queue()
        self.pending = {}
        self.reader = asyncio.create_task(self._read())

    async def _read(self):
        try:
            async for raw in self.ws:
                f = json.loads(raw)
                fut = self.pending.get(f.get("id"))
                if fut is not None and not fut.done():
                    if "error" in f:
                        fut.set_exception(RuntimeError(json.dumps(f["error"])))
                    else:
                        fut.set_result(f.get("result", {}))
                elif f.get("method") == "event":
                    self.events.put_nowait(f["params"])
        finally:
            self.events.put_nowait({"type": "disconnected", "payload": {}})

    async def rpc(self, method, timeout=30, **params):
        self.n += 1
        fut = asyncio.get_running_loop().create_future()
        self.pending[self.n] = fut
        await self.ws.send(json.dumps(
            {"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}))
        return await asyncio.wait_for(fut, timeout)


async def send(g, a):
    if a.session:
        r = await g.rpc("session.resume", session_id=a.session)
    else:
        opts = {k: v for k, v in (("profile", a.profile), ("model", a.model), ("provider", a.provider)) if v}
        r = await g.rpc("session.create", **opts)
    sid, stored = r["session_id"], r.get("stored_session_id") or a.session
    await g.rpc("prompt.submit", session_id=sid, text=a.text)
    reply, failed = "", False
    while True:
        try:
            e = await asyncio.wait_for(g.events.get(), a.timeout)
        except asyncio.TimeoutError:
            print(f"timed out after {a.timeout}s", file=sys.stderr)
            failed = True
            break
        t, p = e.get("type"), e.get("payload") or {}
        if e.get("session_id") not in (None, sid):
            continue
        if t == "tool.start":
            print(f"[tool] {p.get('name')} {json.dumps(p.get('args', p.get('context', '')))[:200]}", file=sys.stderr)
        elif t == "tool.complete":
            print(f"[tool done] {p.get('name')} {str(p.get('error') or '')[:200]}", file=sys.stderr)
        elif t == "message.delta" and a.stream:
            print(p.get("text", ""), end="", flush=True)
        elif t == "error":
            print(f"error: {p.get('message')}", file=sys.stderr)
            failed = True
            break
        elif t == "disconnected":
            print("gateway disconnected", file=sys.stderr)
            failed = True
            break
        elif t == "message.complete":
            reply = p.get("text", "")
            failed = p.get("status") == "error"
            break
    if not a.stream:
        print(reply)
    print(f"session: {stored}", file=sys.stderr)
    return 1 if failed else 0


async def sessions(g, a):
    r = await g.rpc("session.list", limit=a.limit)
    for s in r["sessions"]:
        print(s.get("id"), s.get("title") or s.get("preview") or "")
    return 0


async def history(g, a):
    r = await g.rpc("session.resume", session_id=a.session)
    for m in r.get("messages", []):
        print(f"{m.get('role')}: {m.get('text') or m.get('content') or ''}")
    return 0


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--url", required=True, help="http://host:port of the gateway")
    p.add_argument("--token", default="caos")
    sub = p.add_subparsers(dest="verb", required=True)
    s = sub.add_parser("send")
    s.add_argument("text")
    s.add_argument("--session", help="stored session id to resume; omitted starts one")
    s.add_argument("--profile")
    s.add_argument("--model")
    s.add_argument("--provider")
    s.add_argument("--timeout", type=float, default=300)
    s.add_argument("--stream", action="store_true", help="print the reply as it arrives")
    s.set_defaults(fn=send)
    s = sub.add_parser("list")
    s.add_argument("--limit", type=int, default=20)
    s.set_defaults(fn=sessions)
    s = sub.add_parser("history")
    s.add_argument("session")
    s.set_defaults(fn=history)
    a = p.parse_args()
    ws_url = a.url.replace("http", "ws", 1).rstrip("/") + f"/api/ws?token={a.token}"
    async with connect(ws_url, open_timeout=15, proxy=None, max_size=8 << 20) as ws:
        g = Gateway(ws)
        return await a.fn(g, a)


sys.exit(asyncio.run(main()))
