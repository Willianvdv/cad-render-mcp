#!/usr/bin/env python3
"""Drive a cad-render-mcp MCP server with a script of tool calls; save returned images.

usage: tests/drive.py calls.json OUT_DIR      (calls.json: [{"tool": "...", "args": {...}, "name": "..."}])
"""
import base64, json, os, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mcp_client import Client, images_of, text_of  # noqa: E402

calls = json.load(open(sys.argv[1]))
out = sys.argv[2]
os.makedirs(out, exist_ok=True)
c = Client()
for k, call in enumerate(calls):
    t = time.perf_counter()
    r = c.request("tools/call", {"name": call["tool"], "arguments": call.get("args", {})})
    ms = (time.perf_counter() - t) * 1e3
    name = call.get("name", f"{k:02d}_{call['tool']}")
    print(f"--- {name} ({call['tool']}, {ms:.0f} ms){' ERROR' if r.get('isError') else ''}")
    print(text_of(r)[:3000])
    for c_ in r["content"]:
        if c_["type"] == "image":
            p = os.path.join(out, name + ".png")
            open(p, "wb").write(base64.b64decode(c_["data"]))
            print(f"image -> {p}")
c.close()
