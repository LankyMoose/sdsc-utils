struct Uniforms {
    base_bg: vec4<f32>,
    content: vec4<f32>,
    accent: vec4<f32>,
    time: f32,
    dock_progress: f32,
    veil: f32,
    aperture: f32,
    // x, y = widget size in physical px; z = corner radius in physical px
    // (0 = square, fully opaque — immersive). w unused.
    mask: vec4<f32>,
    // Dev-only tuning (debug builds):
    // tune  = pulse freq rad/s (1.4), veil freq rad/s (3.2),
    //         pulse min (0.10), pulse max (0.14).
    // tune2 = grain gain (1.0), orb drift speed rad/s (0.3),
    //         glow hue shift deg (0.0), unused.
    // tune3 = orb bounds in centered uv (-1..1):
    //         min_x, max_x, min_y, max_y ((-0.35,-0.35,0.05,0.05) = static).
    // Release builds always send defaults.
    tune: vec4<f32>,
    tune2: vec4<f32>,
    tune3: vec4<f32>,
}

// Signed distance to a rounded rect of half-size `half` centred at the origin.
fn rounded_rect_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
    // Fullscreen triangle covering the clip space.
    let uv = vec2<f32>(
        f32((vertex_index << 1u) & 2u),
        f32(vertex_index & 2u),
    );
    var out: VertexOut;
    out.position = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// Hue-rotate `c` by `deg` degrees around the grey axis (same matrix as CSS
// `hue-rotate`). Dev-only glow tuning; `0` is a no-op for shipped visuals.
fn hue_shift_rgb(c: vec3<f32>, deg: f32) -> vec3<f32> {
    if (abs(deg) < 0.5) {
        return c;
    }
    let rad = deg * 3.14159265 / 180.0;
    let s = sin(rad);
    let co = cos(rad);
    let m00 = 0.213 + co * 0.787 - s * 0.213;
    let m01 = 0.715 - co * 0.715 - s * 0.715;
    let m02 = 0.072 - co * 0.072 + s * 0.928;
    let m10 = 0.213 - co * 0.213 + s * 0.143;
    let m11 = 0.715 + co * 0.285 + s * 0.140;
    let m12 = 0.072 - co * 0.072 - s * 0.283;
    let m20 = 0.213 - co * 0.213 - s * 0.787;
    let m21 = 0.715 - co * 0.715 + s * 0.715;
    let m22 = 0.072 + co * 0.928 + s * 0.072;
    let r = m00 * c.r + m01 * c.g + m02 * c.b;
    let g = m10 * c.r + m11 * c.g + m12 * c.b;
    let b = m20 * c.r + m21 * c.g + m22 * c.b;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    // Soft vignette: darker toward edges.
    let centered = uv * 2.0 - 1.0;
    let vignette = clamp(1.0 - dot(centered, centered) * 0.55, 0.22, 1.0);

    var color = mix(u.base_bg.rgb, u.content.rgb, 0.28 + 0.32 * vignette);
    color *= vignette;

    // Dev-only glow hue (shipped 0 = theme accent untouched).
    let accent = hue_shift_rgb(u.accent.rgb, u.tune2.z);

    // Drifting accent orb: Lissajous wander inside the tuned bounds
    // (`tune3` = min_x, max_x, min_y, max_y in centered uv). Speed 0 parks
    // at the bounds center; equal min/max reproduces the static shipped orb.
    let orb_speed = u.tune2.y;
    let ox_m = 0.5 + 0.5 * sin(u.time * orb_speed);
    let oy_m = 0.5 + 0.5 * sin(u.time * orb_speed * 0.83);
    let glow_c = vec2<f32>(
        mix(u.tune3.x, u.tune3.y, ox_m),
        mix(u.tune3.z, u.tune3.w, oy_m),
    );
    let glow_d = length(centered - glow_c);
    let glow = exp(-glow_d * glow_d * 2.8);
    let pulse = 0.5 + 0.5 * sin(u.time * u.tune.x);
    let glow_strength = u.tune.z + (u.tune.w - u.tune.z) * pulse;
    color += accent * glow * glow_strength;

    // Secondary cooler lift toward the center stage.
    let stage_glow = exp(-length(centered - vec2<f32>(0.15, 0.0)) * 1.6);
    color = mix(color, color + u.content.rgb * 0.12, stage_glow * 0.35);

    // Dock side falloff (right edge deepens as the drawer opens).
    let dock_x = smoothstep(0.55, 1.0, uv.x) * u.dock_progress;
    color = mix(color, u.base_bg.rgb * 0.72, dock_x * 0.65);

    // Very soft animated grain.
    let grain = (hash21(uv * vec2<f32>(960.0, 540.0) + u.time * 12.0) - 0.5) * 0.03 * u.tune2.x;
    color += grain;

    // Accent breath while veiling (promote/demote).
    let veil_pulse = 0.5 + 0.5 * sin(u.time * u.tune.y);
    let accent_wash = accent * (0.05 + 0.07 * veil_pulse) * u.veil;
    color = mix(color, color + accent_wash, u.veil);

    // Veil solidifies toward base so chrome never stretches through.
    color = mix(color, u.base_bg.rgb, u.veil * 0.78);

    // Soft iris: aperture 0 = closed (solid base), 1 = fully open.
    let aperture = clamp(u.aperture, 0.0, 1.0);
    let radius = length(centered);
    // Expand open radius with a soft edge; closed keeps a tiny accent ring.
    let open_r = mix(0.08, 1.55, aperture);
    let edge = smoothstep(open_r, open_r - 0.22, radius);
    let iris = mix(0.0, 1.0, edge);
    color = mix(u.base_bg.rgb, color, iris);
    // Accent rim while opening/closing.
    let rim = smoothstep(open_r + 0.04, open_r, radius)
        * smoothstep(open_r - 0.18, open_r - 0.02, radius);
    color += accent * rim * (1.0 - aperture) * 0.35;

    // Rounded window corners (transparent windows): 1px AA edge, premultiplied
    // so the REPLACE blend leaves clean alpha for the compositor.
    let r = u.mask.z;
    if (r > 0.0) {
        let size = u.mask.xy;
        let p = uv * size - size * 0.5;
        let d = rounded_rect_sdf(p, size * 0.5, min(r, min(size.x, size.y) * 0.5));
        let a = clamp(0.5 - d, 0.0, 1.0);
        return vec4<f32>(color * a, a);
    }

    return vec4<f32>(color, 1.0);
}
