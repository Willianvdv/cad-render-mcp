# shapeshot

A headless, GPU-accelerated CAD viewer that speaks only **MCP**. It lets Claude (or any MCP client)
open STEP / BREP / IGES files, look at them from any angle and ask exact geometry questions. It
answers in milliseconds instead of driving a desktop CAD program.

- **Rust**: one static binary, stdio MCP server (`rmcp`, the official SDK)
- **OpenCascade 7.9** reads the files and does exact B-rep queries, linked dynamically (LGPL-safe)
- **wgpu on Metal**: offscreen 4× MSAA rendering on Apple Silicon. Meshes stay resident on the
  GPU, and every view of a contact sheet is drawn in one pass with one readback.
- Images come back **inline** in the tool result, so there is no write-to-disk + read round trip

## Why

It was built to replace Autodesk Fusion in an agent design loop (a roof-rack project, 4 days, 421
Fusion MCP calls). Measured from that session:

| | Fusion (MCP add-in) | shapeshot |
|---|---|---|
| open the 18-part assembly | document open: median 5.4 s, some hit the 60 s MCP timeout | **0.54 s** (read 0.4 s + mesh 0.15 s) |
| screenshot | median 2.7 s, 10 time-outs; written to disk, then read back | **6 ms** warm (1200×900) |
| 4-view contact sheet | 4 calls + 4 file reads | **14 ms**, one call, one image |
| clash check (all parts) | TemporaryBRep booleans, ~15 s per check | **0.4–1.3 s** |
| min distance | `measureMinimumDistance` scripts | **~150 ms** |
| bend radii / volume / mass | face-loop scripts | **~20 ms** (`inspect`) |
| reload after a model change | re-run stage scripts in Fusion | automatic on next call (file mtime) |
| failure modes | 37 errors + 26 time-outs; modal dialogs block the API; app must be running | none seen in tests |

Numbers are from an Apple M4. Reproduce them with `cargo run --release -- bench <file.step>` and
`tests/mcp_e2e.py`.

The renderer replaces Fusion's *viewing* side. For the *modelling* side, use geometry-as-code that
writes STEP/BREP, e.g. [build123d](https://github.com/gumyr/build123d) or CadQuery
(Apache-2.0, also on OCCT). Then the loop becomes: edit script → export → `render`, and shapeshot
picks up the changed file by itself.

## Install

```sh
brew install rust opencascade          # OCCT 7.9 (LGPL-2.1, dynamic libs)
cargo build --release
claude mcp add shapeshot -- "$PWD/target/release/shapeshot"
```

`OCCT_DIR` overrides the OCCT location (default: `brew --prefix opencascade`).

## Tools

| tool | what it does |
|---|---|
| `open` | load files or directories (`split_solids` gives one part per body); returns part ids, sizes, timings |
| `render` | PNG inline. `views` tile into one image: `iso`, `iso_fl/br/bl`, `iso_below`, `front/back/left/right/top/bottom`, `az30el20`, `dir:x,y,z`. Options: `show`, `hide`, `highlight` (others ghosted), `ghost`, `focus`, `zoom`, `colors`, `color_by` (`file`/`part`/`uniform`), `style` (`shaded`/`xray`/`lines`), `edges` (`all`/`sharp`/`none`), `silhouettes`, `section {axis/normal, offset, remove: auto|positive|negative}`, `region` (frame an mm box), `perspective`, `up` (`z`/`y`), `background`, `labels`, `part_labels`, `scale_bar`, `save` |
| `parts` | list parts (id, assembly path, file, bbox size, triangles) |
| `inspect` | exact volume, area, mass (`density`), centroid, bbox, topology counts, face types, cylinder radii (bend radii, holes), torus radii |
| `measure` | exact minimum distance between two parts, with the closest points |
| `clash` | boolean-common interference volume for every pair whose boxes overlap; optionally touching pairs |
| `move` | rigid translate/rotate of parts (`rotate {axis, deg, origin}`), `reset`, apply or `save_pose` a named pose. render/measure/clash use the moved positions |
| `motion` | sweep parts through a translate/rotate in N steps: exact clearance and interference per step, first contact, contact sheet of frames (fixed camera). `save_as` / `run` named motions |
| `save_view` | store the last render (all options + the exact cameras) as a named view; `render {preset}` reproduces it pixel-for-pixel in any session, even after the model changes |
| `saved` | list (or delete) the project's saved views, poses and motions |
| `close` | unload everything |

Saved views, poses and motions live in `.shapeshot.json` next to the first opened file (override
with `open {project}`), so they can be committed with the model.

Interference uses OCCT booleans with a small fuzzy tolerance and a retry ladder; if a boolean still
returns nothing while the exact distance is 0, point classification (mesh nodes plus edge samples)
decides, and the clash is reported with "volume unknown".

Part selectors accept an id (`p12`), an exact name or path (`PLATE_L:1`), a glob (`BAR_*`) or a
substring. Units are mm. Named views assume Z-up, with `front` looking along +Y.

## How it works

```
STEP/IGES ─▶ OCCT XCAF reader (names, colours, assembly tree)
          ─▶ BRepMesh (parallel, deflection relative to model size)
          ─▶ per part: triangles + normals, B-rep edge polylines (seams dropped, tangent edges
             flagged), silhouette candidates (curved-face triangle pairs)
          ─▶ GPU buffers (kept resident)

render ─▶ one MSAA render pass; per tile: viewport + camera uniform
          faces (lit, depth-biased) → feature edges (instanced screen-space quads)
          → silhouettes (decided per view in the vertex shader) → ghosts (blended)
          section: clip in the fragment shader, back faces drawn as the cut cap
       ─▶ resolve → one buffer copy → labels / scale bar on CPU → PNG (fast deflate) → base64
```

stdout is the MCP channel. At startup the process moves it to a private fd and points fd 1 at
stderr, so stray prints from native libraries can't corrupt the protocol.

## Licence

Dual-licensed MIT OR Apache-2.0. OCCT is used as an external LGPL-2.1 (+ Open CASCADE
Exception) shared library; see [NOTICE.md](NOTICE.md) for what binary distributions must include.

## Roadmap

- on-disk cache of parsed shapes (OCCT binary BRep) for instant re-open of big unchanged STEP files
- `section_profile`: 2D outline + dimensions of a cut (sheet-metal flat checks)
- face/edge picking: `render` returns ids under labelled points; per-face highlight
- per-face colours from XCAF; STL/OBJ/glTF input; Linux/Vulkan CI (wgpu backends are already there)
