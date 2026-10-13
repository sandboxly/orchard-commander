// Instanced quads for the whole grid: one triangle strip of 4 vertices per
// instance, positions in physical pixels converted to clip space here.

struct Globals {
    viewport: vec2<f32>,
    mask_size: vec2<f32>,
    color_size: vec2<f32>,
    pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var mask_tex: texture_2d<f32>;
@group(0) @binding(3) var color_tex: texture_2d<f32>;
@group(1) @binding(0) var image_tex: texture_2d<f32>;

fn corner(vi: u32) -> vec2<f32> {
    return vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
}

fn clip_pos(p: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(p.x / g.viewport.x * 2.0 - 1.0, 1.0 - p.y / g.viewport.y * 2.0, 0.0, 1.0);
}

// --- solid rectangles: cell backgrounds, decorations, overlays ------------

struct QuadOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_quad(@builtin(vertex_index) vi: u32, @location(0) rect: vec4<f32>, @location(1) color: vec4<f32>) -> QuadOut {
    var o: QuadOut;
    o.pos = clip_pos(rect.xy + corner(vi) * rect.zw);
    o.color = color;
    return o;
}

@fragment
fn fs_quad(i: QuadOut) -> @location(0) vec4<f32> {
    return i.color;
}

// --- glyphs from the atlases ------------------------------------------------

struct GlyphOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs_glyph(
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<u32>,
    @location(2) color: vec4<f32>,
    @location(3) kind: u32,
) -> GlyphOut {
    let c = corner(vi);
    var o: GlyphOut;
    o.pos = clip_pos(rect.xy + c * rect.zw);
    let texel = vec2<f32>(uv.xy) + c * vec2<f32>(uv.zw);
    if kind == 1u {
        o.uv = texel / g.color_size;
    } else {
        o.uv = texel / g.mask_size;
    }
    o.color = color;
    o.kind = kind;
    return o;
}

@fragment
fn fs_glyph(i: GlyphOut) -> @location(0) vec4<f32> {
    let coverage = textureSampleLevel(mask_tex, samp, i.uv, 0.0).r;
    let colored = textureSampleLevel(color_tex, samp, i.uv, 0.0);
    if i.kind == 1u {
        return vec4<f32>(colored.rgb, colored.a * i.color.a);
    }
    return vec4<f32>(i.color.rgb, i.color.a * coverage);
}

// --- images (previews) --------------------------------------------------------

struct ImageOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) opacity: f32,
};

@vertex
fn vs_image(
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) opacity: f32,
) -> ImageOut {
    let c = corner(vi);
    var o: ImageOut;
    o.pos = clip_pos(rect.xy + c * rect.zw);
    o.uv = uv.xy + c * uv.zw;
    o.opacity = opacity;
    return o;
}

@fragment
fn fs_image(i: ImageOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(image_tex, samp, i.uv, 0.0);
    return vec4<f32>(c.rgb, c.a * i.opacity);
}
