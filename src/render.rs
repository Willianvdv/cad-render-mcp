//! Headless wgpu renderer (Metal on macOS): one MSAA pass draws every view tile, then a single
//! readback copies the atlas to CPU memory for annotation and PNG encoding.

use crate::camera::{self, Up, View};
use crate::font::Canvas;
use crate::occt::PartMesh;
use bytemuck::{Pod, Zeroable};
use glam::{Vec3, Vec4};
use std::time::Instant;
use wgpu::util::DeviceExt;

const SAMPLES: u32 = 4;
const COLOR_FMT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FMT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
pub const MAX_DIM: u32 = 8192;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraU {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    view_dir: [f32; 4],
    light: [f32; 4],
    viewport: [f32; 4],
    clip: [f32; 4],
    flags: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PartU {
    color: [f32; 4],
    edge: [f32; 4],
    cap: [f32; 4],
}

/// GPU-resident tessellation of one part.
pub struct GpuMesh {
    vbuf: Option<wgpu::Buffer>,
    ibuf: Option<wgpu::Buffer>,
    nidx: u32,
    ebuf: Option<wgpu::Buffer>,
    n_sharp: u32,
    n_edges: u32,
    sbuf: Option<wgpu::Buffer>,
    n_sil: u32,
    pub tris: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edges {
    All,
    Sharp,
    None,
}

#[derive(Clone, Copy, Debug)]
pub struct PartStyle {
    pub visible: bool,
    pub ghost: bool,
    pub color: [f32; 3],
    pub edge: [f32; 3],
}

pub struct RenderReq<'a> {
    pub views: Vec<View>,
    pub tile_w: u32,
    pub tile_h: u32,
    pub cols: u32,
    pub up: Up,
    pub perspective: bool,
    pub zoom: f32,
    pub meshes: Vec<&'a GpuMesh>,
    pub styles: Vec<PartStyle>,
    /// Bounding boxes (min, max) of each part, same order as `meshes`.
    pub boxes: Vec<(Vec3, Vec3)>,
    /// Label anchor (a surface point near the centre) per part.
    pub anchors: Vec<Vec3>,
    /// Indices of the parts to frame (empty = all visible).
    pub focus: Vec<usize>,
    /// Explicit box (min, max) to frame; overrides `focus`.
    pub region: Option<(Vec3, Vec3)>,
    pub edges: Edges,
    pub silhouettes: bool,
    pub edge_px: f32,
    /// Section plane (normal, offset, auto): keeps dot(n, p) <= offset; with `auto` each view
    /// removes the half facing its camera instead, so the cut face is always visible.
    pub section: Option<(Vec3, f32, bool)>,
    pub background: Option<[f32; 3]>,
    pub labels: bool,
    pub scale_bar: bool,
    pub part_labels: Vec<(usize, String)>,
    pub xray: bool,
    pub unlit: bool,
}

pub struct RenderOut {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub gpu_ms: f64,
    pub encode_ms: f64,
    pub notes: Vec<String>,
}

struct Targets {
    w: u32,
    h: u32,
    msaa: wgpu::TextureView,
    resolve: wgpu::Texture,
    resolve_view: wgpu::TextureView,
    depth: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded_row: u32,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub adapter_name: String,
    cam_bgl: wgpu::BindGroupLayout,
    part_bgl: wgpu::BindGroupLayout,
    mesh_opaque: wgpu::RenderPipeline,
    mesh_ghost: wgpu::RenderPipeline,
    edge_opaque: wgpu::RenderPipeline,
    edge_ghost: wgpu::RenderPipeline,
    sil_opaque: wgpu::RenderPipeline,
    sil_ghost: wgpu::RenderPipeline,
    align: u64,
    targets: Option<Targets>,
}

fn blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

impl Renderer {
    pub fn new() -> anyhow::Result<Renderer> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))?;
        let adapter_name = adapter.get_info().name;
        let limits = wgpu::Limits {
            max_texture_dimension_2d: MAX_DIM,
            max_buffer_size: adapter.limits().max_buffer_size,
            ..wgpu::Limits::default()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("shapeshot"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            ..Default::default()
        }))?;
        let align = device.limits().min_uniform_buffer_offset_alignment as u64;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shapeshot.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let ubo = |size: u64| wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        };
        let cam_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera"),
            entries: &[ubo(std::mem::size_of::<CameraU>() as u64)],
        });
        let part_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("part"),
            entries: &[ubo(std::mem::size_of::<PartU>() as u64)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("layout"),
            bind_group_layouts: &[Some(&cam_bgl), Some(&part_bgl)],
            immediate_size: 0,
        });
        let f3 = |loc: u32, off: u64| wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: off, shader_location: loc };
        let mesh_attrs = [f3(0, 0), f3(1, 12)];
        let edge_attrs = [f3(0, 0), f3(1, 12)];
        let sil_attrs = [f3(0, 0), f3(1, 12), f3(2, 24), f3(3, 36)];
        let mk = |label: &str, vs: &str, fs: &str, buf: wgpu::VertexBufferLayout, ghost: bool, faces: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[Some(buf)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FMT,
                    depth_write_enabled: Some(!ghost),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: if faces {
                        wgpu::DepthBiasState { constant: 2, slope_scale: 1.5, clamp: 0.0 }
                    } else {
                        Default::default()
                    },
                }),
                multisample: wgpu::MultisampleState { count: SAMPLES, mask: !0, alpha_to_coverage_enabled: false },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR_FMT,
                        blend: Some(blend()),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let mesh_buf = || wgpu::VertexBufferLayout { array_stride: 24, step_mode: wgpu::VertexStepMode::Vertex, attributes: &mesh_attrs };
        let edge_buf = || wgpu::VertexBufferLayout { array_stride: 24, step_mode: wgpu::VertexStepMode::Instance, attributes: &edge_attrs };
        let sil_buf = || wgpu::VertexBufferLayout { array_stride: 48, step_mode: wgpu::VertexStepMode::Instance, attributes: &sil_attrs };
        let mesh_opaque = mk("mesh", "vs_mesh", "fs_mesh", mesh_buf(), false, true);
        let mesh_ghost = mk("mesh_ghost", "vs_mesh", "fs_mesh", mesh_buf(), true, true);
        let edge_opaque = mk("edge", "vs_edge", "fs_edge", edge_buf(), false, false);
        let edge_ghost = mk("edge_ghost", "vs_edge", "fs_edge", edge_buf(), true, false);
        let sil_opaque = mk("sil", "vs_sil", "fs_edge", sil_buf(), false, false);
        let sil_ghost = mk("sil_ghost", "vs_sil", "fs_edge", sil_buf(), true, false);
        Ok(Renderer {
            device,
            queue,
            adapter_name,
            cam_bgl,
            part_bgl,
            mesh_opaque,
            mesh_ghost,
            edge_opaque,
            edge_ghost,
            sil_opaque,
            sil_ghost,
            align,
            targets: None,
        })
    }

    pub fn upload(&self, m: &PartMesh) -> GpuMesh {
        let d = &self.device;
        let mk = |label: &str, bytes: &[u8], usage: wgpu::BufferUsages| -> Option<wgpu::Buffer> {
            (!bytes.is_empty()).then(|| d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytes, usage }))
        };
        let mut inter: Vec<[f32; 3]> = Vec::with_capacity(m.pos.len() * 2);
        for (p, n) in m.pos.iter().zip(&m.nrm) {
            inter.push(*p);
            inter.push(*n);
        }
        GpuMesh {
            vbuf: mk("vbuf", bytemuck::cast_slice(&inter), wgpu::BufferUsages::VERTEX),
            ibuf: mk("ibuf", bytemuck::cast_slice(&m.idx), wgpu::BufferUsages::INDEX),
            nidx: m.idx.len() as u32,
            ebuf: mk("ebuf", bytemuck::cast_slice(&m.edges), wgpu::BufferUsages::VERTEX),
            n_sharp: m.sharp_edges as u32,
            n_edges: (m.edges.len() / 2) as u32,
            sbuf: mk("sbuf", bytemuck::cast_slice(&m.sil), wgpu::BufferUsages::VERTEX),
            n_sil: m.sil.len() as u32,
            tris: m.idx.len() / 3,
        }
    }

    fn targets(&mut self, w: u32, h: u32) -> &Targets {
        if self.targets.as_ref().is_none_or(|t| t.w != w || t.h != h) {
            let d = &self.device;
            let tex = |label: &str, fmt, samples, usage| {
                d.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format: fmt,
                    usage,
                    view_formats: &[],
                })
            };
            let msaa = tex("msaa", COLOR_FMT, SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT);
            let resolve = tex("resolve", COLOR_FMT, 1, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
            let depth = tex("depth", DEPTH_FMT, SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT);
            let padded_row = (w * 4).div_ceil(256) * 256;
            let readback = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: (padded_row * h) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            self.targets = Some(Targets {
                w,
                h,
                msaa: msaa.create_view(&Default::default()),
                resolve_view: resolve.create_view(&Default::default()),
                resolve,
                depth: depth.create_view(&Default::default()),
                readback,
                padded_row,
            });
        }
        self.targets.as_ref().unwrap()
    }

    pub fn render(&mut self, r: &RenderReq) -> anyhow::Result<RenderOut> {
        let t0 = Instant::now();
        let n = r.views.len().max(1) as u32;
        let cols = r.cols.clamp(1, n);
        let rows = n.div_ceil(cols);
        let (w, h) = (r.tile_w * cols, r.tile_h * rows);
        anyhow::ensure!(w <= MAX_DIM && h <= MAX_DIM, "image {w}x{h} exceeds {MAX_DIM}px; use smaller tiles");
        let mut notes = Vec::new();

        // What to frame: focus parts, else every visible part.
        let visible: Vec<usize> = (0..r.meshes.len()).filter(|&i| r.styles[i].visible).collect();
        let frame_set: Vec<usize> = if r.focus.is_empty() { visible.clone() } else { r.focus.clone() };
        let corners = |set: &[usize]| -> Vec<Vec3> {
            let mut v = Vec::new();
            for &i in set {
                let (a, b) = r.boxes[i];
                for k in 0..8 {
                    v.push(Vec3::new(
                        if k & 1 == 0 { a.x } else { b.x },
                        if k & 2 == 0 { a.y } else { b.y },
                        if k & 4 == 0 { a.z } else { b.z },
                    ));
                }
            }
            if v.is_empty() {
                v.push(Vec3::splat(-1.0));
                v.push(Vec3::splat(1.0));
            }
            v
        };
        let frame_pts = match r.region {
            Some((a, b)) => (0..8)
                .map(|k| Vec3::new(if k & 1 == 0 { a.x } else { b.x }, if k & 2 == 0 { a.y } else { b.y }, if k & 4 == 0 { a.z } else { b.z }))
                .collect(),
            None => corners(&frame_set),
        };
        let scene_pts = corners(&visible);

        // Uniforms: one camera block per view, one style block per part.
        let a = self.align as usize;
        let cam_stride = std::mem::size_of::<CameraU>().div_ceil(a) * a;
        let part_stride = std::mem::size_of::<PartU>().div_ceil(a) * a;
        let mut cam_bytes = vec![0u8; cam_stride * n as usize];
        let mut fits = Vec::new();
        let aspect = r.tile_w as f32 / r.tile_h as f32;
        for (k, v) in r.views.iter().enumerate() {
            let f = camera::fit(v.dir, r.up, &frame_pts, &scene_pts, aspect, r.zoom, r.perspective, r.tile_h);
            let light = (-f.dir * 0.8 + f.up * 0.45 - f.right * 0.35).normalize();
            let (clip, on) = match r.section {
                Some((nrm, d, auto)) if auto && f.dir.dot(nrm) > 0.0 => ([-nrm.x, -nrm.y, -nrm.z, -d], 1.0),
                Some((nrm, d, _)) => ([nrm.x, nrm.y, nrm.z, d], 1.0),
                None => ([0.0; 4], 0.0),
            };
            let u = CameraU {
                view_proj: f.view_proj.to_cols_array_2d(),
                eye: [f.eye.x, f.eye.y, f.eye.z, if r.perspective { 1.0 } else { 0.0 }],
                view_dir: [f.dir.x, f.dir.y, f.dir.z, 0.0],
                light: [light.x, light.y, light.z, 0.0],
                viewport: [r.tile_w as f32, r.tile_h as f32, r.edge_px, r.edge_px * 1.15],
                clip,
                flags: [on, if r.unlit { 1.0 } else { 0.0 }, 0.0, 0.0],
            };
            cam_bytes[k * cam_stride..k * cam_stride + std::mem::size_of::<CameraU>()].copy_from_slice(bytemuck::bytes_of(&u));
            fits.push(f);
        }
        let mut part_bytes = vec![0u8; part_stride * r.meshes.len().max(1)];
        for (i, s) in r.styles.iter().enumerate() {
            let (fa, ea) = if s.ghost { (if r.xray { 0.22 } else { 0.12 }, 0.28) } else { (1.0, 1.0) };
            let cap = [s.color[0] * 0.55, s.color[1] * 0.45, s.color[2] * 0.45, 1.0];
            let u = PartU { color: [s.color[0], s.color[1], s.color[2], fa], edge: [s.edge[0], s.edge[1], s.edge[2], ea], cap };
            part_bytes[i * part_stride..i * part_stride + std::mem::size_of::<PartU>()].copy_from_slice(bytemuck::bytes_of(&u));
        }
        let d = &self.device;
        let cam_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("cams"), contents: &cam_bytes, usage: wgpu::BufferUsages::UNIFORM });
        let part_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("parts"), contents: &part_bytes, usage: wgpu::BufferUsages::UNIFORM });
        let bind = |layout: &wgpu::BindGroupLayout, buf: &wgpu::Buffer, size: usize| {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: buf, offset: 0, size: wgpu::BufferSize::new(size as u64) }),
                }],
            })
        };
        let cam_bg = bind(&self.cam_bgl, &cam_buf, std::mem::size_of::<CameraU>());
        let part_bg = bind(&self.part_bgl, &part_buf, std::mem::size_of::<PartU>());

        let bg = r.background.unwrap_or([1.0, 1.0, 1.0]);
        let clear = wgpu::Color {
            r: bg[0] as f64,
            g: bg[1] as f64,
            b: bg[2] as f64,
            a: if r.background.is_some() { 1.0 } else { 0.0 },
        };
        // Borrow-split: pipelines are fields of self, targets need &mut self.
        self.targets(w, h);
        let t = self.targets.as_ref().unwrap();
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("views"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &t.msaa,
                    depth_slice: None,
                    resolve_target: Some(&t.resolve_view),
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Discard },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let show_edges = r.edges != Edges::None;
            for k in 0..n {
                let (tx, ty) = ((k % cols) * r.tile_w, (k / cols) * r.tile_h);
                pass.set_viewport(tx as f32, ty as f32, r.tile_w as f32, r.tile_h as f32, 0.0, 1.0);
                pass.set_scissor_rect(tx, ty, r.tile_w, r.tile_h);
                pass.set_bind_group(0, &cam_bg, &[(k as usize * cam_stride) as u32]);
                for ghost_pass in [false, true] {
                    let parts = visible.iter().copied().filter(|&i| r.styles[i].ghost == ghost_pass);
                    let parts: Vec<usize> = parts.collect();
                    pass.set_pipeline(if ghost_pass { &self.mesh_ghost } else { &self.mesh_opaque });
                    for &i in &parts {
                        let m = r.meshes[i];
                        if let (Some(vb), Some(ib)) = (&m.vbuf, &m.ibuf) {
                            pass.set_bind_group(1, &part_bg, &[(i * part_stride) as u32]);
                            pass.set_vertex_buffer(0, vb.slice(..));
                            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                            pass.draw_indexed(0..m.nidx, 0, 0..1);
                        }
                    }
                    if !show_edges {
                        continue;
                    }
                    pass.set_pipeline(if ghost_pass { &self.edge_ghost } else { &self.edge_opaque });
                    for &i in &parts {
                        let m = r.meshes[i];
                        let count = if r.edges == Edges::All && !ghost_pass { m.n_edges } else { m.n_sharp };
                        if let Some(eb) = &m.ebuf
                            && count > 0
                        {
                            pass.set_bind_group(1, &part_bg, &[(i * part_stride) as u32]);
                            pass.set_vertex_buffer(0, eb.slice(..));
                            pass.draw(0..6, 0..count);
                        }
                    }
                    if r.silhouettes && !ghost_pass {
                        pass.set_pipeline(if ghost_pass { &self.sil_ghost } else { &self.sil_opaque });
                        for &i in &parts {
                            let m = r.meshes[i];
                            if let Some(sb) = &m.sbuf {
                                pass.set_bind_group(1, &part_bg, &[(i * part_stride) as u32]);
                                pass.set_vertex_buffer(0, sb.slice(..));
                                pass.draw(0..6, 0..m.n_sil);
                            }
                        }
                    }
                }
            }
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.resolve, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &t.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(t.padded_row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = t.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()??;
        let mut px = vec![0u8; (w * h * 4) as usize];
        {
            let data = slice.get_mapped_range()?;
            for y in 0..h as usize {
                let src = &data[y * t.padded_row as usize..y * t.padded_row as usize + w as usize * 4];
                px[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(src);
            }
        }
        t.readback.unmap();
        let gpu_ms = t0.elapsed().as_secs_f64() * 1e3;

        // Annotations.
        let t1 = Instant::now();
        let mut cv = Canvas { px: &mut px, w, h };
        let scale = if r.tile_h >= 900 { 3 } else { 2 };
        for (k, v) in r.views.iter().enumerate() {
            let k = k as u32;
            let (tx, ty) = ((k % cols) * r.tile_w, (k / cols) * r.tile_h);
            let f = &fits[k as usize];
            if r.labels {
                cv.label(tx as i32 + 6 * scale, ty as i32 + 5 * scale, &v.label, scale, [40, 44, 52, 255], [255, 255, 255, 190]);
            }
            // Part labels at a surface point near each part's centre; greedy placement avoids overlaps.
            let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
            for (i, name) in &r.part_labels {
                let c = f.view_proj * Vec4::from((r.anchors[*i], 1.0));
                if c.w <= 0.0 {
                    continue;
                }
                let sx = tx as f32 + (c.x / c.w * 0.5 + 0.5) * r.tile_w as f32;
                let sy = ty as f32 + (0.5 - c.y / c.w * 0.5) * r.tile_h as f32;
                let (tw, th) = Canvas::text_size(name, 2);
                let (bw, bh) = (tw + 8, th + 8);
                for dy in [0, 1, -1, 2, -2, 3, -3] {
                    let x = (sx as i32 - bw / 2).clamp(tx as i32 + 2, (tx + r.tile_w) as i32 - bw - 2);
                    let y = sy as i32 - bh / 2 + dy * (bh + 2);
                    if y < ty as i32 + 2 || y + bh > (ty + r.tile_h) as i32 - 2 {
                        continue;
                    }
                    if placed.iter().any(|&(px, py, pw, ph)| x < px + pw && px < x + bw && y < py + ph && py < y + bh) {
                        continue;
                    }
                    placed.push((x, y, bw, bh));
                    if dy != 0 {
                        // leader tick from the label towards the anchor
                        let ly = if dy > 0 { y } else { y + bh };
                        let (y0, y1) = if (sy as i32) < ly { (sy as i32, ly) } else { (ly, sy as i32) };
                        cv.rect(sx as i32, y0, 1, y1 - y0, [20, 60, 140, 200]);
                    }
                    cv.rect(sx as i32 - 2, sy as i32 - 2, 5, 5, [20, 60, 140, 230]);
                    cv.label(x + 4, y + 4, name, 2, [255, 255, 255, 255], [20, 60, 140, 215]);
                    break;
                }
            }
            if r.scale_bar
                && let Some(mpp) = f.mm_per_px
            {
                let target_mm = mpp * r.tile_w as f32 * 0.2;
                let p = 10f32.powf(target_mm.log10().floor());
                let nice = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * p).rfind(|v| *v <= target_mm).unwrap_or(p);
                let len_px = (nice / mpp).round() as i32;
                let x0 = tx as i32 + 8 * scale;
                let y0 = (ty + r.tile_h) as i32 - 8 * scale;
                cv.rect(x0 - 3, y0 - 12 * scale - 3, len_px + 6, 12 * scale + 3 * scale + 6, [255, 255, 255, 170]);
                cv.rect(x0, y0, len_px, scale, [30, 30, 30, 255]);
                cv.rect(x0, y0 - 3 * scale, scale, 4 * scale, [30, 30, 30, 255]);
                cv.rect(x0 + len_px - scale, y0 - 3 * scale, scale, 4 * scale, [30, 30, 30, 255]);
                let txt = if nice >= 1.0 { format!("{} MM", nice as i64) } else { format!("{nice} MM") };
                cv.text(x0, y0 - 12 * scale, &txt, scale, [30, 30, 30, 255]);
            }
        }
        if n > 1 {
            // tile separators
            for k in 1..cols {
                cv.rect((k * r.tile_w) as i32, 0, 1, h as i32, [170, 175, 185, 255]);
            }
            for k in 1..rows {
                cv.rect(0, (k * r.tile_h) as i32, w as i32, 1, [170, 175, 185, 255]);
            }
        }
        if visible.is_empty() {
            notes.push("no visible parts".into());
        }
        let png = encode_png(&px, w, h, r.background.is_some())?;
        let encode_ms = t1.elapsed().as_secs_f64() * 1e3;
        Ok(RenderOut { png, width: w, height: h, gpu_ms, encode_ms, notes })
    }
}

fn encode_png(rgba: &[u8], w: u32, h: u32, opaque: bool) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgba.len() / 8);
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        if opaque {
            e.set_color(png::ColorType::Rgb);
            let mut wr = e.write_header()?;
            let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            wr.write_image_data(&rgb)?;
        } else {
            e.set_color(png::ColorType::Rgba);
            let mut wr = e.write_header()?;
            wr.write_image_data(rgba)?;
        }
    }
    Ok(out)
}

