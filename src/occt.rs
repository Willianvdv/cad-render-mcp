//! Safe wrapper over the C++ OCCT shim (`cpp/occt_shim.cpp`).

use std::ffi::{CStr, CString, c_char, c_int};

#[repr(C)]
struct RawDoc {
    _p: [u8; 0],
}

#[repr(C)]
struct PartView {
    name: *const c_char,
    path: *const c_char,
    has_color: bool,
    color: [f32; 3],
    pos: *const f32,
    nrm: *const f32,
    nverts: usize,
    idx: *const u32,
    nidx: usize,
    edges: *const f32,
    edge_smooth: *const u8,
    nedges: usize,
    sil: *const f32,
    nsil: usize,
}

unsafe extern "C" {
    fn ss_doc_load(
        path: *const c_char,
        split_solids: bool,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut RawDoc;
    fn ss_doc_free(d: *mut RawDoc);
    fn ss_doc_part_count(d: *const RawDoc) -> c_int;
    fn ss_doc_mesh(d: *mut RawDoc, lin: f64, ang: f64, err: *mut c_char, errlen: usize) -> c_int;
    fn ss_doc_part(d: *const RawDoc, i: c_int) -> PartView;
    fn ss_doc_release_mesh(d: *mut RawDoc);
    fn ss_part_inspect(d: *const RawDoc, i: c_int) -> *mut c_char;
    fn ss_part_bbox(d: *const RawDoc, i: c_int, out: *mut f64) -> c_int;
    fn ss_distance(
        a: *const RawDoc,
        ia: c_int,
        ma: *const f64,
        b: *const RawDoc,
        ib: c_int,
        mb: *const f64,
        out: *mut f64,
    ) -> c_int;
    fn ss_common(
        a: *const RawDoc,
        ia: c_int,
        ma: *const f64,
        b: *const RawDoc,
        ib: c_int,
        mb: *const f64,
        out: *mut f64,
    ) -> c_int;
    fn ss_points_inside(
        a: *const RawDoc,
        ia: c_int,
        ma: *const f64,
        b: *const RawDoc,
        ib: c_int,
        mb: *const f64,
        max_pts: c_int,
    ) -> c_int;
    fn ss_str_free(s: *mut c_char);
    fn ss_occt_version() -> *const c_char;
    fn ss_write_demo_step(path: *const c_char, err: *mut c_char, errlen: usize) -> c_int;
}

/// Write the built-in demo assembly (public test fixture) as STEP.
pub fn write_demo_step(path: &str) -> anyhow::Result<()> {
    let c = CString::new(path)?;
    let mut e = err_buf();
    let rc = unsafe { ss_write_demo_step(c.as_ptr(), e.as_mut_ptr() as *mut c_char, e.len()) };
    anyhow::ensure!(rc == 0, "{}", err_str(&e));
    Ok(())
}

pub fn occt_version() -> String {
    unsafe {
        CStr::from_ptr(ss_occt_version())
            .to_string_lossy()
            .into_owned()
    }
}

/// A loaded CAD file: its B-rep parts live on the C++ side.
pub struct Doc {
    raw: *mut RawDoc,
}

// OCCT shapes are only modified while the owning scene's mutex is held; distance and
// (non-destructive) boolean queries only read them, so they may run from several threads.
unsafe impl Send for Doc {}
unsafe impl Sync for Doc {}

/// Tessellated part copied out of OCCT.
pub struct PartMesh {
    pub name: String,
    pub path: String,
    pub color: Option<[f32; 3]>,
    pub pos: Vec<[f32; 3]>,
    pub nrm: Vec<[f32; 3]>,
    pub idx: Vec<u32>,
    /// Segment endpoints, sharp edges first, then tangent-continuous ("smooth") edges.
    pub edges: Vec<[f32; 3]>,
    pub sharp_edges: usize,
    /// Silhouette candidates: p0, p1, n1, n2.
    pub sil: Vec<[[f32; 3]; 4]>,
}

fn err_buf() -> Vec<u8> {
    vec![0u8; 1024]
}

fn err_str(b: &[u8]) -> String {
    CStr::from_bytes_until_nul(b)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Doc {
    pub fn load(path: &str, split_solids: bool) -> anyhow::Result<Doc> {
        let c = CString::new(path)?;
        let mut e = err_buf();
        let raw = unsafe {
            ss_doc_load(
                c.as_ptr(),
                split_solids,
                e.as_mut_ptr() as *mut c_char,
                e.len(),
            )
        };
        if raw.is_null() {
            anyhow::bail!("{}: {}", path, err_str(&e));
        }
        Ok(Doc { raw })
    }

    pub fn part_count(&self) -> usize {
        unsafe { ss_doc_part_count(self.raw) as usize }
    }

    /// Tessellate all parts (parallel inside OCCT) and copy the buffers out.
    /// `lin <= 0` picks a deflection relative to the model size.
    pub fn mesh(&mut self, lin: f64, ang: f64) -> anyhow::Result<Vec<PartMesh>> {
        let mut e = err_buf();
        let rc = unsafe { ss_doc_mesh(self.raw, lin, ang, e.as_mut_ptr() as *mut c_char, e.len()) };
        if rc != 0 {
            anyhow::bail!("{}", err_str(&e));
        }
        let mut out = Vec::with_capacity(self.part_count());
        for i in 0..self.part_count() {
            let v = unsafe { ss_doc_part(self.raw, i as c_int) };
            let s3 = |p: *const f32, n: usize| -> Vec<[f32; 3]> {
                if n == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(p as *const [f32; 3], n) }.to_vec()
                }
            };
            let pos = s3(v.pos, v.nverts);
            let nrm = s3(v.nrm, v.nverts);
            let idx = if v.nidx == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(v.idx, v.nidx) }.to_vec()
            };
            let segs = if v.nedges == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(v.edges as *const [[f32; 3]; 2], v.nedges) }
            };
            let smooth = if v.nedges == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(v.edge_smooth, v.nedges) }
            };
            let mut edges = Vec::with_capacity(v.nedges * 2);
            for (s, &f) in segs.iter().zip(smooth) {
                if f == 0 {
                    edges.extend_from_slice(s);
                }
            }
            let sharp_edges = edges.len() / 2;
            for (s, &f) in segs.iter().zip(smooth) {
                if f != 0 {
                    edges.extend_from_slice(s);
                }
            }
            let sil = if v.nsil == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(v.sil as *const [[f32; 3]; 4], v.nsil) }
                    .to_vec()
            };
            out.push(PartMesh {
                name: unsafe { CStr::from_ptr(v.name) }
                    .to_string_lossy()
                    .into_owned(),
                path: unsafe { CStr::from_ptr(v.path) }
                    .to_string_lossy()
                    .into_owned(),
                color: v.has_color.then_some(v.color),
                pos,
                nrm,
                idx,
                edges,
                sharp_edges,
                sil,
            });
        }
        unsafe { ss_doc_release_mesh(self.raw) };
        Ok(out)
    }

    pub fn inspect(&self, i: usize) -> serde_json::Value {
        unsafe {
            let p = ss_part_inspect(self.raw, i as c_int);
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            ss_str_free(p);
            serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({ "error": s }))
        }
    }

    pub fn bbox(&self, i: usize) -> Option<[f64; 6]> {
        let mut b = [0f64; 6];
        (unsafe { ss_part_bbox(self.raw, i as c_int, b.as_mut_ptr()) } == 0).then_some(b)
    }
}

/// Rigid placement as a 3x4 row-major matrix (None = as loaded).
pub type Placement = Option<[f64; 12]>;

pub fn placement(m: glam::Mat4) -> Placement {
    if m == glam::Mat4::IDENTITY {
        return None;
    }
    let c = m.to_cols_array_2d();
    Some([
        c[0][0] as f64,
        c[1][0] as f64,
        c[2][0] as f64,
        c[3][0] as f64,
        c[0][1] as f64,
        c[1][1] as f64,
        c[2][1] as f64,
        c[3][1] as f64,
        c[0][2] as f64,
        c[1][2] as f64,
        c[2][2] as f64,
        c[3][2] as f64,
    ])
}

fn mptr(m: &Placement) -> *const f64 {
    m.as_ref().map_or(std::ptr::null(), |a| a.as_ptr())
}

/// Minimum distance between two (placed) parts: (distance, point on a, point on b).
pub fn distance(
    a: &Doc,
    ia: usize,
    ma: Placement,
    b: &Doc,
    ib: usize,
    mb: Placement,
) -> Option<(f64, [f64; 3], [f64; 3])> {
    let mut o = [0f64; 7];
    (unsafe {
        ss_distance(
            a.raw,
            ia as c_int,
            mptr(&ma),
            b.raw,
            ib as c_int,
            mptr(&mb),
            o.as_mut_ptr(),
        )
    } == 0)
        .then(|| (o[0], [o[1], o[2], o[3]], [o[4], o[5], o[6]]))
}

/// Boolean common of two (placed) parts: (volume, bbox of the overlap).
pub fn common(
    a: &Doc,
    ia: usize,
    ma: Placement,
    b: &Doc,
    ib: usize,
    mb: Placement,
) -> Option<(f64, [f64; 6])> {
    let mut o = [0f64; 7];
    (unsafe {
        ss_common(
            a.raw,
            ia as c_int,
            mptr(&ma),
            b.raw,
            ib as c_int,
            mptr(&mb),
            o.as_mut_ptr(),
        )
    } == 0)
        .then(|| (o[0], [o[1], o[2], o[3], o[4], o[5], o[6]]))
}

/// Number of sampled surface points of `a` strictly inside solid `b` (None = failed).
pub fn points_inside(
    a: &Doc,
    ia: usize,
    ma: Placement,
    b: &Doc,
    ib: usize,
    mb: Placement,
    max_pts: i32,
) -> Option<usize> {
    let n = unsafe {
        ss_points_inside(
            a.raw,
            ia as c_int,
            mptr(&ma),
            b.raw,
            ib as c_int,
            mptr(&mb),
            max_pts,
        )
    };
    (n >= 0).then_some(n as usize)
}

impl Drop for Doc {
    fn drop(&mut self) {
        unsafe { ss_doc_free(self.raw) }
    }
}
