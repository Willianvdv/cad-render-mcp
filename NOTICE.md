# Third-party notices

shapeshot itself is dual-licensed under MIT or Apache-2.0 (at your option).

## Open CASCADE Technology (OCCT)

This software uses **Open CASCADE Technology** (https://dev.occt3d.com), licensed under the
GNU Lesser General Public License version 2.1 with the *Open CASCADE Exception* version 1.0.

- OCCT is **not** included in this repository or statically linked. shapeshot links the OCCT
  shared libraries (`libTK*.dylib`) dynamically; `cpp/occt_shim.cpp` only includes OCCT's
  public headers, which the Open CASCADE Exception covers.
- Binary distributions that bundle the OCCT dylibs must ship them as separate, replaceable
  shared libraries, include the LGPL-2.1 text and the exception, and state where the OCCT source
  is available (https://github.com/Open-Cascade-SAS/OCCT). Users may replace the bundled OCCT
  with their own build (`OCCT_DIR`, rebuild, or swap the dylibs of a compatible version).

## oneTBB

OCCT's Homebrew build links Intel oneTBB (Apache-2.0) for parallel meshing.

## Rust crates

All crates in the dependency tree are permissive (MIT, Apache-2.0, BSD, Zlib, ISC, Unicode-3.0);
check with `cargo metadata` or `cargo deny check licenses`. Notable ones: `rmcp` (official MCP
Rust SDK, Apache-2.0), `wgpu` (MIT OR Apache-2.0), `glam`, `png`, `tokio`, `serde`.

## Cycles (optional, `photo`)

Cycles (https://www.cycles-renderer.org, Apache-2.0) is run as a separate executable; it is not
linked into shapeshot. Distributions that bundle it must include its LICENSE and third-party notices
(its prebuilt dependencies such as OpenImageIO, OpenImageDenoise and Embree are permissively licensed).
