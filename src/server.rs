//! MCP tools: open, render, parts, inspect, measure, clash, close.

use crate::camera::{self, Up};
use crate::render::{Edges, PartStyle, RenderReq, Renderer};
use crate::scene::{self, Scene};
use base64::Engine;
use glam::Vec3;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct App {
    pub renderer: Option<Renderer>,
    pub renderer_err: Option<String>,
    pub scene: Scene,
}

impl App {
    pub fn new() -> App {
        let (renderer, renderer_err) = match Renderer::new() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(format!("GPU init failed: {e:#}"))),
        };
        App { renderer, renderer_err, scene: Scene::new() }
    }

    /// Reload changed files; returns a note for the response.
    fn refresh(&mut self) -> anyhow::Result<Option<String>> {
        let Some(r) = self.renderer.as_ref() else { return Ok(None) };
        let changed = self.scene.refresh(r)?;
        Ok((!changed.is_empty()).then(|| format!("reloaded (file changed): {}", changed.join(", "))))
    }
}

#[derive(Clone)]
pub struct Shapeshot {
    app: Arc<Mutex<App>>,
    tool_router: ToolRouter<Self>,
}

// ---------- parameters ----------

#[derive(Deserialize, JsonSchema)]
pub struct OpenParams {
    /// STEP (.step/.stp), BREP (.brep) or IGES (.igs/.iges) files, or directories containing them.
    pub paths: Vec<String>,
    /// Keep already loaded files and add these (default false: replace the scene).
    pub add: Option<bool>,
    /// Max chordal deviation of the tessellation in mm (default: 0.04% of the model diagonal).
    pub deflection_mm: Option<f64>,
    /// Max angular deviation of the tessellation in degrees (default 17).
    pub angle_deg: Option<f64>,
    /// Split components that hold several solids into one part per solid ("name#k"), so
    /// render/clash/inspect work per body (default false: one part per component).
    pub split_solids: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SectionSpec {
    /// Cut plane normal axis: "x", "y" or "z" (or use `normal`).
    pub axis: Option<String>,
    /// Plane normal [x, y, z] in model coordinates.
    pub normal: Option<[f32; 3]>,
    /// Plane position along the normal in mm (default: centre of the visible parts).
    pub offset: Option<f32>,
    /// Which half to remove: "auto" (default: the half facing each view's camera, so the cut is
    /// always visible), "positive" (the +normal side) or "negative".
    pub remove: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RenderParams {
    /// Views, tiled row-major into one image. Named: iso, iso_fl, iso_br, iso_bl, iso_below, front,
    /// back, left, right, top, bottom (Z-up, front = looking along +Y); or "az<deg>el<deg>"
    /// (azimuth from front towards +X, elevation); or "dir:x,y,z" (camera -> scene). Default ["iso"].
    pub views: Option<Vec<String>>,
    /// Tile width in px (default 1200 for one view, 800 for 2-4, 640 for more).
    pub width: Option<u32>,
    /// Tile height in px (default 3/4 of width).
    pub height: Option<u32>,
    /// Tiles per row (default: 2 for 2-4 views, 3 for more).
    pub cols: Option<u32>,
    /// Only show these parts (selectors: id like "p3", exact name/path, glob like "BAR_*", or substring).
    pub show: Option<Vec<String>>,
    /// Hide these parts.
    pub hide: Option<Vec<String>>,
    /// Draw these parts normally and every other visible part as a faint ghost.
    pub highlight: Option<Vec<String>>,
    /// Draw these parts as ghosts.
    pub ghost: Option<Vec<String>>,
    /// Frame the camera on these parts (others stay visible).
    pub focus: Option<Vec<String>>,
    /// Override colours: selector -> "#rrggbb".
    pub colors: Option<BTreeMap<String, String>>,
    /// "file" (colours from the CAD file, default), "part" (distinct colour per part), "uniform".
    pub color_by: Option<String>,
    /// "shaded" (default), "xray" (all parts translucent), "lines" (white faces, dark edges).
    pub style: Option<String>,
    /// B-rep edges: "all" (default), "sharp" (hide tangent edges), "none".
    pub edges: Option<String>,
    /// Edge width in px (default 1.6).
    pub edge_width: Option<f32>,
    /// Draw silhouettes of curved faces (default true).
    pub silhouettes: Option<bool>,
    /// Section cut.
    pub section: Option<SectionSpec>,
    /// Perspective projection (default false = orthographic, measurable with the scale bar).
    pub perspective: Option<bool>,
    /// Frame this box instead: [xmin, ymin, zmin, xmax, ymax, zmax] in mm (for close-ups of one spot).
    pub region: Option<[f32; 6]>,
    /// Zoom factor (>1 zooms in, default 1).
    pub zoom: Option<f32>,
    /// Model up axis: "z" (default) or "y".
    pub up: Option<String>,
    /// "#rrggbb" or "transparent" (default "#f4f5f7").
    pub background: Option<String>,
    /// View name label in each tile (default true).
    pub labels: Option<bool>,
    /// Part id/name labels at part centres (default false).
    pub part_labels: Option<bool>,
    /// Scale bar in orthographic views (default true).
    pub scale_bar: Option<bool>,
    /// Also write the PNG to this path.
    pub save: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PartsParams {
    /// Optional selector filter (glob/substring/id).
    pub filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct InspectParams {
    /// Part selectors.
    pub parts: Vec<String>,
    /// Density in g/cm³ to report mass (e.g. 2.66 for 5754 aluminium, 7.85 steel).
    pub density: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MeasureParams {
    /// First part selector (must match exactly one part).
    pub a: String,
    /// Second part selector (must match exactly one part).
    pub b: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ClashParams {
    /// Parts to check against each other (default: all).
    pub parts: Option<Vec<String>>,
    /// Report overlaps above this volume in mm³ (default 0.5).
    pub min_volume: Option<f64>,
    /// Also list pairs that touch (distance < 0.01 mm) without overlapping (default false).
    pub touching: Option<bool>,
}

// ---------- helpers ----------

fn parse_hex(s: &str) -> anyhow::Result<[f32; 3]> {
    let h = s.trim().trim_start_matches('#');
    anyhow::ensure!(h.len() == 6, "colour must be #rrggbb, got {s:?}");
    let v = u32::from_str_radix(h, 16)?;
    Ok([((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0])
}

const PALETTE: &[[f32; 3]] = &[
    [0.27, 0.51, 0.71], [0.89, 0.47, 0.20], [0.35, 0.63, 0.35], [0.80, 0.30, 0.33], [0.55, 0.45, 0.70],
    [0.60, 0.45, 0.35], [0.85, 0.55, 0.75], [0.50, 0.50, 0.50], [0.75, 0.73, 0.30], [0.25, 0.70, 0.75],
];
const DEFAULT_PART: [f32; 3] = [0.74, 0.77, 0.81];

/// File colours often come out near-black (unset appearance); lift them so shading stays readable.
fn usable(c: [f32; 3]) -> [f32; 3] {
    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    if l < 0.18 { [c[0] * 0.5 + 0.3, c[1] * 0.5 + 0.3, c[2] * 0.5 + 0.32] } else { c }
}

fn tool_err(e: anyhow::Error) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(format!("error: {e:#}"))]))
}

fn dims(lo: Vec3, hi: Vec3) -> String {
    let d = hi - lo;
    format!("{:.1} x {:.1} x {:.1}", d.x, d.y, d.z)
}

impl Shapeshot {
    pub fn new(app: App) -> Self {
        Self { app: Arc::new(Mutex::new(app)), tool_router: Self::tool_router() }
    }

    /// Run `f` on the blocking pool with the app locked (OCCT and GPU work are synchronous).
    async fn with_app<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&mut App) -> anyhow::Result<CallToolResult> + Send + 'static,
    {
        let app = self.app.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut g = app.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut g)
        })
        .await;
        match res {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => tool_err(e),
            Err(e) => tool_err(anyhow::anyhow!("internal error: {e}")),
        }
    }
}

pub fn parts_table(sc: &Scene, set: &[usize]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "id | name | file | size mm (x y z) | tris");
    for &i in set {
        let p = &sc.parts[i];
        let size = if p.empty { "(no faces)".to_string() } else { dims(p.bmin, p.bmax) };
        let _ = writeln!(s, "{} | {} | {} | {} | {}", p.id, p.path, sc.file_label(p.file), size, p.gpu.tris);
    }
    s
}

pub fn do_open(app: &mut App, p: &OpenParams) -> anyhow::Result<String> {
    let t0 = Instant::now();
    let files = scene::expand_paths(&p.paths)?;
    if !p.add.unwrap_or(false) {
        app.scene.clear();
    }
    if let Some(d) = p.deflection_mm {
        app.scene.lin_defl = d;
    }
    if let Some(s) = p.split_solids {
        app.scene.split_solids = s;
    }
    if let Some(a) = p.angle_deg {
        app.scene.ang_defl = a.to_radians();
    }
    let gpu = app.renderer.as_ref().ok_or_else(|| anyhow::anyhow!(app.renderer_err.clone().unwrap_or_default()))?;
    let reps = app.scene.load(&files, gpu)?;
    let mut s = String::new();
    for r in &reps {
        let _ = writeln!(
            s,
            "{}: {} parts, {} tris (read {:.0} ms, mesh {:.0} ms, upload {:.0} ms)",
            r.file, r.parts, r.tris, r.load_ms, r.mesh_ms, r.upload_ms
        );
    }
    let all: Vec<usize> = (0..app.scene.parts.len()).collect();
    if let Some((lo, hi)) = app.scene.bounds(&all) {
        let _ = writeln!(s, "scene: {} parts, bbox [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}] ({} mm)", all.len(), lo.x, lo.y, lo.z, hi.x, hi.y, hi.z, dims(lo, hi));
    }
    let _ = writeln!(s, "total {:.0} ms\n", t0.elapsed().as_secs_f64() * 1e3);
    s += &parts_table(&app.scene, &all);
    Ok(s)
}

pub struct Rendered {
    pub png: Vec<u8>,
    pub text: String,
}

pub fn do_render(app: &mut App, p: &RenderParams) -> anyhow::Result<Rendered> {
    let t0 = Instant::now();
    let reload = app.refresh()?;
    anyhow::ensure!(!app.scene.parts.is_empty(), "nothing loaded: call open first");
    let mut notes: Vec<String> = reload.into_iter().collect();
    let sc = &app.scene;
    let up = Up::parse(p.up.as_deref())?;
    let view_specs = p.views.clone().unwrap_or_else(|| vec!["iso".into()]);
    anyhow::ensure!(!view_specs.is_empty() && view_specs.len() <= 16, "1..16 views");
    let views = view_specs.iter().map(|v| camera::parse_view(v, up)).collect::<anyhow::Result<Vec<_>>>()?;
    let n = views.len() as u32;
    let def_w = match n {
        1 => 1200,
        2..=4 => 800,
        _ => 640,
    };
    let tile_w = p.width.unwrap_or(def_w).clamp(64, 4096);
    let tile_h = p.height.unwrap_or(tile_w * 3 / 4).clamp(64, 4096);
    let cols = p.cols.unwrap_or(match n {
        1 => 1,
        2..=4 => 2,
        _ => 3,
    });

    let mut sel = |list: &Option<Vec<String>>| -> Option<Vec<usize>> {
        list.as_ref().map(|l| {
            let (ix, missing) = sc.select(l);
            if !missing.is_empty() {
                notes.push(format!("no part matched: {}", missing.join(", ")));
            }
            ix
        })
    };
    let show = sel(&p.show);
    let hide = sel(&p.hide).unwrap_or_default();
    let highlight = sel(&p.highlight);
    let ghost = sel(&p.ghost).unwrap_or_default();
    let focus = sel(&p.focus).unwrap_or_default();
    let mut color_over: Vec<(Vec<usize>, [f32; 3])> = Vec::new();
    if let Some(c) = &p.colors {
        for (k, v) in c {
            let (ix, missing) = sc.select(std::slice::from_ref(k));
            if !missing.is_empty() {
                notes.push(format!("no part matched colour selector {k:?}"));
            }
            color_over.push((ix, parse_hex(v)?));
        }
    }
    let style = p.style.as_deref().unwrap_or("shaded");
    anyhow::ensure!(["shaded", "xray", "lines"].contains(&style), "style must be shaded, xray or lines");
    let color_by = p.color_by.as_deref().unwrap_or("file");
    let styles: Vec<PartStyle> = sc
        .parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let visible = !part.empty && show.as_ref().is_none_or(|s| s.contains(&i)) && !hide.contains(&i);
            let mut color = match color_by {
                "part" => PALETTE[i % PALETTE.len()],
                "uniform" => DEFAULT_PART,
                _ => part.file_color.map(usable).unwrap_or(DEFAULT_PART),
            };
            for (ix, c) in &color_over {
                if ix.contains(&i) {
                    color = *c;
                }
            }
            if style == "lines" {
                color = [1.0, 1.0, 1.0];
            }
            let ghosted = style == "xray" || ghost.contains(&i) || highlight.as_ref().is_some_and(|h| !h.contains(&i));
            let edge = if style == "lines" { [0.1, 0.1, 0.12] } else { [color[0] * 0.25, color[1] * 0.25, color[2] * 0.27] };
            PartStyle { visible, ghost: ghosted, color, edge }
        })
        .collect();
    let vis: Vec<usize> = (0..styles.len()).filter(|&i| styles[i].visible).collect();
    let section = match &p.section {
        None => None,
        Some(s) => {
            let nrm = match (&s.normal, s.axis.as_deref().map(|a| a.to_ascii_lowercase())) {
                (Some(n), _) => Vec3::from(*n),
                (None, Some(a)) if a == "x" => Vec3::X,
                (None, Some(a)) if a == "y" => Vec3::Y,
                (None, Some(a)) if a == "z" => Vec3::Z,
                _ => anyhow::bail!("section needs axis (x/y/z) or normal"),
            };
            anyhow::ensure!(nrm.length() > 0.0, "section normal must be non-zero");
            let nrm = nrm.normalize();
            let off = match s.offset {
                Some(o) => o,
                None => sc.bounds(&vis).map(|(lo, hi)| ((lo + hi) * 0.5).dot(nrm)).unwrap_or(0.0),
            };
            match s.remove.as_deref().unwrap_or("auto") {
                "auto" => Some((nrm, off, true)),
                "positive" | "+" => Some((nrm, off, false)),
                "negative" | "-" => Some((-nrm, -off, false)),
                o => anyhow::bail!("section.remove must be auto, positive or negative (got {o:?})"),
            }
        }
    };
    let edges = match p.edges.as_deref().unwrap_or("all") {
        "all" => Edges::All,
        "sharp" => Edges::Sharp,
        "none" => Edges::None,
        o => anyhow::bail!("edges must be all, sharp or none (got {o:?})"),
    };
    let background = match p.background.as_deref() {
        Some("transparent") => None,
        Some(h) => Some(parse_hex(h)?),
        None => Some([0.957, 0.961, 0.969]),
    };
    let part_labels = if p.part_labels.unwrap_or(false) {
        vis.iter().map(|&i| (i, sc.parts[i].id.clone())).collect()
    } else {
        Vec::new()
    };
    let req = RenderReq {
        views,
        tile_w,
        tile_h,
        cols,
        up,
        perspective: p.perspective.unwrap_or(false),
        zoom: p.zoom.unwrap_or(1.0),
        meshes: sc.parts.iter().map(|p| &p.gpu).collect(),
        styles,
        boxes: sc.parts.iter().map(|p| (p.bmin, p.bmax)).collect(),
        anchors: sc.parts.iter().map(|p| p.anchor).collect(),
        focus,
        region: p.region.map(|b| (Vec3::new(b[0], b[1], b[2]).min(Vec3::new(b[3], b[4], b[5])), Vec3::new(b[0], b[1], b[2]).max(Vec3::new(b[3], b[4], b[5])))),
        edges,
        silhouettes: p.silhouettes.unwrap_or(true),
        edge_px: p.edge_width.unwrap_or(1.6).clamp(0.5, 8.0),
        section,
        background,
        labels: p.labels.unwrap_or(true),
        scale_bar: p.scale_bar.unwrap_or(true) && !p.perspective.unwrap_or(false),
        part_labels,
        xray: style == "xray",
        unlit: style == "lines",
    };
    let renderer = app.renderer.as_mut().ok_or_else(|| anyhow::anyhow!(app.renderer_err.clone().unwrap_or_default()))?;
    let out = renderer.render(&req)?;
    notes.extend(out.notes);
    if let Some(path) = &p.save {
        std::fs::write(path, &out.png)?;
        notes.push(format!("saved {path}"));
    }
    let names: Vec<String> = view_specs.iter().map(|v| v.to_uppercase()).collect();
    let mut text = format!(
        "{}x{} px, {} view(s) [{}], {} of {} parts visible; {:.0} ms (gpu+readback {:.1}, annotate+png {:.1}), {} KB",
        out.width,
        out.height,
        n,
        names.join(", "),
        vis.len(),
        app.scene.parts.len(),
        t0.elapsed().as_secs_f64() * 1e3,
        out.gpu_ms,
        out.encode_ms,
        out.png.len() / 1024
    );
    if let Some((nrm, off, auto)) = section {
        let side = if auto { "camera-facing half removed per view" } else { "+normal side removed" };
        let _ = write!(text, "; section n=[{:.2},{:.2},{:.2}] at {:.1} mm ({side}, cut faces in cap colour)", nrm.x, nrm.y, nrm.z, off);
    }
    if p.part_labels.unwrap_or(false) {
        // Shorten paths by the common assembly prefix to keep the legend compact.
        let paths: Vec<&str> = vis.iter().map(|&i| app.scene.parts[i].path.as_str()).collect();
        let prefix = paths.first().and_then(|p| p.rfind('/').map(|k| &p[..=k])).filter(|pre| paths.iter().all(|p| p.starts_with(pre))).unwrap_or("");
        let legend: Vec<String> = vis.iter().map(|&i| format!("{}={}", app.scene.parts[i].id, &app.scene.parts[i].path[prefix.len()..])).collect();
        let _ = write!(text, "\nlabels: {}", legend.join(", "));
    }
    for nn in notes {
        let _ = write!(text, "\nnote: {nn}");
    }
    Ok(Rendered { png: out.png, text })
}

fn one(sc: &Scene, s: &str) -> anyhow::Result<usize> {
    let (ix, _) = sc.select(&[s.to_string()]);
    match ix.len() {
        0 => anyhow::bail!("no part matches {s:?}"),
        1 => Ok(ix[0]),
        _ => anyhow::bail!(
            "{s:?} matches {} parts ({}); use an id",
            ix.len(),
            ix.iter().take(8).map(|&i| format!("{} {}", sc.parts[i].id, sc.parts[i].path)).collect::<Vec<_>>().join(", ")
        ),
    }
}

pub fn do_clash(app: &mut App, p: &ClashParams) -> anyhow::Result<String> {
    let t0 = Instant::now();
    let sc = &app.scene;
    let set: Vec<usize> = match &p.parts {
        Some(l) => sc.select(l).0,
        None => (0..sc.parts.len()).collect(),
    };
    let set: Vec<usize> = set.into_iter().filter(|&i| !sc.parts[i].empty).collect();
    let min_v = p.min_volume.unwrap_or(0.5);
    let eps = 0.05f32;
    let mut overlaps = Vec::new();
    let mut touching = Vec::new();
    let mut tested = 0;
    for (k, &a) in set.iter().enumerate() {
        for &b in &set[k + 1..] {
            let (pa, pb) = (&sc.parts[a], &sc.parts[b]);
            let sep = (pa.bmin - pb.bmax).max(pb.bmin - pa.bmax);
            if sep.x > eps || sep.y > eps || sep.z > eps {
                continue;
            }
            tested += 1;
            match sc.common(a, b) {
                Some((v, bb)) if v > min_v => overlaps.push((a, b, v, bb)),
                Some(_) if p.touching.unwrap_or(false) => {
                    if let Some((d, _, _)) = sc.distance(a, b)
                        && d < 0.01
                    {
                        touching.push((a, b));
                    }
                }
                None => overlaps.push((a, b, f64::NAN, [0.0; 6])),
                _ => {}
            }
        }
    }
    let mut s = format!(
        "{} parts, {} pairs with overlapping boxes tested in {:.0} ms\n",
        set.len(),
        tested,
        t0.elapsed().as_secs_f64() * 1e3
    );
    if overlaps.is_empty() {
        let _ = writeln!(s, "no interference above {min_v} mm³");
    }
    for (a, b, v, bb) in &overlaps {
        let (na, nb) = (&sc.parts[*a], &sc.parts[*b]);
        if v.is_nan() {
            let _ = writeln!(s, "CHECK FAILED  {} {} <-> {} {} (boolean error)", na.id, na.path, nb.id, nb.path);
        } else {
            let _ = writeln!(
                s,
                "CLASH {:.1} mm³  {} {} <-> {} {}  overlap box [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}]",
                v, na.id, na.path, nb.id, nb.path, bb[0], bb[1], bb[2], bb[3], bb[4], bb[5]
            );
        }
    }
    for (a, b) in &touching {
        let _ = writeln!(s, "touch  {} {} <-> {} {}", sc.parts[*a].id, sc.parts[*a].path, sc.parts[*b].id, sc.parts[*b].path);
    }
    Ok(s)
}

// ---------- tools ----------

#[tool_router]
impl Shapeshot {
    #[tool(description = "Load STEP/BREP/IGES files (or directories of them) into the scene. Returns parts with ids, sizes and timings. Loaded files auto-reload when they change on disk.")]
    async fn open(&self, Parameters(p): Parameters<OpenParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| Ok(CallToolResult::success(vec![ContentBlock::text(do_open(app, &p)?)]))).await
    }

    #[tool(description = "Render the loaded scene to a PNG returned inline. Several views tile into one image. Supports hide/show/highlight/ghost, colour overrides, section cuts, x-ray, perspective, part labels and a scale bar (orthographic).")]
    async fn render(&self, Parameters(p): Parameters<RenderParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let r = do_render(app, &p)?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&r.png);
            Ok(CallToolResult::success(vec![ContentBlock::image(b64, "image/png"), ContentBlock::text(r.text)]))
        })
        .await
    }

    #[tool(description = "List loaded parts (id, assembly path, file, bounding-box size, triangle count).")]
    async fn parts(&self, Parameters(p): Parameters<PartsParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            if let Some(n) = app.refresh()? {
                log::info(&n);
            }
            let sc = &app.scene;
            let set: Vec<usize> = match &p.filter {
                Some(f) => sc.select(std::slice::from_ref(f)).0,
                None => (0..sc.parts.len()).collect(),
            };
            Ok(CallToolResult::success(vec![ContentBlock::text(parts_table(sc, &set))]))
        })
        .await
    }

    #[tool(description = "Exact B-rep properties of parts: volume, area, mass (with density), centroid, bbox, topology counts, face types, cylinder radii (bend radii, holes) and torus radii.")]
    async fn inspect(&self, Parameters(p): Parameters<InspectParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            app.refresh()?;
            let sc = &app.scene;
            let (ix, missing) = sc.select(&p.parts);
            anyhow::ensure!(!ix.is_empty(), "no part matches {:?}", p.parts);
            let mut out = Vec::new();
            for i in ix {
                let mut v = sc.inspect(i);
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("id".into(), sc.parts[i].id.clone().into());
                    if let (Some(d), Some(vol)) = (p.density, obj.get("volume_mm3").and_then(|v| v.as_f64())) {
                        obj.insert("mass_kg".into(), serde_json::json!((vol * d * 1e-6 * 1000.0).round() / 1000.0));
                    }
                }
                out.push(v);
            }
            let mut s = serde_json::to_string_pretty(&out)?;
            if !missing.is_empty() {
                s += &format!("\nno part matched: {}", missing.join(", "));
            }
            Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
        })
        .await
    }

    #[tool(description = "Exact minimum distance between two parts (B-rep), with the closest points.")]
    async fn measure(&self, Parameters(p): Parameters<MeasureParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            app.refresh()?;
            let sc = &app.scene;
            let (a, b) = (one(sc, &p.a)?, one(sc, &p.b)?);
            let (d, pa, pb) = sc.distance(a, b).ok_or_else(|| anyhow::anyhow!("distance computation failed"))?;
            let s = format!(
                "min distance {} {} <-> {} {}: {:.3} mm\n  on a: [{:.3}, {:.3}, {:.3}]\n  on b: [{:.3}, {:.3}, {:.3}]\n  delta: [{:.3}, {:.3}, {:.3}]",
                sc.parts[a].id, sc.parts[a].path, sc.parts[b].id, sc.parts[b].path, d, pa[0], pa[1], pa[2], pb[0], pb[1], pb[2],
                pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]
            );
            Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
        })
        .await
    }

    #[tool(description = "Interference check: boolean common volume between every pair of parts whose bounding boxes overlap.")]
    async fn clash(&self, Parameters(p): Parameters<ClashParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            app.refresh()?;
            Ok(CallToolResult::success(vec![ContentBlock::text(do_clash(app, &p)?)]))
        })
        .await
    }

    #[tool(description = "Unload everything.")]
    async fn close(&self) -> Result<CallToolResult, ErrorData> {
        self.with_app(|app| {
            app.scene.clear();
            Ok(CallToolResult::success(vec![ContentBlock::text("scene cleared")]))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Shapeshot {
    fn get_info(&self) -> ServerConfig {
        let gpu = self.app.lock().map(|a| a.renderer.as_ref().map(|r| r.adapter_name.clone()).unwrap_or_else(|| "none".into())).unwrap_or_default();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("shapeshot", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "shapeshot renders CAD files (STEP/BREP/IGES via OpenCascade {}) on the GPU ({gpu}) and answers exact geometry questions. \
                 Workflow: `open` files once; edit the source model and re-export; every call reloads changed files automatically. \
                 `render` returns the image inline: ask for several views in one call (e.g. [\"iso\",\"front\",\"right\",\"top\"]) instead of several calls. \
                 Orthographic views carry a scale bar. Units are mm. Use `inspect`, `measure` and `clash` for numbers instead of judging from pixels.",
                crate::occt::occt_version()
            ))
    }
}

mod log {
    pub fn info(s: &str) {
        eprintln!("[shapeshot] {s}");
    }
}
