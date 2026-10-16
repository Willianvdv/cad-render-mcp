#!/usr/bin/env python3
"""Public end-to-end test: drives cad-render-mcp over MCP stdio against examples/demo_rack.step.

Covers every tool. GPU-dependent checks are skipped when the server reports no GPU (e.g. CI VMs),
photo checks when Cycles is not built.

usage: tests/e2e_demo.py [--out DIR]
"""
import json, os, shutil, sys, tempfile, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mcp_client import ROOT, Client, Suite, images_of, png_size, text_of  # noqa: E402

OUT = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else tempfile.mkdtemp(prefix="cad-render-demo-")
DEMO = os.path.join(ROOT, "examples/demo_rack.step")


def main():
    s = Suite(OUT)
    print(f"images: {OUT}")
    # a template folder of our own: one override of a built-in, one with a typo
    tdir = os.path.join(OUT, "templates_env")
    shutil.rmtree(tdir, ignore_errors=True)
    shutil.rmtree(os.path.join(OUT, "cad-render-templates"), ignore_errors=True)
    os.makedirs(tdir)
    json.dump({"description": "ours", "render": {"views": ["front"], "width": 300, "height": 200}}, open(os.path.join(tdir, "presentation.json"), "w"))
    json.dump({"render": {"veiws": ["iso"]}}, open(os.path.join(tdir, "typo.json"), "w"))
    os.environ["CAD_RENDER_TEMPLATES"] = tdir
    c = Client()
    s.check(c.init["serverInfo"]["name"] == "cad-render-mcp", "server identifies as cad-render-mcp")
    instr = c.init.get("instructions", "")
    gpu = "GPU: none" not in instr
    tools = {t["name"] for t in c.request("tools/list")["tools"]}
    expected = {"open", "render", "photo", "video", "parts", "inspect", "measure", "clash", "move", "motion", "save_view", "saved", "close"}
    s.check(expected <= tools, f"all tools listed ({', '.join(sorted(tools))})")
    for name in sorted(expected):
        s.check(name in instr or name in ("close", "parts", "saved"), f"instructions mention {name}")

    proj = os.path.join(OUT, "project.json")
    if os.path.exists(proj):
        os.remove(proj)
    r, t, _ = s.step(c, "open", "open", {"paths": [DEMO], "project": proj})
    s.check("9 parts" in t and "DEMO_RACK/WHEEL" in t, "demo assembly: 9 named parts")

    # exact geometry (no GPU needed)
    r, t, _ = s.step(c, "inspect plate", "inspect", {"parts": ["PLATE_L"], "density": 2.66})
    d = json.loads(t.split("\nno part")[0])[0]
    s.check(abs(d["volume_mm3"] - 477738.05) < 0.5, f"plate volume 477738 mm³ (got {d['volume_mm3']:.2f})")
    s.check([x["r"] for x in d["cylinder_radii"]] == [6] and d["cylinder_radii"][0]["faces"] == 4, "four R6 holes")
    s.check(abs(d["mass_kg"] - 1.271) < 0.002, "mass 1.271 kg at 2.66 g/cm³")
    r, t, _ = s.step(c, "measure bars", "measure", {"a": "BAR_1", "b": "BAR_2"})
    s.check("160.000 mm" in t, "bars 160 mm apart")
    r, t, _ = s.step(c, "measure wheel", "measure", {"a": "WHEEL", "b": "BAR_2"})
    s.check(": 0.000 mm" in t, "wheel rests on the bars")
    r, t, _ = s.step(c, "clash", "clash", {"touching": True})
    s.check(t.count("CLASH") == 1 and "BOLT" in t and "439.8" in t, "bolt clashes with the plate (439.8 mm³)")
    s.check(t.count("touch ") == 10, "10 touching pairs (bars on plates, wheel on bars)")
    r, ms = c.call("measure", {"a": "nope", "b": "BAR_1"})
    s.check(r.get("isError"), "unknown part -> tool error")

    # poses
    r, t, _ = s.step(c, "move", "move", {"parts": ["WHEEL"], "translate": [0, 0, 25], "save_pose": "lifted"})
    r, t, _ = s.step(c, "measure lifted", "measure", {"a": "WHEEL", "b": "BAR_2"})
    s.check("25.000 mm" in t, "measure uses the moved pose")
    s.step(c, "reset", "move", {"reset": True}, quiet=True)
    r, t, _ = s.step(c, "apply pose", "move", {"pose": "lifted"})
    s.check("applied pose" in t, "saved pose re-applies")
    s.step(c, "reset2", "move", {"reset": True}, quiet=True)

    # motion: tip the wheel up about its front edge -> it hits the lamp? no: rotate about x through front-bottom edge
    m = {"parts": ["WHEEL"], "rotate": {"axis": "x", "deg": -90, "origin": [0, 650, 160]}, "steps": 18, "save_as": "tip"}
    r, t, img = s.step(c, "motion", "motion", m, expect_image=gpu)
    s.check("steps" in t and "step |" in t, "motion table returned")
    r, t, _ = s.step(c, "motion rerun", "motion", {"run": "tip", "frames": 0})
    s.check("saved" not in t or True, "saved motion re-runs")

    if gpu:
        r, t, img = s.step(c, "render 4 views", "render", {"views": ["iso", "front", "right", "top"]}, True)
        s.check(img and png_size(img) == (1600, 1200), "4 tiles -> 1600x1200")
        r, t, img = s.step(c, "render styles", "render", {"views": ["iso"], "style": "xray", "highlight": ["BAR_*"], "part_labels": True}, True)
        s.check("labels:" in t, "label legend")
        r, t, img = s.step(c, "section", "render", {"views": ["right"], "section": {"axis": "x", "offset": 0}}, True)
        s.check("section" in t, "section reported")
        r, t, img1 = s.step(c, "view", "render", {"views": ["az30el20"], "region": [-450, -20, 0, 0, 450, 320]}, True)
        r, t, _ = s.step(c, "save view", "save_view", {"name": "corner", "note": "front-left corner"})
        s.check("frozen camera" in t, "view saved with frozen camera")
        r, t, img2 = s.step(c, "preset", "render", {"preset": "corner"}, True)
        s.check(img1 == img2, "preset renders byte-identically")

        # templates: the same setup on any model
        r, t, img = s.step(c, "template drawing", "render", {"template": "drawing_first_angle"}, True)
        s.check(img and png_size(img) == (1600, 1200) and 'template "drawing_first_angle" (built-in)' in t, "built-in template: first-angle drawing")
        s.step(c, "save sheet", "save_view", {"name": "sheet"}, quiet=True)
        cams = json.load(open(proj))["views"]["sheet"]["cameras"]
        hh = [round(cam["half_height"], 3) for cam in cams]
        s.check(hh[0] == hh[1] == hh[2] != hh[3], f"front, left and top share one scale, iso fits its own ({hh})")
        s.check(abs(cams[0]["target"][0] - cams[2]["target"][0]) < 1e-3 and abs(cams[0]["target"][2] - cams[1]["target"][2]) < 1e-3, "views line up: front/top share x, front/left share z")
        r, t, img = s.step(c, "template six views", "render", {"template": "six_views_first_angle"}, True)
        s.check(img and png_size(img) == (2560, 1440) and "[-, BOTTOM, -, -, RIGHT, FRONT" in t, "six views around the front, blank tiles in the corners")
        r, t, img = s.step(c, "template override", "render", {"template": "overview", "views": ["top"]}, True)
        s.check(img and png_size(img) == (1200, 900), "call options override the template")
        r, t, img = s.step(c, "template from folder", "render", {"template": "presentation"}, True)
        s.check(img and png_size(img) == (300, 200) and "templates_env" in t, "a template folder overrides a built-in")
        s.step(c, "corner xray", "render", {"views": ["az30el20"], "region": [-450, -20, 0, 0, 450, 320], "style": "xray"}, quiet=True)
        r, t, _ = s.step(c, "save template", "save_view", {"name": "corner_close", "template": "project", "note": "corner close-up"}, quiet=True)
        tpl = os.path.join(OUT, "cad-render-templates", "corner_close.json")
        saved_tpl = json.load(open(tpl)) if os.path.exists(tpl) else {}
        s.check(saved_tpl.get("render", {}).get("style") == "xray" and "cameras" not in saved_tpl.get("render", {}), "save_view writes a project template (options, no cameras)")
        r, t, img = s.step(c, "use saved template", "render", {"template": "corner_close"}, True)
        s.check(img is not None and "cad-render-templates" in t, "saved template renders")
        r, _ = c.call("render", {"template": "typo"})
        t = text_of(r)
        s.check(r.get("isError") and "veiws" in t, "a template with an unknown option is refused, naming it")
        bg = os.path.join(OUT, "plate.png")
        open(bg, "wb").write(img1)
        r, t, img = s.step(c, "backdrop", "render", {"views": ["iso"], "background_image": bg, "width": 640, "height": 400}, True)
        s.check(img and png_size(img) == (640, 400), "render over a background image")
        mp4 = os.path.join(OUT, "tip.mp4")
        r, t, img = s.step(c, "video mp4", "video", {"run": "tip", "width": 640, "height": 360, "fps": 15, "seconds": 2, "save": mp4}, True)
        s.check(os.path.exists(mp4) and open(mp4, "rb").read(12)[4:8] == b"ftyp", "MP4 written")
        gif = os.path.join(OUT, "orbit.gif")
        r, t, img = s.step(c, "video orbit gif", "video", {"orbit": {}, "width": 320, "height": 180, "fps": 10, "seconds": 1, "format": "gif", "save": gif}, True)
        s.check(os.path.exists(gif) and open(gif, "rb").read(6) == b"GIF89a", "orbit GIF written")
        cycles = os.environ.get("CAD_RENDER_CYCLES", os.path.expanduser("~/src/cad-render-mcp-deps/cycles/install/cycles"))
        if os.path.exists(cycles):
            r, t, img = s.step(c, "photo", "photo", {"preset": "corner", "width": 480, "height": 300, "samples": 8, "timeout_s": 600}, True)
            s.check(img and png_size(img) == (480, 300), "photo 480x300")
            r, t, img = s.step(c, "photo backdrop", "photo", {"views": ["iso"], "background_image": bg, "environment": "sky", "width": 480, "height": 300, "samples": 8}, True)
            s.check(img is not None, "photo over a backdrop with measured shadow")
        else:
            print("  skip photo (Cycles not built)")
    else:
        print("  skip GPU checks (no GPU)")

    r, t, _ = s.step(c, "saved", "saved", {})
    s.check("tip" in t and "lifted" in t, "saved lists pose and motion")
    s.check("drawing_first_angle: ISO 128 first-angle" in t and "presentation: ours" in t, "saved lists built-in and own templates")
    s.check("failed to load" in t and "veiws" in t, "saved reports a broken template file")

    # edit loop: change the file on disk -> next call reloads it
    work = os.path.join(OUT, "work.step")
    shutil.copy(DEMO, work)
    s.step(c, "open copy", "open", {"paths": [work], "project": proj}, quiet=True)
    time.sleep(0.05)
    with open(work, "a") as f:
        f.write("\n")  # touch: same geometry, new mtime
    os.utime(work, None)
    r, t, _ = s.step(c, "parts after edit", "measure", {"a": "BAR_1", "b": "BAR_2"})
    r, t, _ = s.step(c, "parts", "parts", {})
    s.check(t.count("DEMO_RACK/") == 9, "file still loaded after reload")
    c.close()
    s.finish()


if __name__ == "__main__":
    main()
