"""Minimal stdlib MCP (JSON-RPC over stdio) client and test helpers for cad-render-mcp."""
import base64, json, os, subprocess, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BIN = os.environ.get("CAD_RENDER_BIN", os.path.join(ROOT, "target/release/cad-render-mcp"))


class Client:
    """Spawns the server and speaks MCP to it, exactly like an MCP host (e.g. Claude Code) does."""

    def __init__(self, cmd=None):
        self.p = subprocess.Popen(cmd or [DEFAULT_BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.id = 0
        self.init = self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "tests", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, msg):
        self.p.stdin.write((json.dumps(msg) + "\n").encode())
        self.p.stdin.flush()

    def request(self, method, params=None):
        self.id += 1
        self.send({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params or {}})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout; stderr:\n" + self.p.stderr.read().decode())
            msg = json.loads(line)  # any non-JSON on stdout fails here: the protocol stream must stay clean
            if msg.get("id") == self.id:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg["result"]

    def call(self, name, args=None):
        t = time.perf_counter()
        r = self.request("tools/call", {"name": name, "arguments": args or {}})
        return r, (time.perf_counter() - t) * 1e3

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)


def text_of(r):
    return "\n".join(c["text"] for c in r["content"] if c["type"] == "text")


def images_of(r):
    return [base64.b64decode(c["data"]) for c in r["content"] if c["type"] == "image"]


def png_size(data):
    return int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")


class Suite:
    """Collects checks and timings; `step` calls a tool, prints its text and saves any image."""

    def __init__(self, out):
        self.out = out
        os.makedirs(out, exist_ok=True)
        self.failures, self.timings = [], []

    def check(self, cond, what):
        print(("  ok   " if cond else "  FAIL ") + what)
        if not cond:
            self.failures.append(what)

    def step(self, c, label, tool, args, expect_image=False, quiet=False):
        r, ms = c.call(tool, args)
        self.timings.append((label, ms))
        t = text_of(r)
        if not quiet:
            print(f"\n[{label}] {tool} {ms:.0f} ms\n  " + t.replace("\n", "\n  ")[:700])
        self.check(not r.get("isError"), f"{label}: no tool error")
        img = None
        imgs = images_of(r)
        if expect_image:
            self.check(bool(imgs) and imgs[0][:8] == b"\x89PNG\r\n\x1a\n", f"{label}: inline PNG returned")
        if imgs:
            img = imgs[0]
            open(os.path.join(self.out, label.replace(" ", "_") + ".png"), "wb").write(img)
        return r, t, img

    def finish(self):
        print("\ntimings (client side, incl. JSON + base64):")
        for k, v in self.timings:
            print(f"  {k:28s} {v:8.1f} ms")
        if self.failures:
            print(f"\n{len(self.failures)} FAILED:\n  " + "\n  ".join(self.failures))
            raise SystemExit(1)
        print(f"\nall checks passed")
