#!/usr/bin/env python3
"""Private end-to-end test against the original roof-rack STEP files (testdata/, not published).
Skips when the fixtures are absent; the public suite is tests/e2e_demo.py.

Exercises the tools on a real 18-part / 265-body assembly (open, multi-view renders,
section, highlight, inspect bend radii, measure clearance, clash check, reload after a file
change) and checks every response.

usage: tests/e2e_roofrack.py [path/to/cad-render-mcp] [--out DIR]
"""
import base64, json, os, shutil, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = next((a for a in sys.argv[1:] if not a.startswith("--") and os.path.isfile(a)), os.path.join(ROOT, "target/release/cad-render-mcp"))
OUT = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else tempfile.mkdtemp(prefix="cad-render-e2e-")
os.makedirs(OUT, exist_ok=True)
ASM = os.path.join(ROOT, "testdata/roofrack_v26a.step")
PLATE = os.path.join(ROOT, "testdata/v26a_side_plate_L_5083_5mm.step")


class Client:
    def __init__(self, cmd):
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.id = 0

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
            msg = json.loads(line)  # any non-JSON on stdout fails here: protocol stream must stay clean
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


failures = []
timings = []


def check(cond, what):
    print(("  ok   " if cond else "  FAIL ") + what)
    if not cond:
        failures.append(what)


def text_of(r):
    return "\n".join(c["text"] for c in r["content"] if c["type"] == "text")


def image_of(r, name):
    imgs = [c for c in r["content"] if c["type"] == "image"]
    if not imgs:
        return None
    data = base64.b64decode(imgs[0]["data"])
    path = os.path.join(OUT, name)
    open(path, "wb").write(data)
    return data, path


def png_size(data):
    return int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")


def step(c, label, tool, args, expect_image=False):
    r, ms = c.call(tool, args)
    timings.append((label, ms))
    t = text_of(r)
    print(f"\n[{label}] {tool} {ms:.0f} ms\n  " + t.replace("\n", "\n  ")[:900])
    check(not r.get("isError"), f"{label}: no tool error")
    img = None
    if expect_image:
        img = image_of(r, label.replace(" ", "_") + ".png")
        check(img is not None and img[0][:8] == b"\x89PNG\r\n\x1a\n", f"{label}: inline PNG returned")
    return r, t, img


def main():
    if not os.path.exists(ASM):
        print(f"skip: {ASM} not present (private fixture)")
        return
    print(f"binary: {BIN}\nimages: {OUT}")
    t0 = time.perf_counter()
    c = Client([BIN])
    init = c.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}})
    c.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    print(f"initialize {(time.perf_counter() - t0) * 1e3:.0f} ms: {init['serverInfo']}")
    check("cad-render" in init["serverInfo"]["name"], "server identifies as cad-render-mcp")
    check("instructions" in init and "render" in init["instructions"], "server sends usage instructions")
    tools = {t["name"]: t for t in c.request("tools/list")["tools"]}
    print("tools:", ", ".join(sorted(tools)))
    check({"open", "render", "parts", "inspect", "measure", "clash", "close"} <= set(tools), "all tools listed")
    check("views" in json.dumps(tools["render"]["inputSchema"]), "render schema documents views")

    # 1. open the full assembly
    r, t, _ = step(c, "open assembly", "open", {"paths": [ASM]})
    check("18 parts" in t, "assembly has 18 parts")

    # 2. multi-view check
    r, t, img = step(c, "four views", "render", {"views": ["iso", "front", "right", "top"]}, True)
    check(img and png_size(img[0]) == (1600, 1200), "4 tiles of 800x600 -> 1600x1200")

    # 3. same render again (warm)
    r, t, img = step(c, "four views warm", "render", {"views": ["iso", "front", "right", "top"]}, True)

    # 4. highlight the bars, ghost the rest, with part labels
    r, t, img = step(c, "highlight bars", "render", {"views": ["iso"], "highlight": ["BAR_*"], "part_labels": True}, True)
    check("labels:" in t, "label legend returned")

    # 5. section through the side plate, looking at the cut
    r, t, img = step(c, "section x", "render", {"views": ["iso", "right"], "section": {"axis": "x", "offset": -950}, "focus": ["PLATE_L*"]}, True)
    check("section" in t, "section reported")

    # 6. close-up of the front plate, x-ray, perspective
    r, t, img = step(c, "front xray", "render", {"views": ["iso_bl"], "style": "xray", "focus": ["FRONT_PLATE*"], "perspective": True, "width": 900}, True)

    # 7. line drawing (for documents / measure sheets)
    r, t, img = step(c, "lines top", "render", {"views": ["top"], "style": "lines", "edges": "sharp", "save": os.path.join(OUT, "saved_lines_top.png")}, True)
    check(os.path.exists(os.path.join(OUT, "saved_lines_top.png")), "save path written")

    # 8. inspect: bend radii, volume, mass
    r, t, _ = step(c, "inspect plate", "inspect", {"parts": ["PLATE_L:1"], "density": 2.66})
    data = json.loads(t.split("\nno part")[0])
    check(abs(data[0]["volume_mm3"] - 2.43e6) / 2.43e6 < 0.02, f"PLATE_L volume ~2.43e6 mm³ (got {data[0]['volume_mm3']:.0f})")
    check(any(abs(x["r"] - 5.0) < 1e-6 for x in data[0]["cylinder_radii"]), "R5 bend radius found")
    check(6.0 < data[0]["mass_kg"] < 7.0, f"mass {data[0]['mass_kg']} kg in 6..7")

    # 9. measure
    r, t, _ = step(c, "measure bars", "measure", {"a": "BAR_ISB_40x80 (5):1", "b": "BAR_ISB_40x80 (5):2"})
    check("min distance" in t, "distance reported")

    # 10. clash
    r, t, _ = step(c, "clash all", "clash", {})
    check("pairs" in t, "clash summary")

    # 11. error path: unknown part / bad view
    r, ms = c.call("render", {"views": ["sideways"]})
    check(r.get("isError") and "unknown view" in text_of(r), "bad view -> tool error with guidance")
    r, ms = c.call("measure", {"a": "nope", "b": "PLATE_L:1"})
    check(r.get("isError"), "unknown part -> tool error")

    # 12. edit loop: file changes on disk -> next call reloads it
    work = os.path.join(OUT, "work.step")
    shutil.copy(PLATE, work)
    r, t, _ = step(c, "open plate", "open", {"paths": [work]})
    time.sleep(0.05)
    shutil.copy(ASM, work)  # "re-export" with different content
    os.utime(work, None)
    r, t, img = step(c, "render after change", "render", {"views": ["iso"]}, True)
    check("reloaded" in t, "changed file auto-reloaded")
    r, t, _ = step(c, "parts after reload", "parts", {})
    check(t.count("\n") >= 18, "reloaded file shows new parts")

    # 13. directory open: all v26a STEP files at once
    r, t, _ = step(c, "open dir", "open", {"paths": [os.path.join(ROOT, "testdata")]})
    r, t, img = step(c, "dir render", "render", {"views": ["iso"], "color_by": "part"}, True)

    # 14. per-body split: 265 solids, then clash bolts/nuts against the bars only
    r, t, _ = step(c, "open split", "open", {"paths": [ASM], "split_solids": True})
    check("265 parts" in t, "split into 265 solids")
    r, t, _ = step(c, "clash split", "clash", {"parts": ["BAR_ISB*", "FEET_HW*"]})
    r, t, img = step(c, "zoom hardware", "render", {"views": ["iso", "front"], "focus": ["FEET_HW:1#1"], "zoom": 0.5, "highlight": ["FEET_HW*"]}, True)

    # 15. saved viewpoint + poses + motions (project file in the output dir)
    proj = os.path.join(OUT, "project.json")
    if os.path.exists(proj):
        os.remove(proj)
    r, t, _ = step(c, "open project", "open", {"paths": [ASM], "split_solids": True, "project": proj})
    check("views: -" in t, "fresh project is empty")
    r, t, img1 = step(c, "lamps view", "render", {"views": ["right"], "region": [-1110, 2150, 20, -600, 2480, 500]}, True)
    r, t, _ = step(c, "save view", "save_view", {"name": "lamps", "note": "lamp mounts"})
    check("frozen camera" in t and os.path.exists(proj), "view saved with frozen camera")
    r, t, _ = step(c, "move front up", "move", {"parts": ["FRONT_*", "PLACEHOLDERS:1#2"], "translate": [0, 0, 40], "save_pose": "up40"})
    check("offset [0.0, 0.0, 40.0]" in t, "move reports offsets")
    r, t, img2 = step(c, "preset after move", "render", {"preset": "lamps"}, True)
    check("frozen camera" in t and png_size(img1[0]) == png_size(img2[0]) and img1[0] != img2[0], "preset reproduces the camera; moved parts change the image")
    r, t, _ = step(c, "measure moved", "measure", {"a": "FRONT_PLATE:1", "b": "BAR_ISB_40x80 (5):8"})
    check("4.000 mm" not in t, "measure uses the moved pose")
    r, t, _ = step(c, "reset", "move", {"reset": True})
    r, t, _ = step(c, "measure reset", "measure", {"a": "FRONT_PLATE:1", "b": "BAR_ISB_40x80 (5):8"})
    check("4.000 mm" in t, "reset restores the file position")
    r, t, img = step(c, "motion tip", "motion", {"parts": ["PLACEHOLDERS:1#1"], "rotate": {"axis": "x", "deg": -85, "origin": [0, 2018.8, 189.4]}, "steps": 17, "render": {"views": ["right"]}, "save_as": "tip"}, True)
    check("FIRST INTERFERENCE at step 15" in t and "FRONT_PLATE" in t, "wheel tipping up hits the front plate at step 15")
    r, t, _ = step(c, "motion rerun", "motion", {"run": "tip", "frames": 0})
    check("FIRST INTERFERENCE at step 15" in t, "saved motion re-runs identically")
    r, t, _ = step(c, "saved list", "saved", {})
    check("lamps" in t and "up40" in t and "tip" in t, "saved lists view, pose and motion")
    r, t, _ = step(c, "clash tilted", "move", {"parts": ["PLACEHOLDERS:1#1"], "rotate": {"axis": "x", "deg": 10, "origin": [0, 2018.8, 189.4]}})
    r, t, _ = step(c, "clash at 10deg", "clash", {"parts": ["PLACEHOLDERS:1#1", "BAR_ISB*"]})
    check(t.count("CLASH") == 5, "all 5 bars under the tilted wheel reported (incl. failed-boolean fallback)")
    # 16. video: saved motion -> MP4 (hardware H.264) and GIF, fast renderer, clearance burned in
    c.call("move", {"reset": True})  # the clash test above left the wheel tilted into the bars
    mp4 = os.path.join(OUT, "tip.mp4")
    r, t, img = step(c, "video mp4", "video", {"run": "tip", "render": {"views": ["right"]}, "width": 640, "height": 360, "fps": 15, "seconds": 2, "save": mp4}, True)
    check(os.path.exists(mp4) and open(mp4, "rb").read(12)[4:8] == b"ftyp", "MP4 written (ftyp box)")
    check("FIRST INTERFERENCE" in t and "FRONT_PLATE" in t, "video reports the first interference")
    gif = os.path.join(OUT, "tip.gif")
    r, t, img = step(c, "video gif", "video", {"run": "tip", "format": "gif", "width": 320, "height": 180, "fps": 10, "seconds": 1, "annotate": False, "save": gif}, True)
    check(os.path.exists(gif) and open(gif, "rb").read(6) == b"GIF89a", "GIF written")

    # 17. photo (Cycles) when available
    cycles = os.environ.get("CAD_RENDER_CYCLES", os.path.expanduser("~/src/cad-render-mcp-deps/cycles/install/cycles"))
    if os.path.exists(cycles):
        r, t, img = step(c, "photo", "photo", {"preset": "lamps", "width": 640, "height": 400, "samples": 16, "timeout_s": 600}, True)
        check(img is not None and png_size(img[0]) == (640, 400), "photo returns a 640x400 path-traced PNG")
    else:
        print("  skip photo (Cycles not built)")
    c2 = Client([BIN])
    c2.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e2", "version": "0"}})
    c2.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    c2.call("open", {"paths": [ASM], "split_solids": True, "project": proj})
    r, _ = c2.call("render", {"preset": "lamps"})
    img3 = base64.b64decode([x for x in r["content"] if x["type"] == "image"][0]["data"])
    check(img3 == img1[0], "new session: preset renders a byte-identical image")
    c2.close()

    c.close()
    print("\nstdout stayed valid JSON-RPC throughout (every line parsed).")
    print("\ntimings (client side, incl. JSON + base64):")
    for k, v in timings:
        print(f"  {k:24s} {v:7.1f} ms")
    if failures:
        print(f"\n{len(failures)} FAILED:\n  " + "\n  ".join(failures))
        sys.exit(1)
    print("\nall checks passed")


if __name__ == "__main__":
    main()
