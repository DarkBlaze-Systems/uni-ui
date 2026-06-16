// Flat-colored 2D triangles. Vertices arrive already in logical-pixel space;
// the orthographic projection in `globals` maps them to clip space.

struct Globals {
    // Column-major 4x4 orthographic projection (logical px -> clip space).
    proj: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> globals: Globals;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>, // linear RGBA
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = globals.proj * vec4<f32>(in.pos, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Premultiply for the straight-alpha blend state we configure on the host.
    return in.color;
}
