// One pipeline draws everything: each instance is a quad whose fragment
// shader either evaluates a shape analytically or reads the atlas. Output
// is premultiplied alpha.

struct Globals {
    screen: vec2<f32>, // framebuffer size in pixels
    cursor: vec2<f32>, // late-latched cursor position in pixels
};

@group(0) @binding(0) var<uniform> g: Globals;
// One atlas holds every image: font glyphs and skin sprites.
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
// The images of the menus, and the background of the selected beatmap.
@group(0) @binding(3) var ui_atlas: texture_2d<f32>;
@group(0) @binding(4) var background: texture_2d<f32>;

const KIND_RECT: u32 = 0u;
const KIND_DISC: u32 = 1u;
const KIND_RING: u32 = 2u;
const KIND_GLYPH: u32 = 3u;
// A line segment with round ends: half.x is half the length plus the
// radius, half.y the radius, p0 the direction in radians.
const KIND_CAPSULE: u32 = 4u;
// A piece of a texture, multiplied by the instance colour. p0 rotates it,
// p1 says which texture: 0 the atlas, 1 the menu atlas, 2 the background.
const KIND_SPRITE: u32 = 5u;
// A rectangle with round corners: p0 is the corner radius; p1, if not zero,
// makes it an outline that many sixteenths of a pixel thick.
const KIND_RRECT: u32 = 6u;
// One segment of a slider body, shaped like a capsule. color.rgb is the
// track colour and color.a the fade; uv.rgb the border colour; p1 the index
// of the body's first segment, which is the depth layer the whole body
// shares.
const KIND_BODY: u32 = 7u;
const KIND_SECTOR: u32 = 8u;
// Flag: `center` is an offset from the latched cursor, not a screen position.
// The CPU never bakes the cursor into vertex data; it only updates the
// uniform right before submit.
const FLAG_AT_CURSOR: u32 = 256u;

// Keep in sync with lib.rs. Each instance gets its own depth layer, later
// instances higher (the depth test is GREATER), so depth never reorders
// anything; within a slider body the layer is shared and the part of a
// segment nearer its centre line lies higher, which merges the segments.
const MAX_INSTANCES: f32 = 16384.0;
const LAYER: f32 = 1.0 / (MAX_INSTANCES + 2.0);

struct VsIn {
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
    @location(0) center: vec2<f32>,
    @location(1) half: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) kind: u32,
    @location(4) p0: f32,
    @location(5) p1: u32,
    // Atlas rectangle: (u0, v0, u1, v1).
    @location(6) uv: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) half: vec2<f32>,
    @location(4) @interpolate(flat) p0: f32,
    @location(5) @interpolate(flat) p1: u32,
    @location(6) uv: vec2<f32>,
    @location(7) @interpolate(flat) layer: f32,
    @location(8) @interpolate(flat) border: vec3<f32>,
    @location(9) @interpolate(flat) depth_only: f32,
};

struct FsOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

@vertex
fn vs(i: VsIn) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[i.vi];
    var center = i.center;
    if ((i.kind & FLAG_AT_CURSOR) != 0u) {
        center = center + g.cursor;
    }
    var offset = c * i.half;
    let kind = i.kind & 255u;
    if (kind == KIND_CAPSULE || kind == KIND_SPRITE || kind == KIND_BODY) {
        let cs = cos(i.p0);
        let sn = sin(i.p0);
        offset = vec2<f32>(offset.x * cs - offset.y * sn, offset.x * sn + offset.y * cs);
    }
    let p = center + offset;
    var o: VsOut;
    o.pos = vec4<f32>(p / g.screen * 2.0 - 1.0, 0.0, 1.0);
    o.local = c * i.half;
    o.color = i.color;
    o.kind = i.kind & 255u;
    o.half = i.half;
    o.p0 = i.p0;
    o.p1 = i.p1;
    o.uv = mix(i.uv.xy, i.uv.zw, c * 0.5 + 0.5);
    o.border = i.uv.rgb;
    o.depth_only = i.uv.a;
    if (kind == KIND_BODY) {
        o.layer = f32(i.p1 + 1u) * LAYER;
    } else {
        o.layer = f32(i.ii + 1u) * LAYER;
    }
    return o;
}

// osu!stable's slider body, as a function of the distance from the path.
// Outermost is a shadow (left out here: the body is blended once per pixel), then the
// border, then the track colour from darker at the edge to lighter in the
// middle.
const BODY_SHADOW: f32 = 0.078125; // 1 - 59/64
const BODY_BORDER: f32 = 0.1875;

fn body(i: VsOut) -> FsOut {
    let r = i.half.y;
    let along = max(abs(i.local.x) - (i.half.x - r), 0.0);
    let dist = length(vec2<f32>(along, i.local.y));
    let pos = 1.0 - dist / r; // 0 on the outer edge, 1 on the path
    let a = clamp(r * (1.0 - BODY_SHADOW) - dist + 0.5, 0.0, 1.0);
    let in_border = clamp(dist - r * (1.0 - BODY_BORDER) + 0.5, 0.0, 1.0);
    let track = i.color.rgb;
    let outer = track / 1.1;
    let inner = min(track * 1.125 + vec3<f32>(0.25), vec3<f32>(1.0));
    let t = clamp((pos - BODY_BORDER) / (1.0 - BODY_BORDER), 0.0, 1.0);
    // Interpolate premultiplied track and border so the transition has the
    // correct opacity and colour even when the track is translucent.
    let rgb = mix(mix(outer, inner, t) * 0.7, i.border, in_border);
    let opacity = mix(0.7, 1.0, in_border);
    let fade = a * i.color.a;
    let alpha = opacity * fade;
    var o: FsOut;
    o.color = vec4<f32>(rgb * fade, alpha);
    if (i.depth_only > 0.0) {
        o.color = vec4<f32>(0.0);
    }
    o.depth = i.layer + clamp(pos, 0.0, 1.0) * 0.9 * LAYER;
    return o;
}

@fragment
fn fs(i: VsOut) -> FsOut {
    var out: FsOut;
    out.depth = i.layer;
    if (i.kind == KIND_BODY) {
        return body(i);
    }
    if (i.kind == KIND_SPRITE) {
        // No mip levels, so the level can be named and the lookup may sit
        // inside a branch: only the texture that is wanted gets read.
        var texel: vec4<f32>;
        if (i.p1 == 1u) {
            texel = textureSampleLevel(ui_atlas, atlas_sampler, i.uv, 0.0);
        } else if (i.p1 == 2u) {
            texel = textureSampleLevel(background, atlas_sampler, i.uv, 0.0);
        } else {
            texel = textureSampleLevel(atlas, atlas_sampler, i.uv, 0.0);
        }
        // The textures are stored premultiplied.
        out.color = texel * vec4<f32>(i.color.rgb * i.color.a, i.color.a);
        return out;
    }
    var a = 1.0;
    if (i.kind == KIND_DISC) {
        // half.x is the radius; one pixel of analytic edge coverage.
        a = clamp(i.half.x - length(i.local), 0.0, 1.0);
    } else if (i.kind == KIND_SECTOR) {
        let angle = (atan2(i.local.x, -i.local.y) + 6.28318530718) % 6.28318530718;
        a = clamp(i.half.x - length(i.local), 0.0, 1.0);
        if (i.p0 <= 0.0 || (i.p0 < 1.0 && angle >= i.p0 * 6.28318530718)) {
            a = 0.0;
        }
    } else if (i.kind == KIND_RING) {
        // p0 is the stroke width, measured inwards from the outer radius.
        let mid = i.half.x - i.p0 * 0.5;
        a = clamp(i.p0 * 0.5 - abs(length(i.local) - mid) + 0.5, 0.0, 1.0);
    } else if (i.kind == KIND_CAPSULE) {
        // Distance to the segment along the local x axis.
        let along = max(abs(i.local.x) - (i.half.x - i.half.y), 0.0);
        a = clamp(i.half.y - length(vec2<f32>(along, i.local.y)) + 0.5, 0.0, 1.0);
    } else if (i.kind == KIND_RRECT) {
        let q = abs(i.local) - (i.half - vec2<f32>(i.p0));
        let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - i.p0;
        a = clamp(0.5 - d, 0.0, 1.0);
        if (i.p1 != 0u) {
            a = a * clamp(d + f32(i.p1) / 16.0 + 0.5, 0.0, 1.0);
        }
    } else if (i.kind == KIND_GLYPH) {
        // p1 holds a 3x5 bitmap, bit (row * 3 + column), row 0 on top.
        let uv = i.local / i.half * 0.5 + 0.5;
        let cx = min(u32(uv.x * 3.0), 2u);
        let cy = min(u32(uv.y * 5.0), 4u);
        a = f32((i.p1 >> (cy * 3u + cx)) & 1u);
    }
    let alpha = i.color.a * a;
    out.color = vec4<f32>(i.color.rgb * alpha, alpha);
    return out;
}
