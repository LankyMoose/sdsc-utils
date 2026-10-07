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

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    // Soft vignette: darker toward edges.
    let centered = uv * 2.0 - 1.0;
    let vignette = clamp(1.0 - dot(centered, centered) * 0.55, 0.22, 1.0);

    var color = mix(u.base_bg.rgb, u.content.rgb, 0.28 + 0.32 * vignette);
    color *= vignette;

    // Radial accent bloom behind the games strip (mid-left).
    let glow_c = vec2<f32>(-0.35, 0.05);
    let glow_d = length(centered - glow_c);
    let glow = exp(-glow_d * glow_d * 2.8);
    let pulse = 0.5 + 0.5 * sin(u.time * 1.4);
    color += u.accent.rgb * glow * (0.10 + 0.04 * pulse);

    // Secondary cooler lift toward the center stage.
    let stage_glow = exp(-length(centered - vec2<f32>(0.15, 0.0)) * 1.6);
    color = mix(color, color + u.content.rgb * 0.12, stage_glow * 0.35);

    // Dock side falloff (right edge deepens as the drawer opens).
    let dock_x = smoothstep(0.55, 1.0, uv.x) * u.dock_progress;
    color = mix(color, u.base_bg.rgb * 0.72, dock_x * 0.65);

    // Very soft animated grain.
    let grain = (hash21(uv * vec2<f32>(960.0, 540.0) + u.time * 12.0) - 0.5) * 0.03;
    color += grain;

    // Accent breath while veiling (promote/demote).
    let veil_pulse = 0.5 + 0.5 * sin(u.time * 3.2);
    let accent_wash = u.accent.rgb * (0.05 + 0.07 * veil_pulse) * u.veil;
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
    color += u.accent.rgb * rim * (1.0 - aperture) * 0.35;

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
