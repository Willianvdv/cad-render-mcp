//! View specs ("iso", "front", "az30el20", "dir:1,-1,0.5") and camera fitting.

use glam::camera::rh::{proj::directx, view::look_at_mat4};
use glam::{Mat4, Vec3};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Up {
    Z,
    Y,
}

impl Up {
    pub fn parse(s: Option<&str>) -> anyhow::Result<Up> {
        match s.unwrap_or("z").to_ascii_lowercase().as_str() {
            "z" | "+z" => Ok(Up::Z),
            "y" | "+y" => Ok(Up::Y),
            o => anyhow::bail!("up must be \"z\" or \"y\", got {o:?}"),
        }
    }
    /// Map a direction expressed in the canonical Z-up frame into model space.
    fn map(self, v: Vec3) -> Vec3 {
        match self {
            Up::Z => v,
            Up::Y => Vec3::new(v.x, v.z, -v.y),
        }
    }
    fn vec(self) -> Vec3 {
        self.map(Vec3::Z)
    }
}

pub struct View {
    pub label: String,
    /// Stored camera to replay exactly instead of fitting.
    pub fixed: Option<CamSpec>,
    /// Viewing direction (camera -> scene), model space.
    pub dir: Vec3,
}

/// Parse one view spec. Named views assume Z-up with "front" looking along +Y (camera at -Y).
pub fn parse_view(spec: &str, up: Up) -> anyhow::Result<View> {
    let s = spec.trim().to_ascii_lowercase();
    // canonical Z-up directions from the camera position towards the scene
    let named = |cam_pos: Vec3| -> Vec3 { -cam_pos.normalize() };
    let d = match s.as_str() {
        "front" => named(Vec3::new(0.0, -1.0, 0.0)),
        "back" | "rear" => named(Vec3::new(0.0, 1.0, 0.0)),
        "left" => named(Vec3::new(-1.0, 0.0, 0.0)),
        "right" => named(Vec3::new(1.0, 0.0, 0.0)),
        "top" => named(Vec3::new(0.0, 0.0, 1.0)),
        "bottom" => named(Vec3::new(0.0, 0.0, -1.0)),
        "iso" | "iso_fr" | "iso_front_right" => named(Vec3::new(1.0, -1.0, 1.0)),
        "iso_fl" | "iso_front_left" => named(Vec3::new(-1.0, -1.0, 1.0)),
        "iso_br" | "iso_back_right" => named(Vec3::new(1.0, 1.0, 1.0)),
        "iso_bl" | "iso_back_left" => named(Vec3::new(-1.0, 1.0, 1.0)),
        "iso_below" => named(Vec3::new(1.0, -1.0, -1.0)),
        _ => {
            if let Some(rest) = s.strip_prefix("dir:") {
                let v: Vec<f32> = rest
                    .split(',')
                    .map(|x| x.trim().parse::<f32>())
                    .collect::<Result<_, _>>()?;
                anyhow::ensure!(
                    v.len() == 3,
                    "dir: needs 3 numbers (camera -> scene, model space)"
                );
                let d = Vec3::new(v[0], v[1], v[2]);
                anyhow::ensure!(d.length() > 0.0, "dir: must be non-zero");
                return Ok(View {
                    label: spec.trim().to_uppercase(),
                    fixed: None,
                    dir: d.normalize(),
                });
            } else if let Some(rest) = s.strip_prefix("az") {
                // az<deg>el<deg>: camera azimuth from front (-Y) towards right (+X), elevation above horizon
                let (a, e) = rest
                    .split_once("el")
                    .ok_or_else(|| anyhow::anyhow!("use az<deg>el<deg>, e.g. az30el20"))?;
                let a = a.parse::<f32>()?.to_radians();
                let e = e.parse::<f32>()?.to_radians();
                let cam_pos = Vec3::new(a.sin() * e.cos(), -a.cos() * e.cos(), e.sin());
                named(cam_pos)
            } else {
                anyhow::bail!(
                    "unknown view {spec:?}: use front/back/left/right/top/bottom/iso/iso_fl/iso_br/iso_bl/iso_below, az<deg>el<deg> or dir:x,y,z"
                )
            }
        }
    };
    Ok(View {
        label: spec.trim().to_uppercase(),
        fixed: None,
        dir: up.map(d).normalize(),
    })
}

/// A resolved camera that can be stored and replayed exactly (a frozen viewpoint).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CamSpec {
    /// Viewing direction (camera -> scene), model space.
    pub dir: [f32; 3],
    /// Screen-up direction, model space.
    pub up: [f32; 3],
    /// Point the camera looks at (centre of the tile), mm.
    pub target: [f32; 3],
    /// Half of the visible height at the target, mm.
    pub half_height: f32,
    /// Eye distance from the target in mm (perspective only).
    pub distance: Option<f32>,
}

pub struct Fitted {
    pub view_proj: Mat4,
    pub eye: Vec3,
    pub dir: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    /// Model units (mm) per pixel, orthographic only.
    pub mm_per_px: Option<f32>,
    pub spec: CamSpec,
}

const FOV_Y_DEG: f32 = 30.0;

/// Fit the camera so `pts` (bbox corners of what should be in frame) fill the tile.
#[allow(clippy::too_many_arguments)]
pub fn fit(
    dir: Vec3,
    up: Up,
    pts: &[Vec3],
    scene_pts: &[Vec3],
    aspect: f32,
    zoom: f32,
    perspective: bool,
    tile_h_px: u32,
) -> Fitted {
    let mut upv = up.vec();
    if dir.cross(upv).length() < 1e-3 {
        // looking straight up/down: use model +Y (or -Z for Y-up) as screen up
        upv = if up == Up::Z { Vec3::Y } else { Vec3::NEG_Z };
    }
    let right = dir.cross(upv).normalize();
    let up_s = right.cross(dir).normalize();
    let (mut rmin, mut rmax, mut umin, mut umax) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    let (mut fdmin, mut fdmax) = (f32::MAX, f32::MIN);
    for p in pts {
        let (r, u, d) = (p.dot(right), p.dot(up_s), p.dot(dir));
        rmin = rmin.min(r);
        rmax = rmax.max(r);
        umin = umin.min(u);
        umax = umax.max(u);
        fdmin = fdmin.min(d);
        fdmax = fdmax.max(d);
    }
    let margin = 1.08 / zoom.max(1e-3);
    let mut hw = ((rmax - rmin) * 0.5).max(1e-3) * margin;
    let mut hh = ((umax - umin) * 0.5).max(1e-3) * margin;
    if hw / hh > aspect {
        hh = hw / aspect
    } else {
        hw = hh * aspect
    }
    let _ = hw;
    let target =
        right * (rmin + rmax) * 0.5 + up_s * (umin + umax) * 0.5 + dir * (fdmin + fdmax) * 0.5;
    let distance = perspective
        .then(|| hh / (FOV_Y_DEG.to_radians() * 0.5).tan() + (fdmax - fdmin).max(1e-3) * 0.5);
    let spec = CamSpec {
        dir: dir.into(),
        up: up_s.into(),
        target: target.into(),
        half_height: hh,
        distance,
    };
    build(&spec, scene_pts.iter().chain(pts), aspect, tile_h_px)
}

/// Build the projection for a (possibly stored) camera; near/far planes follow the current scene.
pub fn build<'a>(
    spec: &CamSpec,
    scene_pts: impl Iterator<Item = &'a Vec3>,
    aspect: f32,
    tile_h_px: u32,
) -> Fitted {
    let dir = Vec3::from(spec.dir).normalize();
    let right = dir.cross(Vec3::from(spec.up)).normalize();
    let up_s = right.cross(dir).normalize();
    let target = Vec3::from(spec.target);
    let hh = spec.half_height.max(1e-3);
    let (mut sdmin, mut sdmax) = (f32::MAX, f32::MIN);
    for p in scene_pts {
        let d = p.dot(dir);
        sdmin = sdmin.min(d);
        sdmax = sdmax.max(d);
    }
    let td = target.dot(dir);
    if sdmin > sdmax {
        sdmin = td - 1.0;
        sdmax = td + 1.0;
    }
    let sdepth = (sdmax - sdmin).max(1e-3);
    if let Some(dist) = spec.distance {
        let eye = target - dir * dist;
        let eye_d = eye.dot(dir);
        let near = (sdmin - eye_d - sdepth * 0.02).max(dist * 0.01);
        let far = (sdmax - eye_d + sdepth * 0.02 + 1.0).max(near + 1.0);
        let view = look_at_mat4(eye, target, up_s);
        let proj = directx::perspective(FOV_Y_DEG.to_radians(), aspect, near, far);
        Fitted {
            view_proj: proj * view,
            eye,
            dir,
            right,
            up: up_s,
            mm_per_px: None,
            spec: *spec,
        }
    } else {
        // eye in front of everything in the scene, looking through `target`
        let back = (td - sdmin).max(0.0) + sdepth * 0.05 + 1.0;
        let eye = target - dir * back;
        let view = look_at_mat4(eye, target, up_s);
        let far = back + (sdmax - td).max(0.0) + sdepth * 0.05 + 2.0;
        let hw = hh * aspect;
        let proj = directx::orthographic(-hw, hw, -hh, hh, 0.0, far);
        Fitted {
            view_proj: proj * view,
            eye,
            dir,
            right,
            up: up_s,
            mm_per_px: Some(2.0 * hh / tile_h_px as f32),
            spec: *spec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_views() {
        let f = parse_view("front", Up::Z).unwrap();
        assert!((f.dir - Vec3::Y).length() < 1e-6);
        let t = parse_view("top", Up::Z).unwrap();
        assert!((t.dir - Vec3::NEG_Z).length() < 1e-6);
        // Y-up: front looks along -Z, top looks down -Y
        assert!((parse_view("front", Up::Y).unwrap().dir - Vec3::NEG_Z).length() < 1e-6);
        assert!((parse_view("top", Up::Y).unwrap().dir - Vec3::NEG_Y).length() < 1e-6);
        // az0el0 == front, az90el0 == right
        assert!((parse_view("az0el0", Up::Z).unwrap().dir - Vec3::Y).length() < 1e-5);
        assert!((parse_view("az90el0", Up::Z).unwrap().dir - Vec3::NEG_X).length() < 1e-5);
        assert!(parse_view("dir:0,0,0", Up::Z).is_err());
        assert!(parse_view("sideways", Up::Z).is_err());
    }

    #[test]
    fn ortho_fit_contains_points() {
        let pts: Vec<Vec3> = (0..8)
            .map(|k| {
                Vec3::new(
                    (k & 1) as f32 * 2000.0,
                    (k >> 1 & 1) as f32 * 1000.0,
                    (k >> 2) as f32 * 300.0,
                )
            })
            .collect();
        for v in ["iso", "front", "top", "az30el20"] {
            let f = fit(
                parse_view(v, Up::Z).unwrap().dir,
                Up::Z,
                &pts,
                &pts,
                4.0 / 3.0,
                1.0,
                false,
                600,
            );
            for p in &pts {
                let c = f.view_proj * p.extend(1.0);
                let n = c.truncate() / c.w;
                assert!(
                    n.x.abs() <= 1.0 && n.y.abs() <= 1.0 && (0.0..=1.0).contains(&n.z),
                    "{v}: {p} -> {n}"
                );
            }
        }
    }

    #[test]
    fn perspective_fit_contains_points() {
        let pts: Vec<Vec3> = (0..8)
            .map(|k| {
                Vec3::new(
                    (k & 1) as f32 * 50.0,
                    (k >> 1 & 1) as f32 * 80.0,
                    (k >> 2) as f32 * 20.0,
                )
            })
            .collect();
        let f = fit(
            parse_view("iso", Up::Z).unwrap().dir,
            Up::Z,
            &pts,
            &pts,
            1.0,
            1.0,
            true,
            600,
        );
        for p in &pts {
            let c = f.view_proj * p.extend(1.0);
            let n = c.truncate() / c.w;
            assert!(
                c.w > 0.0 && n.x.abs() <= 1.0 && n.y.abs() <= 1.0 && (0.0..=1.0).contains(&n.z),
                "{p} -> {n}"
            );
        }
    }
}
