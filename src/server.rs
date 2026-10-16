//! MCP tools: open, render, parts, inspect, measure, clash, move, motion, save_view, saved, close.

use crate::camera::{self, CamSpec, Up};
use crate::photo;
use crate::project::{self, Project, SavedPose, SavedView};
use crate::render::{Edges, PartStyle, RenderReq, Renderer};
use crate::scene::{self, Scene};
use crate::templates;
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
    /// Decoded background images by path (reloaded when the file changes).
    pub images: std::collections::HashMap<
        std::path::PathBuf,
        (Option<std::time::SystemTime>, Arc<crate::video::Frame>),
    >,
}

impl App {
    pub fn new() -> App {
        let (renderer, renderer_err) = match Renderer::new() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(format!("GPU init failed: {e:#}"))),
        };
        App {
            renderer,
            renderer_err,
            scene: Scene::new(),
            project: Project::default(),
            project_path: None,
            last_render: None,
            images: Default::default(),
        }
    }

    /// Load (or reuse) a background image.
    pub fn image(&mut self, path: &str) -> anyhow::Result<Arc<crate::video::Frame>> {
        let p = std::path::PathBuf::from(scene::shellexpand(path));
        let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        if let Some((t, f)) = self.images.get(&p)
            && *t == mtime
        {
            return Ok(f.clone());
        }
        let f = Arc::new(crate::video::load_image(&p)?);
        self.images.insert(p, (mtime, f.clone()));
        Ok(f)
    }

    fn save_project(&self) -> anyhow::Result<String> {
        let path = self
            .project_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no project file: call open first"))?;
        self.project.save(path)?;
        Ok(path.display().to_string())
    }

    /// Reload changed files; returns a note for the response.
    fn refresh(&mut self) -> anyhow::Result<Option<String>> {
        let changed = self.scene.refresh(self.renderer.as_ref())?;
        Ok((!changed.is_empty())
            .then(|| format!("reloaded (file changed): {}", changed.join(", "))))
    }
}

#[derive(Clone)]
pub struct CadRender {
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
    /// Project file for saved views/poses/motions (default: .cad-render.json next to the first file).
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
    /// Start from a template: a reusable render setup that works on any model. Built-in:
    /// overview, drawing_first_angle, drawing_third_angle, six_views_first_angle,
    /// six_views_third_angle, turnaround, presentation; plus your own (`saved` lists them all).
    /// Other fields given here override it.
    pub template: Option<String>,
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
    /// Fit the principal views (front/back/left/right/top/bottom) at one shared scale, centred
    /// on the same point, so they line up like the views of a drawing (default false). Use "" in
    /// `views` for an empty tile.
    pub same_scale: Option<bool>,
    /// Separator lines between tiles (default true).
    pub grid: Option<bool>,
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
    /// Backdrop image (PNG/JPEG path) behind the model in every tile.
    pub background_image: Option<String>,
    /// How the backdrop fills a tile: "cover" (crop, default) or "contain" (letterbox on `background`).
    pub background_fit: Option<String>,
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

/// Photo-realistic look, shared by `photo` and `video {renderer: "photo"}`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Look {
    /// Materials: selector -> preset (aluminium, brushed_aluminium, anodised_black, steel, stainless,
    /// zinc, black_powdercoat, rubber, plastic, chrome) or "#rrggbb" paint. Default: guessed from the CAD colour.
    pub materials: Option<BTreeMap<String, String>>,
    /// Path-tracing samples (default 64 for photo, 16 for video; denoised).
    pub samples: Option<u32>,
    /// Ground plane with soft shadows (default true; becomes a shadow catcher over a background image).
    pub floor: Option<bool>,
    /// Lighting: "studio" (default: soft environment, key sun, softbox), "sky" (daylight sky + sun),
    /// or a path to an equirectangular HDRI (.hdr/.exr) that lights the model and shows in reflections.
    pub environment: Option<String>,
    /// Lighting strength multiplier (default 1).
    pub environment_strength: Option<f32>,
    /// Rotate the environment / sun around the vertical axis, degrees.
    pub environment_rotation: Option<f32>,
    /// Show the environment behind the model (default true; false = plain `background` colour).
    pub environment_visible: Option<bool>,
    /// Background colour "#rrggbb" (studio environment tint, or behind the model when the
    /// environment is hidden; default light studio grey).
    pub background: Option<String>,
    /// Backdrop photo (PNG/JPEG) behind the model; the model's shadows land on it.
    pub background_image: Option<String>,
    /// "cover" (crop to fill, default) or "contain" (letterbox).
    pub background_fit: Option<String>,
    /// Exposure in stops (default 0; +1 = twice as bright).
    pub exposure: Option<f32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PhotoParams {
    /// Template for the viewpoint (views, framing, perspective), as in `render`.
    pub template: Option<String>,
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
    /// Look: materials, lighting, background (shared with `video`).
    #[serde(flatten)]
    pub look: Look,
    /// Image size in px (default 1600 x 1000).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Seconds before giving up (default 180; the first run compiles Metal kernels).
    pub timeout_s: Option<u64>,
    /// Also write the PNG to this path.
    pub save: Option<String>,
}

#[derive(Deserialize, JsonSchema, Clone, Default)]
pub struct OrbitSpec {
    /// Camera azimuth at the start / end in degrees (0 = front, 90 = right). Default -30 -> 330 (one full turn).
    pub az_from: Option<f32>,
    pub az_to: Option<f32>,
    /// Elevation above the horizon at the start / end in degrees (default 32 -> 16: a swoop down).
    pub el_from: Option<f32>,
    pub el_to: Option<f32>,
    /// Zoom at the start / end (default 1.45 -> 1.7: a slight dolly in; 1 = whole bounding sphere).
    pub zoom_from: Option<f32>,
    pub zoom_to: Option<f32>,
    /// Point to orbit around [x, y, z] mm (default: centre of the visible parts).
    pub target: Option<[f32; 3]>,
    /// Ease in/out (default true).
    pub ease: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct VideoParams {
    /// Camera path: orbit around the model (a "swoop"). Without a motion, parts stay put.
    pub orbit: Option<OrbitSpec>,
    /// Saved motion to record (see `motion {save_as}`).
    pub run: Option<String>,
    /// Or an inline motion (same fields as the `motion` tool); fields here override `run`.
    pub motion: Option<MotionParams>,
    /// "fast" (GPU CAD look, default, ~ms per frame) or "photo" (Cycles path tracing, seconds per frame).
    pub renderer: Option<String>,
    /// Frames per second (default 30).
    pub fps: Option<u32>,
    /// Duration of the movement in seconds (default 3).
    pub seconds: Option<f32>,
    /// Play forward then back (default false).
    pub pingpong: Option<bool>,
    /// Hold the last frame this many seconds (default 0.5).
    pub hold: Option<f32>,
    /// Frame size (default 1280 x 720; rounded to even).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// View settings (preset, views, highlight, style, section...) for the frames; the camera is
    /// fixed for the whole clip and frames the full movement.
    pub render: Option<RenderParams>,
    /// Burn the exact clearance / interference of each frame into the image (default true).
    pub annotate: Option<bool>,
    /// Look for the photo renderer (materials, environment, background image...); the fast renderer
    /// takes its look from `render`.
    #[serde(flatten)]
    pub look: Look,
    /// "mp4" (H.264 via AVFoundation, default) or "gif".
    pub format: Option<String>,
    /// Output path (default: videos/<motion>.<ext> next to the project file).
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
    /// Save it as a template instead, for any model: "user" (your library,
    /// ~/.config/cad-render-mcp/templates) or "project" (cad-render-templates/ next to the
    /// project file, to share with the model). Templates keep the options, not the cameras.
    pub template: Option<String>,
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
        m = Mat4::from_translation(origin)
            * Mat4::from_axis_angle(axis, (deg * t).to_radians())
            * Mat4::from_translation(-origin);
    }
    if let Some(v) = translate {
        m = Mat4::from_translation(Vec3::from(v) * t) * m;
    }
    m
}

fn resolve_rotate(
    sc: &Scene,
    set: &[usize],
    r: &Option<RotateSpec>,
) -> anyhow::Result<Option<(Vec3, f32, Vec3)>> {
    let Some(r) = r else { return Ok(None) };
    let origin = match r.origin {
        Some(o) => Vec3::from(o),
        None => sc
            .bounds(set)
            .map(|(lo, hi)| (lo + hi) * 0.5)
            .unwrap_or(Vec3::ZERO),
    };
    Ok(Some((axis_vec(&r.axis)?, r.deg, origin)))
}

fn short_path(sc: &Scene, i: usize) -> String {
    let p = &sc.parts[i].path;
    let s = p
        .split_once('/')
        .map(|(_, rest)| rest)
        .filter(|r| !r.is_empty())
        .unwrap_or(p);
    format!("{} {}", sc.parts[i].id, s)
}

fn parse_hex(s: &str) -> anyhow::Result<[f32; 3]> {
    let h = s.trim().trim_start_matches('#');
    anyhow::ensure!(h.len() == 6, "colour must be #rrggbb, got {s:?}");
    let v = u32::from_str_radix(h, 16)?;
    Ok([
        ((v >> 16) & 255) as f32 / 255.0,
        ((v >> 8) & 255) as f32 / 255.0,
        (v & 255) as f32 / 255.0,
    ])
}

const PALETTE: &[[f32; 3]] = &[
    [0.27, 0.51, 0.71],
    [0.89, 0.47, 0.20],
    [0.35, 0.63, 0.35],
    [0.80, 0.30, 0.33],
    [0.55, 0.45, 0.70],
    [0.60, 0.45, 0.35],
    [0.85, 0.55, 0.75],
    [0.50, 0.50, 0.50],
    [0.75, 0.73, 0.30],
    [0.25, 0.70, 0.75],
];
const DEFAULT_PART: [f32; 3] = [0.74, 0.77, 0.81];

/// File colours often come out near-black (unset appearance); lift them so shading stays readable.
fn usable(c: [f32; 3]) -> [f32; 3] {
    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    if l < 0.18 {
        [c[0] * 0.5 + 0.3, c[1] * 0.5 + 0.3, c[2] * 0.5 + 0.32]
    } else {
        c
    }
}

fn tool_err(e: anyhow::Error) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(format!(
        "error: {e:#}"
    ))]))
}

fn dims(lo: Vec3, hi: Vec3) -> String {
    let d = hi - lo;
    format!("{:.1} x {:.1} x {:.1}", d.x, d.y, d.z)
}

impl CadRender {
    pub fn new(app: App) -> Self {
        Self {
            app: Arc::new(Mutex::new(app)),
            tool_router: Self::tool_router(),
        }
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
        let size = if p.empty {
            "(no faces)".to_string()
        } else {
            dims(p.bmin, p.bmax)
        };
        let _ = writeln!(
            s,
            "{} | {} | {} | {} | {}",
            p.id,
            p.path,
            sc.file_label(p.file),
            size,
            p.gpu.tris
        );
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
    let reps = app.scene.load(&files, app.renderer.as_ref())?;
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
        let _ = writeln!(
            s,
            "scene: {} parts, bbox [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}] ({} mm)",
            all.len(),
            lo.x,
            lo.y,
            lo.z,
            hi.x,
            hi.y,
            hi.z,
            dims(lo, hi)
        );
    }
    let _ = writeln!(s, "total {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let _ = writeln!(
        s,
        "project {}:\n{}\n",
        ppath.display(),
        app.project.summary()
    );
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
    pub frame: Option<crate::video::Frame>,
}

/// Motion frames: every tile uses the first view's camera with its own set of part poses.
pub struct Frames {
    pub poses: Vec<Vec<Mat4>>,
    pub labels: Vec<String>,
    /// Return raw RGBA (video frames) instead of a PNG.
    pub raw: bool,
}

/// Merge the template and saved view into the call's parameters (template < saved view < call);
/// returns the saved view's frozen cameras unless the call changes the framing.
fn resolve_preset(
    app: &App,
    p: &RenderParams,
) -> anyhow::Result<(RenderParams, Option<Vec<CamSpec>>, Option<String>)> {
    if p.template.is_none() && p.preset.is_none() {
        return Ok((p.clone(), None, None));
    }
    let mut notes = Vec::new();
    let mut base = serde_json::json!({});
    if let Some(name) = &p.template {
        let t = templates::get(app.project_path.as_deref(), name)?;
        notes.push(format!("template {name:?} ({})", t.source));
        base = t.params;
    }
    let mut cams = None;
    if let Some(name) = &p.preset {
        let sv = app.project.views.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no saved view {name:?}; saved: {}",
                app.project
                    .views
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        base = project::overlay(&base, &sv.params);
        let reframed = p.views.is_some()
            || p.region.is_some()
            || p.focus.is_some()
            || p.zoom.is_some()
            || p.perspective.is_some()
            || p.up.is_some()
            || p.cameras.is_some();
        cams = if reframed { None } else { sv.cameras.clone() };
        notes.push(format!(
            "view {name:?}{}",
            if cams.is_some() {
                " (frozen camera)"
            } else {
                ""
            }
        ));
    }
    let merged: RenderParams =
        serde_json::from_value(project::overlay(&base, &serde_json::to_value(p)?))?;
    Ok((merged, cams, Some(notes.join("; "))))
}

pub fn do_render(app: &mut App, p: &RenderParams) -> anyhow::Result<Rendered> {
    do_render_frames(app, p, None)
}

pub fn do_render_frames(
    app: &mut App,
    p_in: &RenderParams,
    frames: Option<Frames>,
) -> anyhow::Result<Rendered> {
    let t0 = Instant::now();
    let reload = app.refresh()?;
    anyhow::ensure!(
        !app.scene.parts.is_empty(),
        "nothing loaded: call open first"
    );
    let mut notes: Vec<String> = reload.into_iter().collect();
    let (p_owned, preset_cams, preset_note) = resolve_preset(app, p_in)?;
    notes.extend(preset_note);
    let p = &p_owned;
    let backplate = match &p.background_image {
        Some(path) => Some(app.image(path)?),
        None => None,
    };
    let fit_mode = p.background_fit.clone().unwrap_or_else(|| "cover".into());
    anyhow::ensure!(
        fit_mode == "cover" || fit_mode == "contain",
        "background_fit must be cover or contain"
    );
    let sc = &app.scene;
    let up = Up::parse(p.up.as_deref())?;
    let view_specs = p.views.clone().unwrap_or_else(|| vec!["iso".into()]);
    anyhow::ensure!(
        !view_specs.is_empty() && view_specs.len() <= 16,
        "1..16 views"
    );
    let mut views = match p.cameras.clone().or(preset_cams) {
        Some(cams) => cams
            .into_iter()
            .enumerate()
            .map(|(k, c)| {
                let spec = view_specs.get(k);
                let blank = spec.is_some_and(|v| camera::is_blank(v));
                camera::View {
                    label: if blank {
                        String::new()
                    } else {
                        spec.map(|v| v.to_uppercase())
                            .unwrap_or_else(|| format!("VIEW {}", k + 1))
                    },
                    dir: Vec3::from(c.dir),
                    fixed: Some(c),
                    blank,
                    principal: false,
                }
            })
            .collect(),
        None => view_specs
            .iter()
            .map(|v| camera::parse_view(v, up))
            .collect::<anyhow::Result<Vec<_>>>()?,
    };
    if let Some(f) = &frames {
        let base = views
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no view"))?;
        views = f
            .labels
            .iter()
            .map(|l| camera::View {
                label: l.clone(),
                dir: base.dir,
                fixed: base.fixed,
                blank: false,
                principal: base.principal,
            })
            .collect();
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
    anyhow::ensure!(
        ["shaded", "xray", "lines"].contains(&style),
        "style must be shaded, xray or lines"
    );
    let color_by = p.color_by.as_deref().unwrap_or("file");
    let styles: Vec<PartStyle> = sc
        .parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let visible =
                !part.empty && show.as_ref().is_none_or(|s| s.contains(&i)) && !hide.contains(&i);
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
            let ghosted = style == "xray"
                || ghost.contains(&i)
                || highlight.as_ref().is_some_and(|h| !h.contains(&i));
            let edge = if style == "lines" {
                [0.1, 0.1, 0.12]
            } else {
                [color[0] * 0.25, color[1] * 0.25, color[2] * 0.27]
            };
            PartStyle {
                visible,
                ghost: ghosted,
                color,
                edge,
            }
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
                None => sc
                    .bounds(&vis)
                    .map(|(lo, hi)| ((lo + hi) * 0.5).dot(nrm))
                    .unwrap_or(0.0),
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
    let part_labels: Vec<(usize, String)> = if p.part_labels.unwrap_or(false) {
        // Label what the call is about: ghosted parts (highlight/ghost/xray) stay unlabelled
        // unless everything is ghosted.
        let solid: Vec<usize> = vis.iter().copied().filter(|&i| !styles[i].ghost).collect();
        let chosen = if solid.is_empty() { vis.clone() } else { solid };
        chosen
            .iter()
            .map(|&i| (i, sc.parts[i].id.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let labelled: Vec<usize> = part_labels.iter().map(|(i, _)| *i).collect();
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
        region: p.region.map(|b| {
            (
                Vec3::new(b[0], b[1], b[2]).min(Vec3::new(b[3], b[4], b[5])),
                Vec3::new(b[0], b[1], b[2]).max(Vec3::new(b[3], b[4], b[5])),
            )
        }),
        edges,
        silhouettes: p.silhouettes.unwrap_or(true),
        edge_px: p.edge_width.unwrap_or(1.6).clamp(0.5, 8.0),
        section,
        background,
        labels: p.labels.unwrap_or(true),
        scale_bar: p.scale_bar.unwrap_or(true) && !p.perspective.unwrap_or(false),
        same_scale: p.same_scale.unwrap_or(false),
        grid: p.grid.unwrap_or(true),
        part_labels,
        xray: style == "xray",
        unlit: style == "lines",
        raw: frames.as_ref().is_some_and(|f| f.raw),
        backplate: backplate.as_deref().map(|f| (f, fit_mode.clone())),
        poses: match &frames {
            Some(f) => f.poses.clone(),
            None => vec![sc.parts.iter().map(|p| p.pose).collect()],
        },
    };
    let labels_out: Vec<String> = req.views.iter().map(|v| v.label.clone()).collect();
    let styles_out = req.styles.clone();
    let poses_out = req.poses[0].clone();
    let renderer = app
        .renderer
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!(app.renderer_err.clone().unwrap_or_default()))?;
    let out = renderer.render(&req)?;
    notes.extend(out.notes);
    if let Some(path) = &p.save {
        std::fs::write(path, &out.png)?;
        notes.push(format!("saved {path}"));
    }
    if frames.is_none() {
        let mut keep = p.clone();
        keep.preset = None;
        keep.template = None;
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
        names
            .iter()
            .map(|n| if n.is_empty() { "-" } else { n.as_str() })
            .collect::<Vec<_>>()
            .join(", "),
        vis.len(),
        app.scene.parts.len(),
        t0.elapsed().as_secs_f64() * 1e3,
        out.gpu_ms,
        out.encode_ms,
        out.png.len() / 1024
    );
    if let Some((nrm, off, auto)) = section {
        let side = if auto {
            "camera-facing half removed per view"
        } else {
            "+normal side removed"
        };
        let _ = write!(
            text,
            "; section n=[{:.2},{:.2},{:.2}] at {:.1} mm ({side}, cut faces in cap colour)",
            nrm.x, nrm.y, nrm.z, off
        );
    }
    if p.part_labels.unwrap_or(false) {
        // Shorten paths by the common assembly prefix to keep the legend compact.
        let paths: Vec<&str> = labelled
            .iter()
            .map(|&i| app.scene.parts[i].path.as_str())
            .collect();
        let prefix = paths
            .first()
            .and_then(|p| p.rfind('/').map(|k| &p[..=k]))
            .filter(|pre| paths.iter().all(|p| p.starts_with(pre)))
            .unwrap_or("");
        let legend: Vec<String> = labelled
            .iter()
            .map(|&i| {
                format!(
                    "{}={}",
                    app.scene.parts[i].id,
                    &app.scene.parts[i].path[prefix.len()..]
                )
            })
            .collect();
        let _ = write!(text, "\nlabels: {}", legend.join(", "));
    }
    for nn in notes {
        let _ = write!(text, "\nnote: {nn}");
    }
    let frame = out.rgba.map(|rgba| crate::video::Frame {
        w: out.width,
        h: out.height,
        rgba,
    });
    Ok(Rendered {
        png: out.png,
        text,
        cams: out.cams,
        styles: styles_out,
        poses: poses_out,
        frame,
    })
}

fn one(sc: &Scene, s: &str) -> anyhow::Result<usize> {
    let (ix, _) = sc.select(&[s.to_string()]);
    match ix.len() {
        0 => anyhow::bail!("no part matches {s:?}"),
        1 => Ok(ix[0]),
        _ => anyhow::bail!(
            "{s:?} matches {} parts ({}); use an id",
            ix.len(),
            ix.iter()
                .take(8)
                .map(|&i| format!("{} {}", sc.parts[i].id, sc.parts[i].path))
                .collect::<Vec<_>>()
                .join(", ")
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
                overlaps.push((
                    a,
                    b,
                    f64::INFINITY,
                    [
                        lo.x as f64,
                        lo.y as f64,
                        lo.z as f64,
                        hi.x as f64,
                        hi.y as f64,
                        hi.z as f64,
                    ],
                ));
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
            let _ = writeln!(
                s,
                "CHECK FAILED  {} {} <-> {} {} (boolean error)",
                na.id, na.path, nb.id, nb.path
            );
        } else {
            let _ = writeln!(
                s,
                "CLASH {:.1} mm³  {} {} <-> {} {}  overlap box [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}]",
                v, na.id, na.path, nb.id, nb.path, bb[0], bb[1], bb[2], bb[3], bb[4], bb[5]
            );
        }
    }
    for (a, b) in &touching {
        let _ = writeln!(
            s,
            "touch  {} {} <-> {} {}",
            sc.parts[*a].id, sc.parts[*a].path, sc.parts[*b].id, sc.parts[*b].path
        );
    }
    Ok(s)
}

pub fn do_move(app: &mut App, p: &MoveParams) -> anyhow::Result<String> {
    app.refresh()?;
    let mut s = String::new();
    if let Some(name) = &p.pose {
        let pose = app
            .project
            .poses
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no saved pose {name:?}"))?
            .clone();
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
            anyhow::ensure!(
                missing.is_empty(),
                "no part matched: {}",
                missing.join(", ")
            );
            Some(ix)
        }
        None => None,
    };
    if p.reset.unwrap_or(false) {
        let set = sel
            .clone()
            .unwrap_or_else(|| (0..app.scene.parts.len()).collect());
        for &i in &set {
            app.scene.parts[i].pose = Mat4::IDENTITY;
        }
        let _ = writeln!(s, "reset {} parts", set.len());
    }
    if p.translate.is_some() || p.rotate.is_some() {
        let set = sel
            .clone()
            .ok_or_else(|| anyhow::anyhow!("parts required to move"))?;
        let rot = resolve_rotate(&app.scene, &set, &p.rotate)?;
        let m = motion_at(1.0, p.translate, rot);
        for &i in &set {
            let part = &mut app.scene.parts[i];
            part.pose = if p.absolute.unwrap_or(false) {
                m
            } else {
                m * part.pose
            };
        }
        let _ = writeln!(s, "moved {} parts", set.len());
    }
    let posed: Vec<usize> = (0..app.scene.parts.len())
        .filter(|&i| app.scene.parts[i].pose != Mat4::IDENTITY)
        .collect();
    for &i in &posed {
        let (lo, hi) = app.scene.world_box(i);
        let t = app.scene.parts[i].pose.w_axis;
        let _ = writeln!(
            s,
            "  {}: offset [{:.1}, {:.1}, {:.1}], bbox [{:.1}, {:.1}, {:.1}] .. [{:.1}, {:.1}, {:.1}]",
            short_path(&app.scene, i),
            t.x,
            t.y,
            t.z,
            lo.x,
            lo.y,
            lo.z,
            hi.x,
            hi.y,
            hi.z
        );
    }
    let _ = writeln!(s, "{} parts away from their file position", posed.len());
    if let Some(name) = &p.save_pose {
        let set = sel.unwrap_or(posed);
        let parts = set
            .iter()
            .map(|&i| {
                (
                    app.scene.parts[i].path.clone(),
                    app.scene.parts[i].pose.to_cols_array(),
                )
            })
            .collect();
        app.project.poses.insert(
            name.clone(),
            SavedPose {
                parts,
                note: p.note.clone(),
            },
        );
        let _ = writeln!(s, "saved pose {name:?} in {}", app.save_project()?);
    }
    Ok(s)
}

/// A resolved motion: who moves, how, and what to check against.
pub struct MotionSpec {
    pub p: MotionParams,
    pub moving: Vec<usize>,
    pub against: Vec<usize>,
    pub steps: u32,
    pub horizon: f32,
    pub translate: Option<[f32; 3]>,
    pub rot: Option<(Vec3, f32, Vec3)>,
}

impl MotionSpec {
    /// Placement of every part at fraction `t` of the motion.
    pub fn poses_at(&self, sc: &Scene, t: f32) -> Vec<Mat4> {
        let m = motion_at(t, self.translate, self.rot);
        let mut v: Vec<Mat4> = sc.parts.iter().map(|p| p.pose).collect();
        for &i in &self.moving {
            v[i] = m * v[i];
        }
        v
    }
}

pub fn resolve_motion(app: &App, p_in: &MotionParams) -> anyhow::Result<MotionSpec> {
    let p: MotionParams = match &p_in.run {
        Some(name) => {
            let saved = app.project.motions.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "no saved motion {name:?}; saved: {}",
                    app.project
                        .motions
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            serde_json::from_value(project::overlay(saved, &serde_json::to_value(p_in)?))?
        }
        None => p_in.clone(),
    };
    let sc = &app.scene;
    let (moving, missing) = sc.select(
        p.parts
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("parts required"))?,
    );
    anyhow::ensure!(
        missing.is_empty() && !moving.is_empty(),
        "no part matched: {}",
        missing.join(", ")
    );
    anyhow::ensure!(
        p.translate.is_some() || p.rotate.is_some(),
        "give translate and/or rotate"
    );
    let against: Vec<usize> = match &p.against {
        Some(l) => sc.select(l).0,
        None => (0..sc.parts.len()).collect(),
    };
    let against: Vec<usize> = against
        .into_iter()
        .filter(|i| !moving.contains(i) && !sc.parts[*i].empty)
        .collect();
    let rot = resolve_rotate(sc, &moving, &p.rotate)?;
    Ok(MotionSpec {
        steps: p.steps.unwrap_or(10).clamp(1, 200),
        horizon: p.clearance_mm.unwrap_or(50.0).max(0.0),
        translate: p.translate,
        rot,
        moving,
        against,
        p,
    })
}

pub struct StepRes {
    pub t: f32,
    pub min: Option<(f64, usize, usize)>,
    pub clash: f64,
    pub clash_pair: Option<(usize, usize)>,
    pub failed: usize,
    /// Interfering pairs found by point classification (boolean gave no volume).
    pub no_volume: usize,
}

impl StepRes {
    pub fn interfering(&self) -> bool {
        self.clash > 0.0 || self.no_volume > 0
    }
}

/// Exact clearance/interference of the moving parts at fraction `t`. Pairs whose boxes overlap get
/// a boolean check; the rest are measured nearest-box-first and pruned once no box can beat the
/// closest exact distance found so far.
fn check_at(sc: &Scene, spec: &MotionSpec, t: f32) -> (StepRes, usize) {
    let m = motion_at(t, spec.translate, spec.rot);
    let mut r = StepRes {
        t,
        min: None,
        clash: 0.0,
        clash_pair: None,
        failed: 0,
        no_volume: 0,
    };
    let mut checks = 0;
    let mut far: Vec<(f32, usize, usize, Mat4)> = Vec::new();
    for &a in &spec.moving {
        let ma = m * sc.parts[a].pose;
        let (alo, ahi) = sc.posed_box(a, ma);
        for &b in &spec.against {
            let (blo, bhi) = sc.world_box(b);
            let sep = (alo - bhi).max(blo - ahi);
            if sep.max_element() > spec.horizon {
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
            && (d as f32) <= spec.horizon
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
}

/// Check many motion fractions in parallel (each is independent). Returns results and exact checks.
pub fn check_many(sc: &Scene, spec: &MotionSpec, ts: &[f32]) -> (Vec<StepRes>, usize) {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(ts.len())
        .max(1);
    let mut slots: Vec<Option<(StepRes, usize)>> = (0..ts.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|tid| {
                scope.spawn(move || {
                    (tid..ts.len())
                        .step_by(threads)
                        .map(|k| (k, check_at(sc, spec, ts[k])))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            for (k, v) in h.join().expect("motion worker panicked") {
                slots[k] = Some(v);
            }
        }
    });
    let mut checks = 0;
    let res = slots
        .into_iter()
        .map(|v| {
            let (r, c) = v.expect("step computed");
            checks += c;
            r
        })
        .collect();
    (res, checks)
}

pub fn do_motion(app: &mut App, p_in: &MotionParams) -> anyhow::Result<(String, Option<Vec<u8>>)> {
    let t0 = Instant::now();
    app.refresh()?;
    let spec = resolve_motion(app, p_in)?;
    let sc = &app.scene;
    let (steps, horizon, rot, moving) = (spec.steps, spec.horizon, spec.rot, spec.moving.clone());
    let p = spec.p.clone();
    let ts: Vec<f32> = (0..=steps).map(|k| k as f32 / steps as f32).collect();
    let (res, exact_checks) = check_many(sc, &spec, &ts);
    let first_clash = res.iter().position(|r| r.interfering());
    let first_touch = res
        .iter()
        .position(|r| r.min.is_some_and(|(d, _, _)| d < 0.01));
    let tightest = res
        .iter()
        .enumerate()
        .filter_map(|(k, r)| r.min.map(|(d, a, b)| (k, d, a, b)))
        .min_by(|x, y| x.1.total_cmp(&y.1));

    let mut s = String::new();
    let desc = format!(
        "{}{}",
        p.translate
            .map(|v| format!("translate [{:.1}, {:.1}, {:.1}] mm ", v[0], v[1], v[2]))
            .unwrap_or_default(),
        rot.map(|(a, d, o)| format!(
            "rotate {d:.1} deg about [{:.2}, {:.2}, {:.2}] through [{:.1}, {:.1}, {:.1}]",
            a.x, a.y, a.z, o.x, o.y, o.z
        ))
        .unwrap_or_default()
    );
    let _ = writeln!(
        s,
        "motion of {} part(s): {desc}, {} steps, {} exact checks, {:.0} ms",
        moving.len(),
        steps,
        exact_checks,
        t0.elapsed().as_secs_f64() * 1e3
    );
    let _ = writeln!(s, "step |  t   | clearance mm | nearest pair | clash mm³");
    for (k, r) in res.iter().enumerate() {
        let (c, pair) = match r.min {
            Some((d, a, b)) => (
                format!("{d:.2}"),
                format!("{} <-> {}", short_path(sc, a), short_path(sc, b)),
            ),
            None => (format!("> {horizon:.0}"), String::new()),
        };
        let clash = if r.clash > 0.0 || r.no_volume > 0 {
            let mut c = if r.clash > 0.0 {
                format!("{:.1}", r.clash)
            } else {
                String::new()
            };
            if r.no_volume > 0 {
                let _ = write!(
                    c,
                    "{}+{} pair(s) interfering, volume unknown",
                    if c.is_empty() { "" } else { " " },
                    r.no_volume
                );
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
            let _ = writeln!(
                s,
                "FIRST INTERFERENCE at step {k} (t={:.2}): {} <-> {}",
                res[k].t,
                short_path(sc, a),
                short_path(sc, b)
            );
        }
        (None, Some(k)) => {
            let _ = writeln!(
                s,
                "first contact at step {k} (t={:.2}), no interference",
                res[k].t
            );
        }
        _ => {
            let _ = writeln!(s, "no contact during the motion");
        }
    }
    if let Some((k, d, a, b)) = tightest {
        let _ = writeln!(
            s,
            "tightest: {d:.2} mm at step {k} ({} <-> {})",
            short_path(sc, a),
            short_path(sc, b)
        );
    }

    // Contact sheet.
    let nframes = p.frames.unwrap_or(4).min(16) as usize;
    let mut png = None;
    if nframes > 0 {
        let mut idx: Vec<usize> = if nframes == 1 {
            vec![steps as usize]
        } else {
            (0..nframes)
                .map(|f| (f * steps as usize + (nframes - 1) / 2) / (nframes - 1))
                .collect()
        };
        if let Some(k) = first_clash.or(first_touch)
            && !idx.contains(&k)
        {
            let j = idx
                .iter()
                .enumerate()
                .min_by_key(|(_, v)| v.abs_diff(k))
                .map(|(j, _)| j)
                .unwrap_or(0);
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
                    if r.clash > 0.0 {
                        format!("CLASH {:.0} MM3", r.clash)
                    } else {
                        "CLASH".into()
                    }
                } else {
                    r.min
                        .map(|(d, _, _)| format!("{d:.1} MM"))
                        .unwrap_or_else(|| format!(">{horizon:.0} MM"))
                };
                format!("STEP {k}/{steps}  {state}")
            })
            .collect();
        let mut rp = p.render.clone().unwrap_or_default();
        rp.cols = rp.cols.or(Some(if idx.len() <= 4 { 2 } else { 3 }));
        let out = do_render_frames(
            app,
            &rp,
            Some(Frames {
                poses,
                labels,
                raw: false,
            }),
        )?;
        let _ = writeln!(
            s,
            "frames: steps {:?}; {}",
            idx,
            out.text.lines().next().unwrap_or("")
        );
        png = Some(out.png);
    }
    if let Some(name) = &p.save_as {
        let mut keep = p.clone();
        keep.run = None;
        keep.save_as = None;
        app.project.motions.insert(
            name.clone(),
            project::strip_nulls(serde_json::to_value(&keep)?),
        );
        let _ = writeln!(s, "saved motion {name:?} in {}", app.save_project()?);
    }
    Ok((s, png))
}

fn resolve_materials(
    sc: &Scene,
    spec: &Option<BTreeMap<String, String>>,
    notes: &mut Vec<String>,
) -> anyhow::Result<Vec<photo::Material>> {
    let mut materials: Vec<photo::Material> = sc
        .parts
        .iter()
        .map(|part| photo::auto_material(part.file_color.unwrap_or([0.74, 0.77, 0.81])))
        .collect();
    if let Some(ms) = spec {
        for (sel, name) in ms {
            let (ix, missing) = sc.select(std::slice::from_ref(sel));
            if !missing.is_empty() {
                notes.push(format!("no part matched {sel:?}"));
            }
            let m = match photo::material_preset(&name.to_ascii_lowercase()) {
                Some(m) => m,
                None if name.starts_with('#') => photo::Material {
                    color: parse_hex(name)?,
                    metallic: 0.0,
                    roughness: 0.4,
                    anisotropic: 0.0,
                },
                None => anyhow::bail!("unknown material {name:?}"),
            };
            for i in ix {
                materials[i] = m;
            }
        }
    }
    Ok(materials)
}

fn lighting(app: &mut App, look: &Look) -> anyhow::Result<photo::Lighting> {
    let env = match look.environment.as_deref().unwrap_or("studio") {
        "studio" => photo::Env::Studio,
        "sky" => photo::Env::Sky,
        path => photo::Env::File(std::path::PathBuf::from(scene::shellexpand(path))),
    };
    let fit = look
        .background_fit
        .clone()
        .unwrap_or_else(|| "cover".into());
    anyhow::ensure!(
        fit == "cover" || fit == "contain",
        "background_fit must be cover or contain"
    );
    let backplate = match &look.background_image {
        Some(p) => Some((app.image(p)?, fit)),
        None => None,
    };
    Ok(photo::Lighting {
        env,
        strength: look.environment_strength.unwrap_or(1.0).max(0.0),
        rotation_deg: look.environment_rotation.unwrap_or(0.0),
        env_visible: look.environment_visible.unwrap_or(true),
        background: match &look.background {
            Some(h) => parse_hex(h)?,
            None => [0.92, 0.93, 0.95],
        },
        backplate,
        floor: look.floor.unwrap_or(true),
        exposure: look.exposure.unwrap_or(0.0),
    })
}

pub fn do_photo(app: &mut App, p: &PhotoParams) -> anyhow::Result<(Vec<u8>, String)> {
    let t0 = Instant::now();
    let (w, h) = (
        p.width.unwrap_or(1600).clamp(64, 8192),
        p.height.unwrap_or(1000).clamp(64, 8192),
    );
    // Resolve camera, visibility and poses exactly like `render` (a quick GPU pass at the same aspect).
    let scale = (2048.0 / w.max(h) as f32).min(1.0);
    let has_saved_cam = p.preset.as_ref().is_some_and(|n| {
        app.project
            .views
            .get(n)
            .is_some_and(|v| v.cameras.is_some())
    });
    let rp = RenderParams {
        template: p.template.clone(),
        preset: p.preset.clone(),
        views: p.views.clone(),
        cameras: p.cameras.clone(),
        region: p.region,
        focus: p.focus.clone(),
        zoom: p.zoom,
        perspective: p.perspective.or(if has_saved_cam || p.cameras.is_some() {
            None
        } else {
            Some(true)
        }),
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
    let cam = *resolved
        .cams
        .first()
        .ok_or_else(|| anyhow::anyhow!("no camera"))?;
    let light = lighting(app, &p.look)?;
    let sc = &app.scene;
    let mut notes = Vec::new();
    let materials = resolve_materials(sc, &p.look.materials, &mut notes)?;
    let work_dir = std::env::temp_dir().join(format!("cad-render-photo-{}", std::process::id()));
    let out = photo::photo(&photo::PhotoReq {
        scene: sc,
        cam,
        width: w,
        height: h,
        styles: &resolved.styles,
        poses: &resolved.poses,
        materials,
        samples: p.look.samples.unwrap_or(64).clamp(1, 4096),
        light: &light,
        timeout: std::time::Duration::from_secs(p.timeout_s.unwrap_or(180)),
        work_dir,
    })?;
    if let Some(path) = &p.save {
        std::fs::write(path, &out.png)?;
        notes.push(format!("saved {path}"));
    }
    let mut text = format!(
        "{w}x{h} px path-traced (Cycles, Metal), {} samples, {} tris; {:.1} s total (scene export {:.0} ms, render {:.1} s), {} KB",
        p.look.samples.unwrap_or(64),
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

pub fn do_video(app: &mut App, p: &VideoParams) -> anyhow::Result<(Vec<u8>, String)> {
    use crate::video::{self, Frame};
    let t0 = Instant::now();
    app.refresh()?;
    let mut mp = p.motion.clone().unwrap_or_default();
    if mp.run.is_none() {
        mp.run = p.run.clone();
    }
    let has_motion = mp.run.is_some() || mp.parts.is_some();
    anyhow::ensure!(
        has_motion || p.orbit.is_some(),
        "give a motion (run / motion) and/or an orbit"
    );
    let spec = if has_motion {
        Some(resolve_motion(app, &mp)?)
    } else {
        None
    };
    let fps = p.fps.unwrap_or(30).clamp(1, 120);
    let n = ((p.seconds.unwrap_or(3.0).clamp(0.1, 120.0) * fps as f32).round() as usize).max(2);
    let even = |v: u32| (v.clamp(64, 3840) / 2) * 2;
    let (w, h) = (even(p.width.unwrap_or(1280)), even(p.height.unwrap_or(720)));
    let photo_mode = match p.renderer.as_deref().unwrap_or("fast") {
        "fast" => false,
        "photo" => true,
        o => anyhow::bail!("renderer must be fast or photo (got {o:?})"),
    };
    let format = p.format.clone().unwrap_or_else(|| "mp4".into());
    anyhow::ensure!(
        format == "mp4" || format == "gif",
        "format must be mp4 or gif"
    );
    let ts: Vec<f32> = (0..n).map(|k| k as f32 / (n - 1) as f32).collect();

    // Exact checks per distinct frame.
    let annotate = p.annotate.unwrap_or(true);
    let t_checks = Instant::now();
    let checks = match &spec {
        Some(spec) if annotate => Some(check_many(&app.scene, spec, &ts).0),
        _ => None,
    };
    let horizon = spec.as_ref().map(|s| s.horizon).unwrap_or(0.0);
    let checks_ms = t_checks.elapsed().as_secs_f64() * 1e3;
    let label = |k: usize| -> String {
        let base = format!("T={:.2}S", k as f32 / fps as f32);
        match &checks {
            Some(c) if c[k].interfering() => format!("{base}  CLASH"),
            Some(c) => match c[k].min {
                Some((d, _, _)) => format!("{base}  {d:.1} MM"),
                None => format!("{base}  >{horizon:.0} MM"),
            },
            None => base,
        }
    };
    let poses: Vec<Vec<Mat4>> = match &spec {
        Some(spec) => ts.iter().map(|&t| spec.poses_at(&app.scene, t)).collect(),
        None => vec![app.scene.parts.iter().map(|p| p.pose).collect(); n],
    };

    // One fixed camera for the whole clip, framing every frame's pose.
    let mut rp = p.render.clone().unwrap_or_default();
    rp.width = Some(160);
    rp.height = Some(((160 * h) / w).max(16));
    let sample: Vec<usize> = (0..16).map(|k| k * (n - 1) / 15).collect();
    let probe = do_render_frames(
        app,
        &rp,
        Some(Frames {
            poses: sample.iter().map(|&k| poses[k].clone()).collect(),
            labels: sample.iter().map(|_| String::new()).collect(),
            raw: true,
        }),
    )?;
    let mut cams = vec![probe.cams[0]; n];
    if let Some(o) = &p.orbit {
        // Constant-distance perspective orbit around the target, sized to the bounding sphere of
        // everything visible during the clip.
        let vis: Vec<usize> = (0..app.scene.parts.len())
            .filter(|&i| probe.styles[i].visible && !app.scene.parts[i].empty)
            .collect();
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for pose in &poses {
            for &i in &vis {
                let (a, b) = app.scene.posed_box(i, pose[i]);
                lo = lo.min(a);
                hi = hi.max(b);
            }
        }
        let target = o.target.map(Vec3::from).unwrap_or((lo + hi) * 0.5);
        let radius = (hi - lo).length() * 0.5;
        let half_fov = 15f32.to_radians();
        let aspect = w as f32 / h as f32;
        let fit_half = if aspect >= 1.0 {
            half_fov
        } else {
            (half_fov.tan() * aspect).atan()
        };
        let ease = o.ease.unwrap_or(true);
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        for (k, cam) in cams.iter_mut().enumerate() {
            let t = ts[k];
            let te = if ease { t * t * (3.0 - 2.0 * t) } else { t };
            let az = lerp(o.az_from.unwrap_or(-30.0), o.az_to.unwrap_or(330.0), te).to_radians();
            let el = lerp(o.el_from.unwrap_or(32.0), o.el_to.unwrap_or(16.0), te).to_radians();
            let zoom = lerp(o.zoom_from.unwrap_or(1.45), o.zoom_to.unwrap_or(1.7), te).max(0.05);
            let cam_pos_dir = Vec3::new(az.sin() * el.cos(), -az.cos() * el.cos(), el.sin());
            let dir = -cam_pos_dir;
            let right = dir.cross(Vec3::Z).normalize();
            let up = right.cross(dir).normalize();
            let dist = radius / fit_half.sin() / zoom;
            *cam = CamSpec {
                dir: dir.into(),
                up: up.into(),
                target: target.into(),
                half_height: dist * half_fov.tan(),
                distance: Some(dist),
            };
        }
    }
    let cam = cams[0];
    let mut rp = p.render.clone().unwrap_or_default();
    rp.cameras = Some(vec![cam]);
    rp.width = Some(w);
    rp.height = Some(h);
    rp.cols = Some(1);
    if checks.is_none() && rp.labels.is_none() {
        rp.labels = Some(false); // nothing to annotate: clean frames
    }

    let t_frames = Instant::now();
    let mut frames: Vec<Frame> = Vec::with_capacity(n);
    let mut notes = Vec::new();
    if photo_mode {
        rp.labels = Some(false);
        rp.scale_bar = Some(false);
        let resolved = do_render_frames(
            app,
            &rp,
            Some(Frames {
                poses: vec![poses[0].clone()],
                labels: vec![String::new()],
                raw: true,
            }),
        )?;
        let materials = resolve_materials(&app.scene, &p.look.materials, &mut notes)?;
        let light = lighting(app, &p.look)?;
        let work_dir =
            std::env::temp_dir().join(format!("cad-render-video-{}", std::process::id()));
        for (k, pose) in poses.iter().enumerate() {
            let out = photo::photo(&photo::PhotoReq {
                scene: &app.scene,
                cam: cams[k],
                width: w,
                height: h,
                styles: &resolved.styles,
                poses: pose,
                materials: materials.clone(),
                samples: p.look.samples.unwrap_or(16).clamp(1, 4096),
                light: &light,
                timeout: std::time::Duration::from_secs(600),
                work_dir: work_dir.clone(),
            })?;
            let mut f = out.frame;
            if checks.is_some() {
                let mut cv = crate::font::Canvas {
                    px: &mut f.rgba,
                    w: f.w,
                    h: f.h,
                };
                cv.label(
                    12,
                    10,
                    &label(k),
                    2,
                    [40, 44, 52, 255],
                    [255, 255, 255, 190],
                );
            }
            frames.push(f);
        }
    } else {
        for (k, pose) in poses.iter().enumerate() {
            rp.cameras = Some(vec![cams[k]]);
            let out = do_render_frames(
                app,
                &rp,
                Some(Frames {
                    poses: vec![pose.clone()],
                    labels: vec![label(k)],
                    raw: true,
                }),
            )?;
            frames.push(out.frame.ok_or_else(|| anyhow::anyhow!("no frame"))?);
        }
    }
    let frames_ms = t_frames.elapsed().as_secs_f64() * 1e3;

    // Sequence: forward, optional back, hold.
    let mut order: Vec<usize> = (0..n).collect();
    if p.pingpong.unwrap_or(false) {
        order.extend((1..n - 1).rev());
    }
    let hold = (p.hold.unwrap_or(0.5).max(0.0) * fps as f32).round() as usize;
    let last = *order.last().unwrap();
    order.extend(std::iter::repeat_n(last, hold));
    let seq: Vec<Frame> = order
        .iter()
        .map(|&k| Frame {
            w: frames[k].w,
            h: frames[k].h,
            rgba: frames[k].rgba.clone(),
        })
        .collect();

    let path = match &p.save {
        Some(s) => std::path::PathBuf::from(s),
        None => {
            let dir = app
                .project_path
                .as_ref()
                .and_then(|pp| pp.parent().map(|d| d.join("videos")))
                .unwrap_or_else(std::env::temp_dir);
            std::fs::create_dir_all(&dir)?;
            let stem = spec
                .as_ref()
                .and_then(|s| s.p.run.clone())
                .or(p.run.clone())
                .unwrap_or_else(|| {
                    if p.orbit.is_some() {
                        "orbit".into()
                    } else {
                        "motion".into()
                    }
                });
            dir.join(format!("{stem}.{format}"))
        }
    };
    let t_enc = Instant::now();
    if format == "mp4" {
        video::write_mp4(&path, &seq, fps)?;
    } else {
        video::write_gif(&path, &seq, fps)?;
    }
    let enc_ms = t_enc.elapsed().as_secs_f64() * 1e3;

    // Contact sheet of 6 key frames (always including the first interference/contact).
    let mut keys: Vec<usize> = (0..6).map(|k| k * (n - 1) / 5).collect();
    if let Some(c) = &checks
        && let Some(k) = c.iter().position(|r| r.interfering()).or_else(|| {
            c.iter()
                .position(|r| r.min.is_some_and(|(d, _, _)| d < 0.01))
        })
        && !keys.contains(&k)
    {
        let j = keys
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| v.abs_diff(k))
            .map(|(j, _)| j)
            .unwrap_or(0);
        keys[j] = k;
        keys.sort();
    }
    let thumbs: Vec<Frame> = keys
        .iter()
        .map(|&k| video::downscale(&frames[k], 640, 400))
        .collect();
    let sheet = video::encode_png(&video::sheet(&thumbs, 3))?;

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut text = format!(
        "{} {}x{} @ {fps} fps, {} frames ({:.1} s) -> {} ({} KB)\nrenderer {}: frames {:.1} s ({:.0} ms/frame), checks {:.0} ms, encode {:.0} ms, total {:.1} s\nsheet: frames {:?}",
        format.to_uppercase(),
        w,
        h,
        seq.len(),
        seq.len() as f32 / fps as f32,
        path.display(),
        size / 1024,
        if photo_mode {
            "photo (Cycles)"
        } else {
            "fast (GPU)"
        },
        frames_ms / 1e3,
        frames_ms / n as f64,
        checks_ms,
        enc_ms,
        t0.elapsed().as_secs_f64(),
        keys
    );
    if let Some(c) = &checks {
        if let Some(k) = c.iter().position(|r| r.interfering()) {
            let (a, b) = c[k].clash_pair.unwrap_or((0, 0));
            let _ = write!(
                text,
                "\nFIRST INTERFERENCE at frame {k} (t={:.2}): {} <-> {}",
                c[k].t,
                short_path(&app.scene, a),
                short_path(&app.scene, b)
            );
        } else if let Some((k, d)) = c
            .iter()
            .enumerate()
            .filter_map(|(k, r)| r.min.map(|(d, _, _)| (k, d)))
            .min_by(|x, y| x.1.total_cmp(&y.1))
        {
            let _ = write!(text, "\nno interference; tightest {d:.2} mm at frame {k}");
        } else {
            let _ = write!(text, "\nno interference; nothing within {horizon:.0} mm");
        }
    }
    for nn in notes {
        let _ = write!(text, "\nnote: {nn}");
    }
    Ok((sheet, text))
}

// ---------- tools ----------

#[tool_router]
impl CadRender {
    #[tool(
        description = "Load STEP/BREP/IGES files (or directories of them) into the scene. Returns parts with ids, sizes and timings. Loaded files auto-reload when they change on disk.",
        annotations(
            title = "Open CAD files",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn open(
        &self,
        Parameters(p): Parameters<OpenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            Ok(CallToolResult::success(vec![ContentBlock::text(do_open(
                app, &p,
            )?)]))
        })
        .await
    }

    #[tool(
        description = "Render the loaded scene to a PNG returned inline. Several views tile into one image. Supports hide/show/highlight/ghost, colour overrides, section cuts, x-ray, perspective, part labels and a scale bar (orthographic).",
        annotations(
            title = "Render views",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn render(
        &self,
        Parameters(p): Parameters<RenderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let r = do_render(app, &p)?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&r.png);
            Ok(CallToolResult::success(vec![
                ContentBlock::image(b64, "image/png"),
                ContentBlock::text(r.text),
            ]))
        })
        .await
    }

    #[tool(
        description = "List loaded parts (id, assembly path, file, bounding-box size, triangle count).",
        annotations(
            title = "List parts",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn parts(
        &self,
        Parameters(p): Parameters<PartsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            if let Some(n) = app.refresh()? {
                log::info(&n);
            }
            let sc = &app.scene;
            let set: Vec<usize> = match &p.filter {
                Some(f) => sc.select(std::slice::from_ref(f)).0,
                None => (0..sc.parts.len()).collect(),
            };
            Ok(CallToolResult::success(vec![ContentBlock::text(
                parts_table(sc, &set),
            )]))
        })
        .await
    }

    #[tool(
        description = "Exact B-rep properties of parts: volume, area, mass (with density), centroid, bbox, topology counts, face types, cylinder radii (bend radii, holes) and torus radii.",
        annotations(
            title = "Inspect geometry",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn inspect(
        &self,
        Parameters(p): Parameters<InspectParams>,
    ) -> Result<CallToolResult, ErrorData> {
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
                    if let (Some(d), Some(vol)) =
                        (p.density, obj.get("volume_mm3").and_then(|v| v.as_f64()))
                    {
                        obj.insert(
                            "mass_kg".into(),
                            serde_json::json!((vol * d * 1e-6 * 1000.0).round() / 1000.0),
                        );
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

    #[tool(
        description = "Exact minimum distance between two parts (B-rep), with the closest points.",
        annotations(
            title = "Measure distance",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn measure(
        &self,
        Parameters(p): Parameters<MeasureParams>,
    ) -> Result<CallToolResult, ErrorData> {
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

    #[tool(
        description = "Interference check: boolean common volume between every pair of parts whose bounding boxes overlap.",
        annotations(
            title = "Check interference",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn clash(
        &self,
        Parameters(p): Parameters<ClashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            app.refresh()?;
            Ok(CallToolResult::success(vec![ContentBlock::text(do_clash(
                app, &p,
            )?)]))
        })
        .await
    }

    #[tool(
        name = "move",
        description = "Move parts (rigid translate/rotate) or apply/reset/save a named pose. Later render/measure/clash/motion calls use the moved positions. Poses are saved in the project file.",
        annotations(
            title = "Move parts",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn r#move(
        &self,
        Parameters(p): Parameters<MoveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            Ok(CallToolResult::success(vec![ContentBlock::text(do_move(
                app, &p,
            )?)]))
        })
        .await
    }

    #[tool(
        description = "Sweep parts through a motion (translate and/or rotate) in steps; at every step reports exact clearance to nearby parts and any interference, the first contact, and returns a contact sheet of frames. Motions can be saved and re-run by name.",
        annotations(
            title = "Sweep a motion",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn motion(
        &self,
        Parameters(p): Parameters<MotionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (text, png) = do_motion(app, &p)?;
            let mut content = Vec::new();
            if let Some(png) = png {
                content.push(ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(&png),
                    "image/png",
                ));
            }
            content.push(ContentBlock::text(text));
            Ok(CallToolResult::success(content))
        })
        .await
    }

    #[tool(
        description = "Photo-realistic still (path-traced with Cycles on the GPU): physical materials (aluminium, steel, rubber, paint...), soft studio light and a shadowed floor. Takes the same viewpoint options as `render`, incl. saved views and poses. Seconds per image, so use `render` for checks and `photo` for presentation.",
        annotations(
            title = "Photo-realistic render",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn photo(
        &self,
        Parameters(p): Parameters<PhotoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (png, text) = do_photo(app, &p)?;
            Ok(CallToolResult::success(vec![
                ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(&png),
                    "image/png",
                ),
                ContentBlock::text(text),
            ]))
        })
        .await
    }

    #[tool(
        description = "Record a motion (saved via `motion {save_as}` or inline) as an MP4 (hardware H.264) or GIF with a fixed camera. renderer \"fast\" (CAD look, exact clearance burned into every frame) or \"photo\" (Cycles path tracing, seconds per frame). Returns a contact sheet of key frames inline and the video path.",
        annotations(
            title = "Record a video",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn video(
        &self,
        Parameters(p): Parameters<VideoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (png, text) = do_video(app, &p)?;
            Ok(CallToolResult::success(vec![
                ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(&png),
                    "image/png",
                ),
                ContentBlock::text(text),
            ]))
        })
        .await
    }

    #[tool(
        description = "Save the last render (parameters + exact cameras) as a named view in the project file, so `render {preset: name}` reproduces the same point of view in any session. With `template` it goes to the template library instead (options only, for any model: `render {template: name}`).",
        annotations(
            title = "Save viewpoint",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn save_view(
        &self,
        Parameters(p): Parameters<SaveViewParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_app(move |app| {
            let (params, cams) = app
                .last_render
                .clone()
                .ok_or_else(|| anyhow::anyhow!("render something first"))?;
            if let Some(scope) = &p.template {
                let dir = match scope.as_str() {
                    "user" => templates::user_dir()
                        .ok_or_else(|| anyhow::anyhow!("no home folder for the user library"))?,
                    "project" => app
                        .project_path
                        .as_deref()
                        .and_then(std::path::Path::parent)
                        .ok_or_else(|| anyhow::anyhow!("no project yet: call open first"))?
                        .join("cad-render-templates"),
                    o => anyhow::bail!("template must be \"user\" or \"project\", got {o:?}"),
                };
                let path = templates::save(&dir, &p.name, p.note.as_deref().unwrap_or(""), params)?;
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "saved template {:?} in {}; use it on any model with render {{template: {:?}}}",
                    p.name,
                    path.display(),
                    p.name
                ))]));
            }
            let freeze = p.freeze.unwrap_or(true);
            app.project.views.insert(
                p.name.clone(),
                SavedView {
                    params: project::strip_nulls(params),
                    cameras: freeze.then_some(cams),
                    note: p.note.clone(),
                },
            );
            let path = app.save_project()?;
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "saved view {:?}{} in {path}",
                p.name,
                if freeze { " with frozen camera" } else { "" }
            ))]))
        })
        .await
    }

    #[tool(
        description = "List saved views, poses and motions of the project (or delete one), and the render templates (built-in and your own).",
        annotations(
            title = "Saved views, poses, motions",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn saved(
        &self,
        Parameters(p): Parameters<SavedParams>,
    ) -> Result<CallToolResult, ErrorData> {
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
            let _ = writeln!(
                s,
                "project: {}",
                app.project_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or("(none, call open)".into())
            );
            let _ = writeln!(s, "\nviews:");
            for (k, v) in &app.project.views {
                let views = v
                    .params
                    .get("views")
                    .map(|x| x.to_string())
                    .unwrap_or("[\"iso\"]".into());
                let _ = writeln!(
                    s,
                    "  {k}: {views}{}{}",
                    if v.cameras.is_some() { " frozen" } else { "" },
                    v.note
                        .as_ref()
                        .map(|n| format!(" - {n}"))
                        .unwrap_or_default()
                );
            }
            let _ = writeln!(s, "poses:");
            for (k, v) in &app.project.poses {
                let _ = writeln!(
                    s,
                    "  {k}: {} parts{}",
                    v.parts.len(),
                    v.note
                        .as_ref()
                        .map(|n| format!(" - {n}"))
                        .unwrap_or_default()
                );
            }
            let _ = writeln!(s, "motions:");
            for (k, v) in &app.project.motions {
                let _ = writeln!(s, "  {k}: {}", v);
            }
            let (all, errors) = templates::all(app.project_path.as_deref());
            let _ = writeln!(s, "\ntemplates (render {{template: name}}, any model):");
            for t in &all {
                let _ = writeln!(s, "  {}: {} [{}]", t.name, t.description, t.source);
            }
            for e in &errors {
                let _ = writeln!(s, "  failed to load: {e}");
            }
            let _ = writeln!(
                s,
                "template folders: {}",
                templates::dirs(app.project_path.as_deref())
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
        })
        .await
    }

    #[tool(
        description = "Unload everything.",
        annotations(
            title = "Unload everything",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn close(&self) -> Result<CallToolResult, ErrorData> {
        self.with_app(|app| {
            app.scene.clear();
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "scene cleared",
            )]))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CadRender {
    fn get_info(&self) -> ServerConfig {
        let gpu = self
            .app
            .lock()
            .map(|a| {
                a.renderer
                    .as_ref()
                    .map(|r| r.adapter_name.clone())
                    .unwrap_or_else(|| "none".into())
            })
            .unwrap_or_default();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("cad-render-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "cad-render-mcp: look at and check CAD models (STEP/BREP/IGES, OpenCascade {}; GPU: {gpu}). Units mm, Z up, front view looks along +Y.\n\
                 1. `open` the files (a directory works too; `split_solids` = one part per body). Edit the source model and re-export at will: every call reloads changed files.\n\
                 2. Look: `render` returns the image inline in ~10 ms. Ask for several views in ONE call (views [\"iso\",\"front\",\"right\",\"top\"]); use show/hide/highlight/ghost, section, region (close-ups), style xray/lines, part_labels, background_image. Orthographic views carry a scale bar.\n\
                 3. Numbers, never from pixels: `inspect` (volume, mass, bend/hole radii), `measure` (exact min distance), `clash` (interference volumes).\n\
                 4. Movement: `move` places parts (poses, saved by name); `motion` sweeps parts through a translate/rotate and reports clearance per step and the first contact.\n\
                 5. Repeatable views: `save_view` stores the last render incl. exact camera; `render {{preset}}` reproduces it in any session. `saved` lists views/poses/motions (kept in .cad-render.json next to the model). Templates are the same for any model: `render {{template: \"drawing_first_angle\"}}` (ISO first-angle drawing, views at one scale), overview, drawing_third_angle, six_views_*, turnaround, presentation, or the user's own; `save_view {{name, template: \"user\"}}` adds one; `saved` lists them.\n\
                 6. Presentation: `photo` (Cycles path tracing: materials, environment studio/sky/HDRI, backdrop photos with real shadows; seconds) and `video` (MP4/GIF of a motion and/or an orbit swoop, fast or photo frames, exact clearance burned in).",
                crate::occt::occt_version()
            ))
    }
}

mod log {
    pub fn info(s: &str) {
        eprintln!("[cad-render] {s}");
    }
}
