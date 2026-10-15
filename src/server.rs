//! MCP tools: open, render, parts, inspect, measure, clash, move, motion, save_view, saved, close.

use crate::camera::{self, CamSpec, Up};
use crate::project::{self, Project, SavedPose, SavedView};
use crate::render::{Edges, PartStyle, RenderReq, Renderer};
use crate::photo;
use crate::scene::{self, Scene};
use base64::Engine;
use glam::{Mat4, Vec3};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct App {
    pub renderer: Option<Renderer>,
    pub renderer_err: Option<String>,
    pub scene: Scene,
    pub project: Project,
    pub project_path: Option<std::path::PathBuf>,
    /// Parameters and resolved cameras of the last render (for `save_view`).
    pub last_render: Option<(serde_json::Value, Vec<CamSpec>)>,
}

impl App {
    pub fn new() -> App {
        let (renderer, renderer_err) = match Renderer::new() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(format!("GPU init failed: {e:#}"))),
        };
        App { renderer, renderer_err, scene: Scene::new(), project: Project::default(), project_path: None, last_render: None }
    }

    fn save_project(&self) -> anyhow::Result<String> {
        let path = self.project_path.as_ref().ok_or_else(|| anyhow::anyhow!("no project file: call open first"))?;
        self.project.save(path)?;
        Ok(path.display().to_string())
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
    /// Project file for saved views/poses/motions (default: .shapeshot.json next to the first file).
    pub project: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
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

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct RenderParams {
    /// Start from a saved view (see `save_view`/`saved`); other fields given here override it.
    /// Its frozen cameras are reused unless you also pass views/region/focus/zoom/perspective/up.
    pub preset: Option<String>,
    /// Exact cameras, one per tile (as reported by `saved`); overrides views.
    pub cameras: Option<Vec<CamSpec>>,
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

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(untagged)]
pub enum Axis {
    /// "x", "y" or "z"
    Name(String),
    /// direction vector
    Vec([f32; 3]),
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct RotateSpec {
    /// Rotation axis: "x", "y", "z" or [x, y, z].
    pub axis: Axis,
    /// Angle in degrees (right-hand rule around the axis).
    pub deg: f32,
    /// Point on the axis in mm (default: centre of the moved parts' bounding box).
    pub origin: Option<[f32; 3]>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveParams {
    /// Parts to move (selectors). Default for `reset`: all parts.
    pub parts: Option<Vec<String>>,
    /// Translation in mm.
    pub translate: Option<[f32; 3]>,
    /// Rotation (applied before the translation).
    pub rotate: Option<RotateSpec>,
    /// Replace the parts' pose instead of adding to it (default false).
    pub absolute: Option<bool>,
    /// Reset the parts (or all) to their position in the file first.
    pub reset: Option<bool>,
    /// Apply a saved pose first.
    pub pose: Option<String>,
    /// Save the resulting pose of every moved part under this name.
    pub save_pose: Option<String>,
    /// Note stored with the saved pose.
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct MotionParams {
    /// Run a saved motion; other fields given here override it.
    pub run: Option<String>,
    /// Moving parts (selectors).
    pub parts: Option<Vec<String>>,
    /// Total translation over the motion, mm.
    pub translate: Option<[f32; 3]>,
    /// Total rotation over the motion.
    pub rotate: Option<RotateSpec>,
    /// Number of steps (default 10; checks are done at steps+1 positions).
    pub steps: Option<u32>,
    /// Parts to check against (default: every other visible part).
    pub against: Option<Vec<String>>,
    /// Only compute exact clearances closer than this, mm (default 50).
    pub clearance_mm: Option<f32>,
    /// Frames in the contact sheet (default 4, 0 = no image). The first contact is always shown.
    pub frames: Option<u32>,
    /// Render settings for the frames (e.g. {"preset": "lamps"} or {"views": ["right"], "highlight": [...]}); the first view is used.
    pub render: Option<RenderParams>,
    /// Save this motion under a name.
    pub save_as: Option<String>,
    /// Note stored with the saved motion.
    pub note: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PhotoParams {
    /// Saved view to photograph (its frozen camera is reused unless you reframe).
    pub preset: Option<String>,
    /// View (first one is used): same names as `render` (iso, front, az30el20, dir:x,y,z).
    pub views: Option<Vec<String>>,
    /// Exact camera (as stored by `save_view`).
    pub cameras: Option<Vec<CamSpec>>,
    /// Frame this box: [xmin, ymin, zmin, xmax, ymax, zmax] mm.
    pub region: Option<[f32; 6]>,
    /// Frame these parts.
    pub focus: Option<Vec<String>>,
    /// Zoom factor (>1 zooms in).
    pub zoom: Option<f32>,
    /// Perspective (default true for photos unless a saved view says otherwise).
    pub perspective: Option<bool>,
    /// Model up axis: "z" (default) or "y".
    pub up: Option<String>,
    /// Only show these parts.
    pub show: Option<Vec<String>>,
    /// Hide these parts.
    pub hide: Option<Vec<String>>,
    /// Materials: selector -> preset (aluminium, brushed_aluminium, anodised_black, steel, stainless,
    /// zinc, black_powdercoat, rubber, plastic, chrome) or "#rrggbb" paint. Default: guessed from the CAD colour.
    pub materials: Option<BTreeMap<String, String>>,
    /// Image size in px (default 1600 x 1000).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Path-tracing samples (default 64, denoised).
    pub samples: Option<u32>,
    /// Ground plane with soft shadows (default true).
    pub floor: Option<bool>,
    /// Environment colour "#rrggbb" (default light studio grey).
    pub background: Option<String>,
    /// Exposure in stops (default 0; +1 = twice as bright).
    pub exposure: Option<f32>,
    /// Seconds before giving up (default 180; the first run compiles Metal kernels).
    pub timeout_s: Option<u64>,
    /// Also write the PNG to this path.
    pub save: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SaveViewParams {
    /// Name to save the last render under (overwrites).
    pub name: String,
    /// What this view is for.
    pub note: Option<String>,
    /// Store the exact cameras so the view never drifts when the model changes (default true).
    pub freeze: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SavedParams {
    /// Delete a saved view by name.
    pub delete_view: Option<String>,
    /// Delete a saved pose by name.
    pub delete_pose: Option<String>,
    /// Delete a saved motion by name.
    pub delete_motion: Option<String>,
}

// ---------- helpers ----------

fn axis_vec(a: &Axis) -> anyhow::Result<Vec3> {
    let v = match a {
        Axis::Name(n) => match n.to_ascii_lowercase().as_str() {
            "x" | "+x" => Vec3::X,
            "y" | "+y" => Vec3::Y,
            "z" | "+z" => Vec3::Z,
            "-x" => Vec3::NEG_X,
            "-y" => Vec3::NEG_Y,
            "-z" => Vec3::NEG_Z,
            o => anyhow::bail!("axis must be x, y, z or [x,y,z] (got {o:?})"),
        },
        Axis::Vec(v) => Vec3::from(*v),
    };
    anyhow::ensure!(v.length() > 1e-6, "axis must be non-zero");
    Ok(v.normalize())
}

/// Rigid transform for fraction `t` of a move: rotate about the axis through `origin`, then translate.
fn motion_at(t: f32, translate: Option<[f32; 3]>, rotate: Option<(Vec3, f32, Vec3)>) -> Mat4 {
    let mut m = Mat4::IDENTITY;
    if let Some((axis, deg, origin)) = rotate {
        m = Mat4::from_translation(origin) * Mat4::from_axis_angle(axis, (deg * t).to_radians()) * Mat4::from_translation(-origin);
    }
    if let Some(v) = translate {
        m = Mat4::from_translation(Vec3::from(v) * t) * m;
    }
    m
}

fn resolve_rotate(sc: &Scene, set: &[usize], r: &Option<RotateSpec>) -> anyhow::Result<Option<(Vec3, f32, Vec3)>> {
    let Some(r) = r else { return Ok(None) };
    let origin = match r.origin {
        Some(o) => Vec3::from(o),
        None => sc.bounds(set).map(|(lo, hi)| (lo + hi) * 0.5).unwrap_or(Vec3::ZERO),
    };
    Ok(Some((axis_vec(&r.axis)?, r.deg, origin)))
}

fn short_path(sc: &Scene, i: usize) -> String {
    let p = &sc.parts[i].path;
    let s = p.split_once('/').map(|(_, rest)| rest).filter(|r| !r.is_empty()).unwrap_or(p);
    format!("{} {}", sc.parts[i].id, s)
}


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
    let ppath = match &p.project {
        Some(pp) => std::path::PathBuf::from(pp),
        None => project::default_path(&files[0]),
    };
    if app.project_path.as_ref() != Some(&ppath) {
        app.project = Project::load(&ppath)?;
        app.project_path = Some(ppath.clone());
    }
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
    let _ = writeln!(s, "total {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let _ = writeln!(s, "project {}:\n{}\n", ppath.display(), app.project.summary());
    s += &parts_table(&app.scene, &all);
    Ok(s)
}

pub struct Rendered {
    pub png: Vec<u8>,
    pub text: String,
    /// Resolved camera per tile, the parts drawn and their styles (for `photo`).
    pub cams: Vec<CamSpec>,
    pub styles: Vec<PartStyle>,
    pub poses: Vec<Mat4>,
}

/// Motion frames: every tile uses the first view's camera with its own set of part poses.
pub struct Frames {
    pub poses: Vec<Vec<Mat4>>,
    pub labels: Vec<String>,
}

/// Merge a saved view into the call's parameters; returns its frozen cameras unless the call
/// changes the framing.
fn resolve_preset(app: &App, p: &RenderParams) -> anyhow::Result<(RenderParams, Option<Vec<CamSpec>>, Option<String>)> {
    let Some(name) = &p.preset else { return Ok((p.clone(), None, None)) };
    let sv = app.project.views.get(name).ok_or_else(|| {
        anyhow::anyhow!("no saved view {name:?}; saved: {}", app.project.views.keys().cloned().collect::<Vec<_>>().join(", "))
    })?;
    let merged: RenderParams = serde_json::from_value(project::overlay(&sv.params, &serde_json::to_value(p)?))?;
    let reframed = p.views.is_some() || p.region.is_some() || p.focus.is_some() || p.zoom.is_some() || p.perspective.is_some() || p.up.is_some() || p.cameras.is_some();
    let cams = if reframed { None } else { sv.cameras.clone() };
    let note = Some(format!("view {name:?}{}", if cams.is_some() { " (frozen camera)" } else { "" }));
    Ok((merged, cams, note))
}

pub fn do_render(app: &mut App, p: &RenderParams) -> anyhow::Result<Rendered> {
    do_render_frames(app, p, None)
}

pub fn do_render_frames(app: &mut App, p_in: &RenderParams, frames: Option<Frames>) -> anyhow::Result<Rendered> {
    let t0 = Instant::now();
    let reload = app.refresh()?;
    anyhow::ensure!(!app.scene.parts.is_empty(), "nothing loaded: call open first");
    let mut notes: Vec<String> = reload.into_iter().collect();
    let (p_owned, preset_cams, preset_note) = resolve_preset(app, p_in)?;
    notes.extend(preset_note);
    let p = &p_owned;
    let sc = &app.scene;
    let up = Up::parse(p.up.as_deref())?;
    let view_specs = p.views.clone().unwrap_or_else(|| vec!["iso".into()]);
    anyhow::ensure!(!view_specs.is_empty() && view_specs.len() <= 16, "1..16 views");
    let mut views = match p.cameras.clone().or(preset_cams) {
        Some(cams) => cams
            .into_iter()
            .enumerate()
            .map(|(k, c)| camera::View {
                label: view_specs.get(k).map(|v| v.to_uppercase()).unwrap_or_else(|| format!("VIEW {}", k + 1)),
                dir: Vec3::from(c.dir),
                fixed: Some(c),
            })
            .collect(),
        None => view_specs.iter().map(|v| camera::parse_view(v, up)).collect::<anyhow::Result<Vec<_>>>()?,
    };
    if let Some(f) = &frames {
        let base = views.into_iter().next().ok_or_else(|| anyhow::anyhow!("no view"))?;
        views = f.labels.iter().map(|l| camera::View { label: l.clone(), dir: base.dir, fixed: base.fixed }).collect();
    }
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
        poses: match &frames {
            Some(f) => f.poses.clone(),
            None => vec![sc.parts.iter().map(|p| p.pose).collect()],
        },
    };
    let labels_out: Vec<String> = req.views.iter().map(|v| v.label.clone()).collect();
    let styles_out = req.styles.clone();
    let poses_out = req.poses[0].clone();
    let renderer = app.renderer.as_mut().ok_or_else(|| anyhow::anyhow!(app.renderer_err.clone().unwrap_or_default()))?;
    let out = renderer.render(&req)?;
    notes.extend(out.notes);
    if let Some(path) = &p.save {
        std::fs::write(path, &out.png)?;
        notes.push(format!("saved {path}"));
    }
    if frames.is_none() {
        let mut keep = p.clone();
        keep.preset = None;
        keep.cameras = None;
        keep.save = None;
        app.last_render = Some((serde_json::to_value(&keep)?, out.cams.clone()));
    }
    let names = labels_out;
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
    Ok(Rendered { png: out.png, text, cams: out.cams, styles: styles_out, poses: poses_out })
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
            let ((amin, amax), (bmin, bmax)) = (sc.world_box(a), sc.world_box(b));
            let sep = (amin - bmax).max(bmin - amax);
            if sep.x > eps || sep.y > eps || sep.z > eps {
                continue;
            }
            tested += 1;
            let common = sc.common(a, b);
            if common.is_none_or(|(v, _)| v <= min_v)
                && sc.distance(a, b).is_some_and(|(d, _, _)| d < 0.01)
                && sc.interferes_at(a, sc.parts[a].pose, b, sc.parts[b].pose)
            {
                let (lo, hi) = (amin.max(bmin), amax.min(bmax));
                overlaps.push((a, b, f64::INFINITY, [lo.x as f64, lo.y as f64, lo.z as f64, hi.x as f64, hi.y as f64, hi.z as f64]));
                continue;
            }
            match common {
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
        if v.is_infinite() {
            let _ = writeln!(
                s,
                "CLASH (volume unknown: boolean failed, confirmed by point classification)  {} {} <-> {} {}  box overlap [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}]",
                na.id, na.path, nb.id, nb.path, bb[0], bb[1], bb[2], bb[3], bb[4], bb[5]
            );
        } else if v.is_nan() {
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

pub fn do_move(app: &mut App, p: &MoveParams) -> anyhow::Result<String> {
    app.refresh()?;
    let mut s = String::new();
    if let Some(name) = &p.pose {
        let pose = app.project.poses.get(name).ok_or_else(|| anyhow::anyhow!("no saved pose {name:?}"))?.clone();
        let mut n = 0;
        for part in app.scene.parts.iter_mut() {
            if let Some(m) = pose.parts.get(&part.path) {
                part.pose = Mat4::from_cols_array(m);
                n += 1;
            }
        }
        let _ = writeln!(s, "applied pose {name:?} to {n} parts");
    }
    let sel: Option<Vec<usize>> = match &p.parts {
        Some(l) => {
            let (ix, missing) = app.scene.select(l);
            anyhow::ensure!(missing.is_empty(), "no part matched: {}", missing.join(", "));
            Some(ix)
        }
        None => None,
    };
    if p.reset.unwrap_or(false) {
        let set = sel.clone().unwrap_or_else(|| (0..app.scene.parts.len()).collect());
        for &i in &set {
            app.scene.parts[i].pose = Mat4::IDENTITY;
        }
        let _ = writeln!(s, "reset {} parts", set.len());
    }
    if p.translate.is_some() || p.rotate.is_some() {
        let set = sel.clone().ok_or_else(|| anyhow::anyhow!("parts required to move"))?;
        let rot = resolve_rotate(&app.scene, &set, &p.rotate)?;
        let m = motion_at(1.0, p.translate, rot);
        for &i in &set {
            let part = &mut app.scene.parts[i];
            part.pose = if p.absolute.unwrap_or(false) { m } else { m * part.pose };
        }
        let _ = writeln!(s, "moved {} parts", set.len());
    }
    let posed: Vec<usize> = (0..app.scene.parts.len()).filter(|&i| app.scene.parts[i].pose != Mat4::IDENTITY).collect();
    for &i in &posed {
        let (lo, hi) = app.scene.world_box(i);
        let t = app.scene.parts[i].pose.w_axis;
        let _ = writeln!(s, "  {}: offset [{:.1}, {:.1}, {:.1}], bbox [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}]", short_path(&app.scene, i), t.x, t.y, t.z, lo.x, lo.y, lo.z, hi.x, hi.y, hi.z);
    }
    let _ = writeln!(s, "{} parts away from their file position", posed.len());
    if let Some(name) = &p.save_pose {
        let set = sel.unwrap_or(posed);
        let parts = set.iter().map(|&i| (app.scene.parts[i].path.clone(), app.scene.parts[i].pose.to_cols_array())).collect();
        app.project.poses.insert(name.clone(), SavedPose { parts, note: p.note.clone() });
        let _ = writeln!(s, "saved pose {name:?} in {}", app.save_project()?);
    }
    Ok(s)
}

pub fn do_motion(app: &mut App, p_in: &MotionParams) -> anyhow::Result<(String, Option<Vec<u8>>)> {
    let t0 = Instant::now();
    app.refresh()?;
    let p: MotionParams = match &p_in.run {
        Some(name) => {
            let saved = app.project.motions.get(name).ok_or_else(|| anyhow::anyhow!("no saved motion {name:?}"))?;
            serde_json::from_value(project::overlay(saved, &serde_json::to_value(p_in)?))?
        }
        None => p_in.clone(),
    };
    let sc = &app.scene;
    let (moving, missing) = sc.select(p.parts.as_deref().ok_or_else(|| anyhow::anyhow!("parts required"))?);
    anyhow::ensure!(missing.is_empty() && !moving.is_empty(), "no part matched: {}", missing.join(", "));
    anyhow::ensure!(p.translate.is_some() || p.rotate.is_some(), "give translate and/or rotate");
    let against: Vec<usize> = match &p.against {
        Some(l) => sc.select(l).0,
        None => (0..sc.parts.len()).collect(),
    };
    let against: Vec<usize> = against.into_iter().filter(|i| !moving.contains(i) && !sc.parts[*i].empty).collect();
    let steps = p.steps.unwrap_or(10).clamp(1, 200);
    let horizon = p.clearance_mm.unwrap_or(50.0).max(0.0);
    let rot = resolve_rotate(sc, &moving, &p.rotate)?;

    struct StepRes {
        t: f32,
        min: Option<(f64, usize, usize)>,
        clash: f64,
        clash_pair: Option<(usize, usize)>,
        failed: usize,
        /// Interfering pairs found by point classification (boolean gave no volume).
        no_volume: usize,
    }
    // Each step is independent: run them on all cores. Per step, pairs whose boxes overlap get a
    // boolean check; the rest are measured nearest-box-first and pruned once no box can beat the
    // closest exact distance found so far.
    let check_step = |k: u32| -> (StepRes, usize) {
        let t = k as f32 / steps as f32;
        let m = motion_at(t, p.translate, rot);
        let mut r = StepRes { t, min: None, clash: 0.0, clash_pair: None, failed: 0, no_volume: 0 };
        let mut checks = 0;
        let mut far: Vec<(f32, usize, usize, Mat4)> = Vec::new();
        for &a in &moving {
            let ma = m * sc.parts[a].pose;
            let (alo, ahi) = sc.posed_box(a, ma);
            for &b in &against {
                let (blo, bhi) = sc.world_box(b);
                let sep = (alo - bhi).max(blo - ahi);
                if sep.max_element() > horizon {
                    continue;
                }
                if sep.max_element() <= 0.05 {
                    checks += 1;
                    match sc.common_at(a, ma, b, sc.parts[b].pose) {
                        Some((v, _)) if v > 0.5 => {
                            r.clash += v;
                            r.clash_pair.get_or_insert((a, b));
                            r.min = Some((0.0, a, b));
                            continue;
                        }
                        None => r.failed += 1,
                        _ => {}
                    }
                }
                far.push((sep.max(Vec3::ZERO).length(), a, b, ma));
            }
        }
        far.sort_by(|x, y| x.0.total_cmp(&y.0));
        for (lb, a, b, ma) in far {
            // Overlapping-box pairs (lb ~ 0) are always measured: they may hide a failed boolean.
            if r.min.is_some_and(|(d, _, _)| lb as f64 >= d) && lb > 0.05 {
                break;
            }
            checks += 1;
            if let Some((d, _, _)) = sc.distance_at(a, ma, b, sc.parts[b].pose)
                && (d as f32) <= horizon
            {
                // Touching per the exact distance but no boolean overlap: confirm by classification.
                if d < 0.01 && lb <= 0.05 && sc.interferes_at(a, ma, b, sc.parts[b].pose) {
                    r.no_volume += 1;
                    r.clash_pair.get_or_insert((a, b));
                    r.min = Some((0.0, a, b));
                    continue;
                }
                if r.min.is_none_or(|(md, _, _)| d < md) {
                    r.min = Some((d, a, b));
                }
            }
        }
        (r, checks)
    };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(steps as usize + 1);
    let mut slots: Vec<Option<(StepRes, usize)>> = (0..=steps).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|tid| {
                let check_step = &check_step;
                scope.spawn(move || (tid as u32..=steps).step_by(threads).map(|k| (k, check_step(k))).collect::<Vec<_>>())
            })
            .collect();
        for h in handles {
            for (k, v) in h.join().expect("motion worker panicked") {
                slots[k as usize] = Some(v);
            }
        }
    });
    let mut exact_checks = 0;
    let res: Vec<StepRes> = slots
        .into_iter()
        .map(|v| {
            let (r, c) = v.expect("step computed");
            exact_checks += c;
            r
        })
        .collect();
    let first_clash = res.iter().position(|r| r.clash > 0.0 || r.no_volume > 0);
    let first_touch = res.iter().position(|r| r.min.is_some_and(|(d, _, _)| d < 0.01));
    let tightest = res.iter().enumerate().filter_map(|(k, r)| r.min.map(|(d, a, b)| (k, d, a, b))).min_by(|x, y| x.1.total_cmp(&y.1));

    let mut s = String::new();
    let desc = format!(
        "{}{}",
        p.translate.map(|v| format!("translate [{:.1}, {:.1}, {:.1}] mm ", v[0], v[1], v[2])).unwrap_or_default(),
        rot.map(|(a, d, o)| format!("rotate {d:.1} deg about [{:.2}, {:.2}, {:.2}] through [{:.1}, {:.1}, {:.1}]", a.x, a.y, a.z, o.x, o.y, o.z)).unwrap_or_default()
    );
    let _ = writeln!(s, "motion of {} part(s): {desc}, {} steps, {} exact checks, {:.0} ms", moving.len(), steps, exact_checks, t0.elapsed().as_secs_f64() * 1e3);
    let _ = writeln!(s, "step |  t   | clearance mm | nearest pair | clash mm³");
    for (k, r) in res.iter().enumerate() {
        let (c, pair) = match r.min {
            Some((d, a, b)) => (format!("{d:.2}"), format!("{} <-> {}", short_path(sc, a), short_path(sc, b))),
            None => (format!("> {horizon:.0}"), String::new()),
        };
        let clash = if r.clash > 0.0 || r.no_volume > 0 {
            let mut c = if r.clash > 0.0 { format!("{:.1}", r.clash) } else { String::new() };
            if r.no_volume > 0 {
                let _ = write!(c, "{}+{} pair(s) interfering, volume unknown", if c.is_empty() { "" } else { " " }, r.no_volume);
            }
            c
        } else if r.failed > 0 {
            format!("? ({} boolean check(s) failed)", r.failed)
        } else {
            "-".into()
        };
        let _ = writeln!(s, "{k:4} | {:.2} | {c:>12} | {pair} | {clash}", r.t);
    }
    match (first_clash, first_touch) {
        (Some(k), _) => {
            let (a, b) = res[k].clash_pair.unwrap();
            let _ = writeln!(s, "FIRST INTERFERENCE at step {k} (t={:.2}): {} <-> {}", res[k].t, short_path(sc, a), short_path(sc, b));
        }
        (None, Some(k)) => {
            let _ = writeln!(s, "first contact at step {k} (t={:.2}), no interference", res[k].t);
        }
        _ => {
            let _ = writeln!(s, "no contact during the motion");
        }
    }
    if let Some((k, d, a, b)) = tightest {
        let _ = writeln!(s, "tightest: {d:.2} mm at step {k} ({} <-> {})", short_path(sc, a), short_path(sc, b));
    }

    // Contact sheet.
    let nframes = p.frames.unwrap_or(4).min(16) as usize;
    let mut png = None;
    if nframes > 0 {
        let mut idx: Vec<usize> = if nframes == 1 { vec![steps as usize] } else { (0..nframes).map(|f| (f * steps as usize + (nframes - 1) / 2) / (nframes - 1)).collect() };
        if let Some(k) = first_clash.or(first_touch)
            && !idx.contains(&k)
        {
            let j = idx.iter().enumerate().min_by_key(|(_, v)| v.abs_diff(k)).map(|(j, _)| j).unwrap_or(0);
            idx[j] = k;
        }
        idx.sort();
        idx.dedup();
        let base: Vec<Mat4> = sc.parts.iter().map(|p| p.pose).collect();
        let poses: Vec<Vec<Mat4>> = idx
            .iter()
            .map(|&k| {
                let m = motion_at(res[k].t, p.translate, rot);
                let mut v = base.clone();
                for &i in &moving {
                    v[i] = m * base[i];
                }
                v
            })
            .collect();
        let labels: Vec<String> = idx
            .iter()
            .map(|&k| {
                let r = &res[k];
                let state = if r.clash > 0.0 || r.no_volume > 0 {
                    if r.clash > 0.0 { format!("CLASH {:.0} MM3", r.clash) } else { "CLASH".into() }
                } else {
                    r.min.map(|(d, _, _)| format!("{d:.1} MM")).unwrap_or_else(|| format!(">{horizon:.0} MM"))
                };
                format!("STEP {k}/{steps}  {state}")
            })
            .collect();
        let mut rp = p.render.clone().unwrap_or_default();
        rp.cols = rp.cols.or(Some(if idx.len() <= 4 { 2 } else { 3 }));
        let out = do_render_frames(app, &rp, Some(Frames { poses, labels }))?;
        let _ = writeln!(s, "frames: steps {:?}; {}", idx, out.text.lines().next().unwrap_or(""));
        png = Some(out.png);
    }
    if let Some(name) = &p.save_as {
        let mut keep = p.clone();
        keep.run = None;
        keep.save_as = None;
        app.project.motions.insert(name.clone(), project::strip_nulls(serde_json::to_value(&keep)?));
        let _ = writeln!(s, "saved motion {name:?} in {}", app.save_project()?);
    }
    Ok((s, png))
}

pub fn do_photo(app: &mut App, p: &PhotoParams) -> anyhow::Result<(Vec<u8>, String)> {
    let t0 = Instant::now();
    let (w, h) = (p.width.unwrap_or(1600).clamp(64, 8192), p.height.unwrap_or(1000).clamp(64, 8192));
    // Resolve camera, visibility and poses exactly like `render` (a quick GPU pass at the same aspect).
    let scale = (2048.0 / w.max(h) as f32).min(1.0);
    let has_saved_cam = p.preset.as_ref().is_some_and(|n| app.project.views.get(n).is_some_and(|v| v.cameras.is_some()));
    let rp = RenderParams {
        preset: p.preset.clone(),
        views: p.views.clone(),
        cameras: p.cameras.clone(),
        region: p.region,
        focus: p.focus.clone(),
        zoom: p.zoom,
        perspective: p.perspective.or(if has_saved_cam || p.cameras.is_some() { None } else { Some(true) }),
        up: p.up.clone(),
        show: p.show.clone(),
        hide: p.hide.clone(),
        width: Some(((w as f32 * scale) as u32).max(64)),
        height: Some(((h as f32 * scale) as u32).max(64)),
        labels: Some(false),
        scale_bar: Some(false),
        ..Default::default()
    };
    let resolved = do_render(app, &rp)?;
    let cam = *resolved.cams.first().ok_or_else(|| anyhow::anyhow!("no camera"))?;
    let sc = &app.scene;
    let mut materials: Vec<photo::Material> =
        sc.parts.iter().map(|part| photo::auto_material(part.file_color.unwrap_or([0.74, 0.77, 0.81]))).collect();
    let mut notes = Vec::new();
    if let Some(ms) = &p.materials {
        for (sel, name) in ms {
            let (ix, missing) = sc.select(std::slice::from_ref(sel));
            if !missing.is_empty() {
                notes.push(format!("no part matched {sel:?}"));
            }
            let m = match photo::material_preset(&name.to_ascii_lowercase()) {
                Some(m) => m,
                None if name.starts_with('#') => photo::Material { color: parse_hex(name)?, metallic: 0.0, roughness: 0.4, anisotropic: 0.0 },
                None => anyhow::bail!("unknown material {name:?}"),
            };
            for i in ix {
                materials[i] = m;
            }
        }
    }
    let bg = match &p.background {
        Some(h) => parse_hex(h)?,
        None => [0.92, 0.93, 0.95],
    };
    let work_dir = std::env::temp_dir().join(format!("shapeshot-photo-{}", std::process::id()));
    let out = photo::photo(&photo::PhotoReq {
        scene: sc,
        cam,
        width: w,
        height: h,
        styles: &resolved.styles,
        poses: &resolved.poses,
        materials,
        samples: p.samples.unwrap_or(64).clamp(1, 4096),
        floor: p.floor.unwrap_or(true),
        background: bg,
        timeout: std::time::Duration::from_secs(p.timeout_s.unwrap_or(180)),
        exposure: p.exposure.unwrap_or(0.0),
        work_dir,
    })?;
    if let Some(path) = &p.save {
        std::fs::write(path, &out.png)?;
        notes.push(format!("saved {path}"));
    }
    let mut text = format!(
        "{w}x{h} px path-traced (Cycles, Metal), {} samples, {} tris; {:.1} s total (scene export {:.0} ms, render {:.1} s), {} KB",
        p.samples.unwrap_or(64),
        out.tris,
        t0.elapsed().as_secs_f64(),
        out.export_ms,
        out.render_ms / 1e3,
        out.png.len() / 1024
    );
    for n in notes {
        let _ = write!(text, "\nnote: {n}");
    }
    Ok((out.png, text))
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

    #[tool(name = "move", description = "Move parts (rigid translate/rotate) or apply/reset/save a named pose. Later render/measure/clash/motion calls use the moved positions. Poses are saved in the project file.")]
    async fn r#move(&self, Parameters(p): Parameters<MoveParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| Ok(CallToolResult::success(vec![ContentBlock::text(do_move(app, &p)?)]))).await
    }

    #[tool(description = "Sweep parts through a motion (translate and/or rotate) in steps; at every step reports exact clearance to nearby parts and any interference, the first contact, and returns a contact sheet of frames. Motions can be saved and re-run by name.")]
    async fn motion(&self, Parameters(p): Parameters<MotionParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (text, png) = do_motion(app, &p)?;
            let mut content = Vec::new();
            if let Some(png) = png {
                content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(&png), "image/png"));
            }
            content.push(ContentBlock::text(text));
            Ok(CallToolResult::success(content))
        })
        .await
    }

    #[tool(description = "Photo-realistic still (path-traced with Cycles on the GPU): physical materials (aluminium, steel, rubber, paint...), soft studio light and a shadowed floor. Takes the same viewpoint options as `render`, incl. saved views and poses. Seconds per image, so use `render` for checks and `photo` for presentation.")]
    async fn photo(&self, Parameters(p): Parameters<PhotoParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (png, text) = do_photo(app, &p)?;
            Ok(CallToolResult::success(vec![ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(&png), "image/png"), ContentBlock::text(text)]))
        })
        .await
    }

    #[tool(description = "Save the last render (parameters + exact cameras) as a named view in the project file, so `render {preset: name}` reproduces the same point of view in any session.")]
    async fn save_view(&self, Parameters(p): Parameters<SaveViewParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (params, cams) = app.last_render.clone().ok_or_else(|| anyhow::anyhow!("render something first"))?;
            let freeze = p.freeze.unwrap_or(true);
            app.project.views.insert(p.name.clone(), SavedView { params: project::strip_nulls(params), cameras: freeze.then_some(cams), note: p.note.clone() });
            let path = app.save_project()?;
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "saved view {:?}{} in {path}",
                p.name,
                if freeze { " with frozen camera" } else { "" }
            ))]))
        })
        .await
    }

    #[tool(description = "List saved views, poses and motions of the project (or delete one).")]
    async fn saved(&self, Parameters(p): Parameters<SavedParams>) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let mut s = String::new();
            let mut changed = false;
            if let Some(n) = &p.delete_view {
                changed |= app.project.views.remove(n).is_some();
            }
            if let Some(n) = &p.delete_pose {
                changed |= app.project.poses.remove(n).is_some();
            }
            if let Some(n) = &p.delete_motion {
                changed |= app.project.motions.remove(n).is_some();
            }
            if changed {
                let _ = writeln!(s, "deleted; saved {}", app.save_project()?);
            }
            let _ = writeln!(s, "project: {}", app.project_path.as_ref().map(|p| p.display().to_string()).unwrap_or("(none, call open)".into()));
            let _ = writeln!(s, "\nviews:");
            for (k, v) in &app.project.views {
                let views = v.params.get("views").map(|x| x.to_string()).unwrap_or("[\"iso\"]".into());
                let _ = writeln!(s, "  {k}: {views}{}{}", if v.cameras.is_some() { " frozen" } else { "" }, v.note.as_ref().map(|n| format!(" - {n}")).unwrap_or_default());
            }
            let _ = writeln!(s, "poses:");
            for (k, v) in &app.project.poses {
                let _ = writeln!(s, "  {k}: {} parts{}", v.parts.len(), v.note.as_ref().map(|n| format!(" - {n}")).unwrap_or_default());
            }
            let _ = writeln!(s, "motions:");
            for (k, v) in &app.project.motions {
                let _ = writeln!(s, "  {k}: {}", v);
            }
            Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
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
