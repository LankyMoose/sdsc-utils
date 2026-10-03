struct Uniforms {
    base_bg: vec4<f32>,
    content: vec4<f32>,
    accent: vec4<f32>,
    time: f32,
    dock_progress: f32,
    veil: f32,
    _pad: f32,
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
    let vignette = clamp(1.0 - dot(centered, centered) * 0.42, 0.35, 1.0);

    var color = mix(u.base_bg.rgb, u.content.rgb, 0.35 + 0.25 * vignette);
    color *= vignette;

    // Dock side falloff (right edge deepens as the drawer opens).
    let dock_x = smoothstep(0.55, 1.0, uv.x) * u.dock_progress;
    color = mix(color, u.base_bg.rgb * 0.82, dock_x * 0.55);

    // Very soft animated grain.
    let grain = (hash21(uv * vec2<f32>(960.0, 540.0) + u.time * 12.0) - 0.5) * 0.035;
    color += grain;

    // Accent breath while veiling (promote/demote).
    let pulse = 0.5 + 0.5 * sin(u.time * 3.2);
    let accent_wash = u.accent.rgb * (0.04 + 0.06 * pulse) * u.veil;
    color = mix(color, color + accent_wash, u.veil);

    // Veil solidifies toward base so chrome never stretches through.
    color = mix(color, u.base_bg.rgb, u.veil * 0.72);

    return vec4<f32>(color, 1.0);
}
