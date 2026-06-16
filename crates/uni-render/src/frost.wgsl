// Frosted-glass render-graph shaders.
//
// Three pipelines share this module:
//
//   * `vs_fullscreen` — a vertex shader that emits a full-screen triangle from
//     `@builtin(vertex_index)` (no vertex buffer needed). Used by both blur
//     passes and the final blit.
//
//   * `fs_blur` — a separable Gaussian blur. One invocation does a single
//     direction; the host runs it twice (horizontal then vertical) with a
//     `BlurParams.direction` of (1,0) then (0,1). Samples are taken in texel
//     space so the same kernel works at any blur target resolution.
//
//   * `fs_blit` — a straight copy (used to upscale the blurred half-res
//     backdrop back to full res, and as the final scene -> surface blit).
//
//   * `fs_frost` — composites one frosted panel: it samples the BLURRED
//     backdrop, clips to a rounded rect (signed-distance), lays a translucent
//     tint on top, and adds a subtle 1px light inner edge. Drawn as a
//     full-screen triangle but discards everything outside the panel rect.

struct BlurParams {
    // 1.0 / texture_size, in texels — so `direction * texel_size` steps one px.
    texel_size: vec2<f32>,
    // (1,0) for the horizontal pass, (0,1) for the vertical pass.
    direction: vec2<f32>,
    // Gaussian standard deviation, in *blur-target* pixels.
    sigma: f32,
    // Half-width of the kernel in taps (radius). Clamped on the host to MAX.
    radius: i32,
    _pad: vec2<f32>,
};

struct FrostParams {
    // Panel rect in blur-target/backdrop UV-pixel space (matches the sampled tex).
    rect_min: vec2<f32>,
    rect_max: vec2<f32>,
    // Full target (surface) resolution in pixels — fragment coords are here.
    resolution: vec2<f32>,
    // Backdrop texture resolution in pixels (blurred tex may be downsampled).
    backdrop_res: vec2<f32>,
    // Tint color, straight-alpha *linear* RGBA.
    tint: vec4<f32>,
    corner_radius: f32,
    // Light inner-edge strength 0..1.
    edge: f32,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;

// blur uses group(1); frost uses group(1) too (different layout) — bound per pipeline.
@group(1) @binding(0) var<uniform> blur: BlurParams;

struct FsOut {
    @location(0) color: vec4<f32>,
};

struct VsFull {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle: 3 verts covering clip space, UVs in [0,1] (y-down).
@vertex
fn vs_fullscreen(@builtin(vertex_index) vid: u32) -> VsFull {
    var out: VsFull;
    // (0,0),(2,0),(0,2) in UV -> clip covers the screen with one triangle.
    let uv = vec2<f32>(
        f32((vid << 1u) & 2u),
        f32(vid & 2u),
    );
    out.uv = uv;
    // map uv [0,1] -> clip [-1,1], flip y so uv.y=0 is top.
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return out;
}

// Separable Gaussian. Weights computed analytically so any sigma/radius works.
@fragment
fn fs_blur(in: VsFull) -> FsOut {
    var out: FsOut;
    let two_s2 = 2.0 * blur.sigma * blur.sigma;
    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    // center tap
    {
        let w = 1.0;
        acc += textureSampleLevel(src_tex, src_samp, in.uv, 0.0) * w;
        wsum += w;
    }
    for (var i = 1; i <= blur.radius; i = i + 1) {
        let fi = f32(i);
        let w = exp(-(fi * fi) / two_s2);
        let off = blur.direction * blur.texel_size * fi;
        acc += textureSampleLevel(src_tex, src_samp, in.uv + off, 0.0) * w;
        acc += textureSampleLevel(src_tex, src_samp, in.uv - off, 0.0) * w;
        wsum += 2.0 * w;
    }
    out.color = acc / max(wsum, 1e-5);
    return out;
}

// Straight copy.
@fragment
fn fs_blit(in: VsFull) -> FsOut {
    var out: FsOut;
    out.color = textureSampleLevel(src_tex, src_samp, in.uv, 0.0);
    return out;
}

@group(1) @binding(0) var<uniform> frost: FrostParams;

// Signed distance to a rounded box centered at the origin with half-size `b`
// and corner radius `r`. Negative inside.
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// Composite one frosted panel. Sampled `src_tex` is the BLURRED backdrop.
@fragment
fn fs_frost(in: VsFull) -> FsOut {
    var out: FsOut;
    // Fragment position in full-resolution pixels.
    let frag = in.uv * frost.resolution;

    let center = 0.5 * (frost.rect_min + frost.rect_max);
    let half = 0.5 * (frost.rect_max - frost.rect_min);
    let r = min(frost.corner_radius, min(half.x, half.y));
    let d = sd_rounded_box(frag - center, half, r);

    // 1px-ish antialiased coverage of the rounded rect.
    let aa = 1.0;
    let coverage = 1.0 - smoothstep(-aa, aa, d);
    if (coverage <= 0.0) {
        discard;
    }

    // Sample the blurred backdrop at this fragment (UV is shared 0..1).
    var col = textureSampleLevel(src_tex, src_samp, in.uv, 0.0).rgb;

    // Lay the translucent tint over the blurred backdrop (straight-alpha over).
    col = mix(col, frost.tint.rgb, frost.tint.a);

    // Subtle light inner edge: brighten a ~1.5px band just inside the border.
    let edge_band = 1.5;
    // distance inside the panel from the border (positive inside)
    let inside = -d;
    let edge_t = clamp(1.0 - inside / edge_band, 0.0, 1.0) * clamp(inside / 0.5, 0.0, 1.0);
    col += vec3<f32>(frost.edge) * edge_t;

    out.color = vec4<f32>(col, coverage);
    return out;
}
