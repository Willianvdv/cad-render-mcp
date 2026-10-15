//! `photo`: path-traced stills with Cycles (Apache-2.0, Metal on Apple Silicon), run as a
//! separate process. The scene, camera and visibility come from the same resolution as `render`,
//! so a saved view photographs exactly as it renders.

use crate::camera::{self, CamSpec};
use crate::render::PartStyle;
use crate::scene::Scene;
use glam::{Mat4, Vec3};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Locate the Cycles standalone binary: $SHAPESHOT_CYCLES, next to our executable, or the
/// developer build in ~/src/shapeshot-deps/cycles/install.
pub fn cycles_binary() -> anyhow::Result<PathBuf> {
    let mut tried = Vec::new();
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("SHAPESHOT_CYCLES") {
        cands.push(p.into());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        cands.push(dir.join("cycles/cycles"));
    }
    if let Ok(h) = std::env::var("HOME") {
        cands.push(PathBuf::from(h).join("src/shapeshot-deps/cycles/install/cycles"));
    }
    for c in cands {
        if c.is_file() {
            return Ok(c);
        }
        tried.push(c.display().to_string());
    }
    anyhow::bail!("Cycles not found (tried {}); build it (see README) or set SHAPESHOT_CYCLES", tried.join(", "))
}

/// Physical material for a part.
#[derive(Clone, Copy, Debug)]
pub struct Material {
    pub color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub anisotropic: f32,
}

pub fn material_preset(name: &str) -> Option<Material> {
    let m = |c: [f32; 3], metallic, roughness, anisotropic| Material { color: c, metallic, roughness, anisotropic };
    Some(match name {
        "aluminium" | "aluminum" => m([0.91, 0.92, 0.93], 1.0, 0.32, 0.0),
        "brushed_aluminium" | "brushed_aluminum" => m([0.90, 0.91, 0.92], 1.0, 0.38, 0.6),
        "anodised_black" | "anodized_black" => m([0.04, 0.04, 0.045], 1.0, 0.45, 0.0),
        "steel" => m([0.56, 0.57, 0.58], 1.0, 0.35, 0.0),
        "stainless" => m([0.68, 0.68, 0.68], 1.0, 0.22, 0.3),
        "zinc" | "galvanised" | "galvanized" => m([0.70, 0.72, 0.74], 1.0, 0.45, 0.0),
        "black_powdercoat" => m([0.035, 0.035, 0.038], 0.0, 0.55, 0.0),
        "rubber" => m([0.06, 0.06, 0.06], 0.0, 0.8, 0.0),
        "plastic" => m([0.05, 0.05, 0.055], 0.0, 0.45, 0.0),
        "chrome" => m([0.95, 0.95, 0.95], 1.0, 0.04, 0.0),
        _ => return None,
    })
}

/// Guess a material from the CAD colour: light greys look like aluminium, very dark like
/// rubber/plastic, other colours like paint.
pub fn auto_material(c: [f32; 3]) -> Material {
    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let sat = c.iter().cloned().fold(0.0f32, f32::max) - c.iter().cloned().fold(1.0f32, f32::min);
    if sat < 0.06 && l > 0.45 {
        material_preset("aluminium").unwrap()
    } else if sat < 0.06 && l < 0.2 {
        Material { color: [0.06, 0.06, 0.065], metallic: 0.0, roughness: 0.6, anisotropic: 0.0 }
    } else {
        Material { color: c, metallic: 0.0, roughness: 0.4, anisotropic: 0.0 }
    }
}

pub struct PhotoReq<'a> {
    pub scene: &'a Scene,
    pub cam: CamSpec,
    pub width: u32,
    pub height: u32,
    pub styles: &'a [PartStyle],
    pub poses: &'a [Mat4],
    pub materials: Vec<Material>,
    pub samples: u32,
    pub floor: bool,
    pub background: [f32; 3],
    pub timeout: Duration,
    /// Exposure in stops (0 = neutral).
    pub exposure: f32,
    /// Directory for the scene file and output.
    pub work_dir: PathBuf,
}

pub struct PhotoOut {
    pub png: Vec<u8>,
    pub frame: crate::video::Frame,
    pub export_ms: f64,
    pub render_ms: f64,
    pub tris: usize,
}

fn fmt3(s: &mut String, v: [f32; 3]) {
    let _ = write!(s, "{} {} {}", v[0], v[1], v[2]);
}

fn mat_attr(m: Mat4) -> String {
    m.to_cols_array().iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ")
}

pub fn photo(r: &PhotoReq) -> anyhow::Result<PhotoOut> {
    let t0 = Instant::now();
    let sc = r.scene;
    let aspect = r.width as f32 / r.height as f32;
    let vis: Vec<usize> = (0..sc.parts.len()).filter(|&i| r.styles[i].visible && !sc.parts[i].empty).collect();
    anyhow::ensure!(!vis.is_empty(), "no visible parts");
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut scene_pts = Vec::new();
    for &i in &vis {
        let (a, b) = sc.posed_box(i, r.poses[i]);
        lo = lo.min(a);
        hi = hi.max(b);
        scene_pts.push(a);
        scene_pts.push(b);
    }
    let diag = (hi - lo).length().max(1.0);
    let floor_z = lo.z - diag * 0.0005;
    if r.floor {
        // keep the floor in the depth range so the ortho eye sits in front of it
        let c = (lo + hi) * 0.5;
        for (dx, dy) in [(-1.0, -1.0), (1.0, 1.0)] {
            scene_pts.push(Vec3::new(c.x + dx * diag * 0.6, c.y + dy * diag * 0.6, floor_z));
        }
    }
    let f = camera::build(&r.cam, scene_pts.iter(), aspect, r.height);

    let mut x = String::with_capacity(1 << 22);
    x.push_str("<cycles>\n");
    let _ = writeln!(x, "<integrator use_denoise=\"true\" max_bounce=\"8\" />");
    let _ = writeln!(x, "<film exposure=\"1.0\" />");
    // Camera: Cycles camera space is x right, y up, z forward.
    let cam_m = Mat4::from_cols(f.right.extend(0.0), f.up.extend(0.0), f.dir.extend(0.0), f.eye.extend(1.0));
    let hh = r.cam.half_height;
    let _ = writeln!(x, "<transform matrix=\"{}\">", mat_attr(cam_m));
    match r.cam.distance {
        Some(_) => {
            let _ = writeln!(
                x,
                "  <camera width=\"{}\" height=\"{}\" type=\"perspective\" fov=\"{}\" nearclip=\"{}\" farclip=\"{}\" viewplane.left=\"{}\" viewplane.right=\"{}\" viewplane.bottom=\"-1\" viewplane.top=\"1\" />",
                r.width,
                r.height,
                30f32.to_radians(),
                diag * 1e-3,
                diag * 50.0,
                -aspect,
                aspect
            );
        }
        None => {
            let _ = writeln!(
                x,
                "  <camera width=\"{}\" height=\"{}\" type=\"orthograph\" nearclip=\"0\" farclip=\"{}\" viewplane.left=\"{}\" viewplane.right=\"{}\" viewplane.bottom=\"{}\" viewplane.top=\"{}\" />",
                r.width,
                r.height,
                diag * 50.0,
                -hh * aspect,
                hh * aspect,
                -hh,
                hh
            );
        }
    }
    x.push_str("</transform>\n");
    // Studio world: soft even environment plus a key sun from behind the camera's left shoulder.
    let bg = r.background;
    let _ = writeln!(
        x,
        "<background>\n  <background name=\"bg\" color=\"{} {} {}\" strength=\"0.4\" />\n  <connect from=\"bg background\" to=\"output surface\" />\n</background>",
        bg[0], bg[1], bg[2]
    );
    let to_sun = (-f.dir * 0.55 + Vec3::Z * 0.75 - f.right * 0.45).normalize();
    let zs = to_sun;
    let xs = zs.cross(if zs.z.abs() > 0.99 { Vec3::X } else { Vec3::Z }).normalize();
    let ys = zs.cross(xs);
    let sun_m = Mat4::from_cols(xs.extend(0.0), ys.extend(0.0), zs.extend(0.0), glam::Vec4::W);
    let _ = writeln!(
        x,
        "<shader name=\"sun\">\n  <emission name=\"e\" color=\"1 0.97 0.92\" strength=\"1.9\" />\n  <connect from=\"e emission\" to=\"output surface\" />\n</shader>"
    );
    let _ = writeln!(x, "<transform matrix=\"{}\">\n  <state shader=\"sun\">\n    <light light_type=\"sun\" angle=\"0.12\" />\n  </state>\n</transform>", mat_attr(sun_m));

    // Softbox: a big emitter above and slightly in front of the model gives metals highlight bands.
    {
        let c = (lo + hi) * 0.5;
        let pos = c + Vec3::Z * diag * 0.9 - f.dir.with_z(0.0).normalize_or_zero() * diag * 0.35;
        let zl = (pos - c).normalize(); // light emits along -Z of its transform, towards the model
        let xl = zl.cross(if zl.z.abs() > 0.99 { Vec3::X } else { Vec3::Z }).normalize();
        let yl = zl.cross(xl);
        let m = Mat4::from_cols(xl.extend(0.0), yl.extend(0.0), zl.extend(0.0), pos.extend(1.0));
        let _ = writeln!(
            x,
            "<shader name=\"softbox\">\n  <emission name=\"e\" color=\"1 1 1\" strength=\"2.2\" />\n  <connect from=\"e emission\" to=\"output surface\" />\n</shader>"
        );
        let _ = writeln!(
            x,
            "<transform matrix=\"{}\">\n  <state shader=\"softbox\">\n    <light light_type=\"area\" size=\"1\" sizeu=\"{}\" sizev=\"{}\" normalize=\"false\" spread=\"2.2\" />\n  </state>\n</transform>",
            mat_attr(m),
            diag * 0.9,
            diag * 0.45
        );
    }

    // Materials: one shader per distinct material.
    let mut shaders: Vec<(String, Material)> = Vec::new();
    let mut part_shader = vec![0usize; sc.parts.len()];
    for &i in &vis {
        let m = r.materials[i];
        let key = format!("{:?}", (m.color.map(|c| (c * 1000.0) as i32), (m.metallic * 100.0) as i32, (m.roughness * 100.0) as i32, (m.anisotropic * 100.0) as i32));
        let k = match shaders.iter().position(|(n, _)| *n == key) {
            Some(k) => k,
            None => {
                shaders.push((key, m));
                shaders.len() - 1
            }
        };
        part_shader[i] = k;
    }
    for (k, (_, m)) in shaders.iter().enumerate() {
        let _ = writeln!(
            x,
            "<shader name=\"m{k}\">\n  <principled_bsdf name=\"p\" base_color=\"{} {} {}\" metallic=\"{}\" roughness=\"{}\" anisotropic=\"{}\" />\n  <connect from=\"p bsdf\" to=\"output surface\" />\n</shader>",
            m.color[0], m.color[1], m.color[2], m.metallic, m.roughness, m.anisotropic
        );
    }
    if r.floor {
        let _ = writeln!(
            x,
            "<shader name=\"floor\">\n  <principled_bsdf name=\"p\" base_color=\"0.55 0.56 0.58\" metallic=\"0\" roughness=\"0.8\" />\n  <connect from=\"p bsdf\" to=\"output surface\" />\n</shader>"
        );
        let c = (lo + hi) * 0.5;
        let e = diag * 20.0;
        let _ = writeln!(
            x,
            "<state shader=\"floor\">\n  <mesh P=\"{} {} {}  {} {} {}  {} {} {}  {} {} {}\" nverts=\"4\" verts=\"0 1 2 3\" />\n</state>",
            c.x - e, c.y - e, floor_z, c.x + e, c.y - e, floor_z, c.x + e, c.y + e, floor_z, c.x - e, c.y + e, floor_z
        );
    }
    let mut tris = 0;
    for &i in &vis {
        let m = &sc.parts[i].cpu;
        if m.idx.is_empty() {
            continue;
        }
        tris += m.idx.len() / 3;
        let _ = writeln!(x, "<transform matrix=\"{}\">\n<state shader=\"m{}\" interpolation=\"smooth\">", mat_attr(r.poses[i]), part_shader[i]);
        x.push_str("<mesh P=\"");
        for p in &m.pos {
            fmt3(&mut x, *p);
            x.push(' ');
        }
        x.push_str("\" N=\"");
        for n in &m.nrm {
            fmt3(&mut x, *n);
            x.push(' ');
        }
        x.push_str("\" nverts=\"");
        for _ in 0..m.idx.len() / 3 {
            x.push_str("3 ");
        }
        x.push_str("\" verts=\"");
        for v in &m.idx {
            let _ = write!(x, "{v} ");
        }
        x.push_str("\" />\n</state>\n</transform>\n");
    }
    x.push_str("</cycles>\n");
    std::fs::create_dir_all(&r.work_dir)?;
    let xml = r.work_dir.join("scene.xml");
    let out = r.work_dir.join("photo.exr");
    let _ = std::fs::remove_file(&out);
    std::fs::write(&xml, x)?;
    let export_ms = t0.elapsed().as_secs_f64() * 1e3;

    let t1 = Instant::now();
    let bin = cycles_binary()?;
    let log = r.work_dir.join("cycles.log");
    let mut child = std::process::Command::new(&bin)
        .args(["--background", "--device", "METAL", "--samples", &r.samples.to_string()])
        .args(["--width", &r.width.to_string(), "--height", &r.height.to_string(), "--output"])
        .arg(&out)
        .arg(&xml)
        .current_dir(&r.work_dir)
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(r.work_dir.join("cycles.err"))?)
        .stdin(std::process::Stdio::null())
        .spawn()?;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if t1.elapsed() > r.timeout {
            let _ = child.kill();
            anyhow::bail!("Cycles timed out after {:?} (first run compiles Metal kernels; retry, or lower samples)", r.timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let render_ms = t1.elapsed().as_secs_f64() * 1e3;
    let tail = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default().lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let log_tail = format!("{}\n{}", tail(log), tail(r.work_dir.join("cycles.err")));
    anyhow::ensure!(status.success() && out.exists(), "Cycles failed ({status}):\n{log_tail}");
    let t2 = Instant::now();
    let frame = tonemap_exr(&out, r.exposure)?;
    let _ = t2;
    let png = crate::video::encode_png(&frame)?;
    Ok(PhotoOut { png, frame, export_ms, render_ms, tris })
}


/// Linear scene-referred EXR -> display PNG: exposure, filmic shoulder (ACES fit), sRGB encoding.
fn tonemap_exr(path: &std::path::Path, exposure_stops: f32) -> anyhow::Result<crate::video::Frame> {
    use exr::prelude::*;
    let img = read_first_rgba_layer_from_file(
        path,
        |res, _| (vec![[0f32; 4]; res.width() * res.height()], res.width()),
        |(px, w), pos, (r, g, b, a): (f32, f32, f32, f32)| px[pos.y() * *w + pos.x()] = [r, g, b, a],
    )
    .map_err(|e| anyhow::anyhow!("reading Cycles output: {e}"))?;
    let (px, w) = img.layer_data.channel_data.pixels;
    let h = px.len() / w;
    let k = 2f32.powf(exposure_stops);
    let aces = |x: f32| {
        let x = (x * k * 0.8).max(0.0);
        ((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0.0, 1.0)
    };
    let srgb = |c: f32| if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in &px {
        for c in &p[..3] {
            rgba.push((srgb(aces(*c)) * 255.0 + 0.5) as u8);
        }
        rgba.push(255);
    }
    Ok(crate::video::Frame { w: w as u32, h: h as u32, rgba })
}
