struct Uniforms {
    /// Overall edge darkness (0 = off, 1 = full designed falloff).
    strength: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
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

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    // Rectangular edge falloff (inner shadow): 0 at center axes, 1 at borders.
    let p = abs(uv * 2.0 - 1.0);
    // How far inward the soft edge reaches (NDC units).
    let soft = 0.52;
    let ax = smoothstep(1.0 - soft, 1.0, p.x);
    let ay = smoothstep(1.0 - soft, 1.0, p.y);
    // Prefer max for a rectangular frame; light corner boost without a hard box.
    let edge = max(ax, ay);
    let corner = ax * ay;
    let alpha = clamp(edge * 0.72 + corner * 0.28, 0.0, 0.82) * clamp(u.strength, 0.0, 1.0);
    // Straight-alpha black; pipeline uses ALPHA_BLENDING.
    return vec4<f32>(0.0, 0.0, 0.0, alpha);
}
