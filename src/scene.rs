//! Loaded files and parts, part selectors, auto-reload on file change.

use crate::occt::{self, Doc};
use crate::render::{GpuMesh, Renderer};
use glam::Vec3;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

pub const CAD_EXTS: &[&str] = &["step", "stp", "brep", "brp", "igs", "iges"];

pub struct Part {
    pub id: String,
    pub name: String,
    pub path: String,
    pub file: usize,
    pub local: usize,
    /// Colour from the file (sRGB), if any.
    pub file_color: Option<[f32; 3]>,
    pub bmin: Vec3,
    pub bmax: Vec3,
    /// Surface point closest to the bbox centre (for labels).
    pub anchor: Vec3,
    pub gpu: GpuMesh,
    pub cpu: std::sync::Arc<CpuMesh>,
    pub empty: bool,
    /// Current rigid placement (from `move`), applied on top of the file's own placement.
    pub pose: glam::Mat4,
}

pub struct File {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    pub doc: Doc,
    pub load_ms: f64,
    pub mesh_ms: f64,
    pub upload_ms: f64,
    pub tris: usize,
}

#[derive(Default)]
pub struct Scene {
    pub files: Vec<File>,
    pub parts: Vec<Part>,
    next_id: usize,
    pub lin_defl: f64,
    pub ang_defl: f64,
    pub split_solids: bool,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Expand directories into the CAD files they contain (non-recursive), sorted.
pub fn expand_paths(paths: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        let p = PathBuf::from(shellexpand(p));
        if p.is_dir() {
            let mut v: Vec<PathBuf> = std::fs::read_dir(&p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| CAD_EXTS.contains(&e.to_ascii_lowercase().as_str())))
                .collect();
            v.sort();
            anyhow::ensure!(!v.is_empty(), "{} contains no STEP/BREP/IGES files", p.display());
            out.extend(v);
        } else {
            anyhow::ensure!(p.exists(), "{} does not exist", p.display());
            out.push(p);
        }
    }
    Ok(out)
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(h) = std::env::var("HOME")
    {
        return format!("{h}/{rest}");
    }
    p.to_string()
}

/// CPU copy of a part's triangles (for exporters such as `photo`).
pub struct CpuMesh {
    pub pos: Vec<[f32; 3]>,
    pub nrm: Vec<[f32; 3]>,
    pub idx: Vec<u32>,
}

struct Built {
    cpu: std::sync::Arc<CpuMesh>,
    name: String,
    path: String,
    color: Option<[f32; 3]>,
    lo: Vec3,
    hi: Vec3,
    anchor: Vec3,
    gpu: GpuMesh,
    empty: bool,
}

pub struct LoadReport {
    pub file: String,
    pub parts: usize,
    pub tris: usize,
    pub load_ms: f64,
    pub mesh_ms: f64,
    pub upload_ms: f64,
}

impl Scene {
    pub fn new() -> Scene {
        Scene { lin_defl: 0.0, ang_defl: 0.3, ..Default::default() }
    }

    pub fn clear(&mut self) {
        self.parts.clear();
        self.files.clear();
    }

    fn build_file(&self, path: &Path, r: &Renderer) -> anyhow::Result<(File, Vec<Built>)> {
        let t0 = Instant::now();
        let mut doc = Doc::load(&path.to_string_lossy(), self.split_solids)?;
        let load_ms = t0.elapsed().as_secs_f64() * 1e3;
        let t1 = Instant::now();
        let meshes = doc.mesh(self.lin_defl, self.ang_defl)?;
        let mesh_ms = t1.elapsed().as_secs_f64() * 1e3;
        let t2 = Instant::now();
        let mut parts = Vec::with_capacity(meshes.len());
        let mut tris = 0;
        for (k, m) in meshes.iter().enumerate() {
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for p in &m.pos {
                let v = Vec3::from(*p);
                lo = lo.min(v);
                hi = hi.max(v);
            }
            for p in &m.edges {
                let v = Vec3::from(*p);
                lo = lo.min(v);
                hi = hi.max(v);
            }
            let empty = m.pos.is_empty() && m.edges.is_empty();
            if empty {
                if let Some(b) = doc.bbox(k) {
                    lo = Vec3::new(b[0] as f32, b[1] as f32, b[2] as f32);
                    hi = Vec3::new(b[3] as f32, b[4] as f32, b[5] as f32);
                } else {
                    lo = Vec3::ZERO;
                    hi = Vec3::ZERO;
                }
            }
            let mid = (lo + hi) * 0.5;
            let anchor = m.pos.iter().map(|p| Vec3::from(*p)).min_by(|a, b| a.distance_squared(mid).total_cmp(&b.distance_squared(mid))).unwrap_or(mid);
            tris += m.idx.len() / 3;
            parts.push(Built { cpu: std::sync::Arc::new(CpuMesh { pos: m.pos.clone(), nrm: m.nrm.clone(), idx: m.idx.clone() }), name: m.name.clone(), path: m.path.clone(), color: m.color, lo, hi, anchor, gpu: r.upload(m), empty });
        }
        let upload_ms = t2.elapsed().as_secs_f64() * 1e3;
        Ok((File { path: path.to_path_buf(), mtime: mtime(path), doc, load_ms, mesh_ms, upload_ms, tris }, parts))
    }

    /// Load files into the scene (replacing a file that is already loaded).
    pub fn load(&mut self, paths: &[PathBuf], r: &Renderer) -> anyhow::Result<Vec<LoadReport>> {
        let mut reps = Vec::new();
        for p in paths {
            let p = std::fs::canonicalize(p).unwrap_or(p.clone());
            let existing = self.files.iter().position(|f| f.path == p);
            reps.push(self.load_one(&p, existing, r)?);
        }
        Ok(reps)
    }

    fn load_one(&mut self, path: &Path, slot: Option<usize>, r: &Renderer) -> anyhow::Result<LoadReport> {
        let (file, built) = self.build_file(path, r)?;
        let rep = LoadReport {
            file: path.display().to_string(),
            parts: built.len(),
            tris: file.tris,
            load_ms: file.load_ms,
            mesh_ms: file.mesh_ms,
            upload_ms: file.upload_ms,
        };
        let fi = match slot {
            Some(i) => {
                self.files[i] = file;
                i
            }
            None => {
                self.files.push(file);
                self.files.len() - 1
            }
        };
        // Keep part ids stable across reloads: reuse ids by (assembly path, occurrence order).
        let old: Vec<(String, String, glam::Mat4)> = self.parts.iter().filter(|p| p.file == fi).map(|p| (p.path.clone(), p.id.clone(), p.pose)).collect();
        let insert_at = self.parts.iter().position(|p| p.file == fi).unwrap_or(self.parts.len());
        self.parts.retain(|p| p.file != fi);
        let mut used = vec![false; old.len()];
        let mut new_parts = Vec::with_capacity(built.len());
        for (local, b) in built.into_iter().enumerate() {
            let reuse = old.iter().enumerate().position(|(k, (op, _, _))| !used[k] && *op == b.path);
            let (id, pose) = match reuse {
                Some(k) => {
                    used[k] = true;
                    (old[k].1.clone(), old[k].2)
                }
                None => {
                    self.next_id += 1;
                    (format!("p{}", self.next_id), glam::Mat4::IDENTITY)
                }
            };
            new_parts.push(Part { id, name: b.name, path: b.path, file: fi, local, file_color: b.color, bmin: b.lo, bmax: b.hi, anchor: b.anchor, gpu: b.gpu, cpu: b.cpu, empty: b.empty, pose });
        }
        let insert_at = insert_at.min(self.parts.len());
        self.parts.splice(insert_at..insert_at, new_parts);
        Ok(rep)
    }

    /// Reload files whose modification time changed. Returns the reloaded file names.
    pub fn refresh(&mut self, r: &Renderer) -> anyhow::Result<Vec<String>> {
        let mut changed = Vec::new();
        for i in 0..self.files.len() {
            let m = mtime(&self.files[i].path);
            if m.is_some() && m != self.files[i].mtime {
                let p = self.files[i].path.clone();
                self.load_one(&p, Some(i), r)?;
                changed.push(p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
            }
        }
        Ok(changed)
    }

    pub fn file_label(&self, fi: usize) -> String {
        self.files[fi].path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Resolve selectors to part indices. A selector is a part id ("p3"), an exact name or assembly
    /// path, a case-insensitive glob with `*`/`?`, or a case-insensitive substring of the path or
    /// file name. Returns (indices, selectors that matched nothing).
    pub fn select(&self, sels: &[String]) -> (Vec<usize>, Vec<String>) {
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for s in sels {
            let sl = s.to_lowercase();
            let mut hit: Vec<usize> = self.parts.iter().enumerate().filter(|(_, p)| p.id == *s || p.name == *s || p.path == *s).map(|(i, _)| i).collect();
            if hit.is_empty() {
                let is_glob = sl.contains('*') || sl.contains('?');
                hit = self
                    .parts
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        let name = p.name.to_lowercase();
                        let path = p.path.to_lowercase();
                        let file = self.file_label(p.file).to_lowercase();
                        if is_glob {
                            glob(&sl, &name) || glob(&sl, &path) || glob(&sl, &file)
                        } else {
                            path.contains(&sl) || file.contains(&sl)
                        }
                    })
                    .map(|(i, _)| i)
                    .collect();
            }
            if hit.is_empty() {
                missing.push(s.clone());
            }
            for i in hit {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        }
        (out, missing)
    }

    /// Axis-aligned box of part `i` placed with `m`.
    pub fn posed_box(&self, i: usize, m: glam::Mat4) -> (Vec3, Vec3) {
        let p = &self.parts[i];
        let (a, b) = (p.bmin, p.bmax);
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for k in 0..8 {
            let c = Vec3::new(if k & 1 == 0 { a.x } else { b.x }, if k & 2 == 0 { a.y } else { b.y }, if k & 4 == 0 { a.z } else { b.z });
            let w = m.transform_point3(c);
            lo = lo.min(w);
            hi = hi.max(w);
        }
        (lo, hi)
    }

    /// Box of the part at its current pose.
    pub fn world_box(&self, i: usize) -> (Vec3, Vec3) {
        self.posed_box(i, self.parts[i].pose)
    }

    pub fn bounds(&self, set: &[usize]) -> Option<(Vec3, Vec3)> {
        let mut it = set.iter().copied().filter(|&i| !self.parts[i].empty).map(|i| self.world_box(i));
        let (mut lo, mut hi) = it.next()?;
        for (a, b) in it {
            lo = lo.min(a);
            hi = hi.max(b);
        }
        Some((lo, hi))
    }

    /// Exact distance with explicit placements (use `parts[i].pose` for the current pose).
    pub fn distance_at(&self, a: usize, ma: glam::Mat4, b: usize, mb: glam::Mat4) -> Option<(f64, [f64; 3], [f64; 3])> {
        let (pa, pb) = (&self.parts[a], &self.parts[b]);
        occt::distance(&self.files[pa.file].doc, pa.local, occt::placement(ma), &self.files[pb.file].doc, pb.local, occt::placement(mb))
    }

    pub fn common_at(&self, a: usize, ma: glam::Mat4, b: usize, mb: glam::Mat4) -> Option<(f64, [f64; 6])> {
        let (pa, pb) = (&self.parts[a], &self.parts[b]);
        occt::common(&self.files[pa.file].doc, pa.local, occt::placement(ma), &self.files[pb.file].doc, pb.local, occt::placement(mb))
    }

    /// Point-classification interference test, both ways (fallback for failed booleans).
    pub fn interferes_at(&self, a: usize, ma: glam::Mat4, b: usize, mb: glam::Mat4) -> bool {
        let (pa, pb) = (&self.parts[a], &self.parts[b]);
        let (da, db) = (&self.files[pa.file].doc, &self.files[pb.file].doc);
        let (xa, xb) = (occt::placement(ma), occt::placement(mb));
        occt::points_inside(da, pa.local, xa, db, pb.local, xb, 3000).unwrap_or(0) > 0
            || occt::points_inside(db, pb.local, xb, da, pa.local, xa, 3000).unwrap_or(0) > 0
    }

    pub fn distance(&self, a: usize, b: usize) -> Option<(f64, [f64; 3], [f64; 3])> {
        self.distance_at(a, self.parts[a].pose, b, self.parts[b].pose)
    }

    pub fn common(&self, a: usize, b: usize) -> Option<(f64, [f64; 6])> {
        self.common_at(a, self.parts[a].pose, b, self.parts[b].pose)
    }

    pub fn inspect(&self, i: usize) -> serde_json::Value {
        let p = &self.parts[i];
        self.files[p.file].doc.inspect(p.local)
    }
}

/// Minimal glob: `*` any run, `?` one char.
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::glob;
    #[test]
    fn globbing() {
        assert!(glob("bar_*", "bar_isb_40x80 (5):3"));
        assert!(glob("*plate*", "front_plate:1"));
        assert!(glob("p?", "p3"));
        assert!(!glob("plate_l", "plate_r"));
    }
}
