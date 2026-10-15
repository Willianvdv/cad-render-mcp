// shapeshot shaders: shaded B-rep faces, screen-space-width feature edges and GPU silhouettes.

struct Camera {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,        // xyz eye position, w = 1 perspective / 0 orthographic
    view_dir: vec4<f32>,   // xyz forward (camera -> scene)
    light: vec4<f32>,      // xyz key light direction (towards light)
    viewport: vec4<f32>,   // tile width px, tile height px, edge width px, silhouette width px
    clip: vec4<f32>,       // section plane: keep points with dot(n, p) <= w
    flags: vec4<f32>,      // x = section enabled, y = unlit (line drawing)
};

struct Part {
    model: mat4x4<f32>,    // rigid placement (pose)
    color: vec4<f32>,      // rgb, a = opacity
    edge: vec4<f32>,       // edge rgb, a = opacity
    cap: vec4<f32>,        // section cap colour
};

@group(0) @binding(0) var<uniform> cam: Camera;
@group(1) @binding(0) var<uniform> part: Part;

fn clipped(p: vec3<f32>) -> bool {
    return cam.flags.x > 0.5 && dot(cam.clip.xyz, p) > cam.clip.w;
}

// ---------- faces ----------

struct MeshOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

@vertex
fn vs_mesh(@location(0) p: vec3<f32>, @location(1) n: vec3<f32>) -> MeshOut {
    var o: MeshOut;
    let w = (part.model * vec4<f32>(p, 1.0)).xyz;
    o.pos = cam.view_proj * vec4<f32>(w, 1.0);
    o.world = w;
    o.normal = (part.model * vec4<f32>(n, 0.0)).xyz;
    return o;
}

@fragment
fn fs_mesh(i: MeshOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    if clipped(i.world) { discard; }
    if !front && cam.flags.x > 0.5 {
        // Looking through the section plane into the solid: draw a flat cap.
        return vec4<f32>(part.cap.rgb, part.color.a);
    }
    if cam.flags.y > 0.5 { return part.color; }
    var n = normalize(i.normal);
    if !front { n = -n; }
    var v = -cam.view_dir.xyz;
    if cam.eye.w > 0.5 { v = normalize(cam.eye.xyz - i.world); }
    if dot(n, v) < 0.0 { n = -n; }
    let l = normalize(cam.light.xyz);
    let fill = normalize(v * 0.6 - l * 0.4 + vec3<f32>(0.0, 0.0, 0.3));
    let diff = max(dot(n, l), 0.0) * 0.55 + max(dot(n, fill), 0.0) * 0.25 + 0.28;
    let h = normalize(l + v);
    let spec = pow(max(dot(n, h), 0.0), 48.0) * 0.22;
    let rgb = part.color.rgb * diff + vec3<f32>(spec);
    return vec4<f32>(min(rgb, vec3<f32>(1.0)), part.color.a);
}

// ---------- edges (instanced screen-space quads) ----------

struct LineOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) world: vec3<f32>,
};

fn line_vertex(p0: vec3<f32>, p1: vec3<f32>, vi: u32, width: f32) -> LineOut {
    // 6 vertices: two triangles spanning the segment, `width` pixels wide, with square caps.
    var corner = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0));
    let c = corner[vi];
    let c0 = cam.view_proj * vec4<f32>(p0, 1.0);
    let c1 = cam.view_proj * vec4<f32>(p1, 1.0);
    let half_px = cam.viewport.xy * 0.5;
    let s0 = c0.xy / c0.w * half_px;
    let s1 = c1.xy / c1.w * half_px;
    var d = s1 - s0;
    let len = length(d);
    if len < 1e-6 { d = vec2<f32>(1.0, 0.0); } else { d = d / len; }
    let nrm = vec2<f32>(-d.y, d.x);
    let hw = width * 0.5;
    var clip_p = c0;
    if c.x > 0.5 { clip_p = c1; }
    let ext = (c.x * 2.0 - 1.0) * hw;  // extend half a width beyond each end
    let off_px = nrm * c.y * hw + d * ext;
    var o: LineOut;
    o.pos = vec4<f32>(clip_p.xy + off_px / half_px * clip_p.w, clip_p.z - 2e-5 * clip_p.w, clip_p.w);
    o.world = mix(p0, p1, c.x);
    return o;
}

@vertex
fn vs_edge(@location(0) p0: vec3<f32>, @location(1) p1: vec3<f32>, @builtin(vertex_index) vi: u32) -> LineOut {
    return line_vertex((part.model * vec4<f32>(p0, 1.0)).xyz, (part.model * vec4<f32>(p1, 1.0)).xyz, vi, cam.viewport.z);
}

@vertex
fn vs_sil(@location(0) p0: vec3<f32>, @location(1) p1: vec3<f32>, @location(2) n1: vec3<f32>,
          @location(3) n2: vec3<f32>, @builtin(vertex_index) vi: u32) -> LineOut {
    let w0 = (part.model * vec4<f32>(p0, 1.0)).xyz;
    let w1 = (part.model * vec4<f32>(p1, 1.0)).xyz;
    var v = -cam.view_dir.xyz;
    if cam.eye.w > 0.5 { v = cam.eye.xyz - w0; }
    let a = dot((part.model * vec4<f32>(n1, 0.0)).xyz, v);
    let b = dot((part.model * vec4<f32>(n2, 0.0)).xyz, v);
    if a * b > 0.0 {
        var o: LineOut;
        o.pos = vec4<f32>(2.0, 2.0, 2.0, 1.0);  // not a silhouette from this view: cull
        o.world = w0;
        return o;
    }
    return line_vertex(w0, w1, vi, cam.viewport.w);
}

@fragment
fn fs_edge(i: LineOut) -> @location(0) vec4<f32> {
    if clipped(i.world) { discard; }
    return part.edge;
}
