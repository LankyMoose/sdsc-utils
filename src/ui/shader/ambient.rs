//! Full-bleed immersive atmosphere via iced's custom shader widget.

use crate::ui::theme;
use iced::mouse;
use iced::widget::shader::{self, Viewport};
use iced::{Color, Rectangle};
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static PIPELINE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Defaults: locked-in Diagnostics tuning export (also the release behavior).
/// pulse 0.85 rad/s (~7.4s breath), bloom 0.02–0.08, grain 0.6, orb drifting
/// in a wide shallow box (±0.35 x, ±0.05 y at 0.3 rad/s), hue unshifted.
pub const DEFAULT_PULSE_FREQ: f32 = 0.85;
pub const DEFAULT_VEIL_FREQ: f32 = 3.2;
/// Accent-bloom strength floor (pulse trough).
pub const DEFAULT_PULSE_MIN: f32 = 0.02;
/// Accent-bloom strength ceiling (pulse peak).
pub const DEFAULT_PULSE_MAX: f32 = 0.08;
pub const DEFAULT_GRAIN_GAIN: f32 = 0.6;
/// Orb drift speed, rad/s (0 parks at the bounds center).
pub const DEFAULT_ORB_SPEED: f32 = 0.3;
/// Glow hue shift, degrees (0 = theme accent untouched).
pub const DEFAULT_GLOW_HUE: f32 = 0.0;
/// Shipped orb wander bounds in centered uv (wide shallow drift).
pub const DEFAULT_ORB_MIN_X: f32 = -0.35;
pub const DEFAULT_ORB_MAX_X: f32 = 0.34999996;
pub const DEFAULT_ORB_MIN_Y: f32 = -0.05000002;
pub const DEFAULT_ORB_MAX_Y: f32 = 0.05;
/// CPU time advance multiplier (`tick_anim`); 0 freezes the pulse.
pub const DEFAULT_TIME_SCALE: f32 = 1.0;

/// Reset one-shot pipeline diag (process Exit) so the next run logs again.
pub fn reset_pipeline_diag() {
    PIPELINE_LOGGED.store(false, Ordering::Relaxed);
}

/// CPU-side uniforms (must match [`ambient.wgsl`] layout / alignment).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct AmbientUniforms {
    pub base_bg: [f32; 4],
    pub content: [f32; 4],
    pub accent: [f32; 4],
    pub time: f32,
    pub dock_progress: f32,
    pub veil: f32,
    /// Soft iris open amount (0 = closed, 1 = fully open).
    pub aperture: f32,
    /// `[width_px, height_px, corner_radius_px, _]`. Filled in `prepare` from
    /// the widget bounds × scale; radius 0 keeps the output opaque and square.
    pub mask: [f32; 4],
    /// Dev-only tuning tail (appended last so existing offsets stay stable):
    /// `tune = [pulse_freq, veil_freq, pulse_min, pulse_max]`,
    /// `tune2 = [grain_gain, orb_speed, glow_hue_deg, _]`,
    /// `tune3 = [orb_min_x, orb_max_x, orb_min_y, orb_max_y]`.
    /// Release builds always send defaults; debug builds read the
    /// process-wide tuning atomics (see [`set_pulse_freq`]).
    pub tune: [f32; 4],
    pub tune2: [f32; 4],
    pub tune3: [f32; 4],
}

#[cfg(debug_assertions)]
static PULSE_FREQ_BITS: AtomicU32 = AtomicU32::new(DEFAULT_PULSE_FREQ.to_bits());
#[cfg(debug_assertions)]
static VEIL_FREQ_BITS: AtomicU32 = AtomicU32::new(DEFAULT_VEIL_FREQ.to_bits());
#[cfg(debug_assertions)]
static PULSE_MIN_BITS: AtomicU32 = AtomicU32::new(DEFAULT_PULSE_MIN.to_bits());
#[cfg(debug_assertions)]
static PULSE_MAX_BITS: AtomicU32 = AtomicU32::new(DEFAULT_PULSE_MAX.to_bits());
#[cfg(debug_assertions)]
static GRAIN_GAIN_BITS: AtomicU32 = AtomicU32::new(DEFAULT_GRAIN_GAIN.to_bits());
#[cfg(debug_assertions)]
static ORB_SPEED_BITS: AtomicU32 = AtomicU32::new(DEFAULT_ORB_SPEED.to_bits());
#[cfg(debug_assertions)]
static GLOW_HUE_BITS: AtomicU32 = AtomicU32::new(DEFAULT_GLOW_HUE.to_bits());
#[cfg(debug_assertions)]
static ORB_MIN_X_BITS: AtomicU32 = AtomicU32::new(DEFAULT_ORB_MIN_X.to_bits());
#[cfg(debug_assertions)]
static ORB_MAX_X_BITS: AtomicU32 = AtomicU32::new(DEFAULT_ORB_MAX_X.to_bits());
#[cfg(debug_assertions)]
static ORB_MIN_Y_BITS: AtomicU32 = AtomicU32::new(DEFAULT_ORB_MIN_Y.to_bits());
#[cfg(debug_assertions)]
static ORB_MAX_Y_BITS: AtomicU32 = AtomicU32::new(DEFAULT_ORB_MAX_Y.to_bits());
#[cfg(debug_assertions)]
static TIME_SCALE_BITS: AtomicU32 = AtomicU32::new(DEFAULT_TIME_SCALE.to_bits());

/// Current pulse frequency (rad/s). Release: always [`DEFAULT_PULSE_FREQ`].
pub fn pulse_freq() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(PULSE_FREQ_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_PULSE_FREQ
    }
}

/// Current veil frequency (rad/s). Release: always [`DEFAULT_VEIL_FREQ`].
pub fn veil_freq() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(VEIL_FREQ_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_VEIL_FREQ
    }
}

/// Current pulse floor. Release: always [`DEFAULT_PULSE_MIN`].
pub fn pulse_min() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(PULSE_MIN_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_PULSE_MIN
    }
}

/// Current pulse ceiling. Release: always [`DEFAULT_PULSE_MAX`].
pub fn pulse_max() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(PULSE_MAX_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_PULSE_MAX
    }
}

/// Current grain gain scale. Release: always [`DEFAULT_GRAIN_GAIN`].
pub fn grain_gain() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(GRAIN_GAIN_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_GRAIN_GAIN
    }
}

/// Current orb drift speed (rad/s). Release: always [`DEFAULT_ORB_SPEED`].
pub fn orb_speed() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(ORB_SPEED_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_ORB_SPEED
    }
}

/// Current glow hue shift (degrees). Release: always [`DEFAULT_GLOW_HUE`].
pub fn glow_hue() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(GLOW_HUE_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_GLOW_HUE
    }
}

/// Orb X bounds in centered uv. Release: always the static defaults.
pub fn orb_min_x() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(ORB_MIN_X_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_ORB_MIN_X
    }
}

/// Orb X bounds in centered uv. Release: always the static defaults.
pub fn orb_max_x() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(ORB_MAX_X_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_ORB_MAX_X
    }
}

/// Orb Y bounds in centered uv. Release: always the static defaults.
pub fn orb_min_y() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(ORB_MIN_Y_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_ORB_MIN_Y
    }
}

/// Orb Y bounds in centered uv. Release: always the static defaults.
pub fn orb_max_y() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(ORB_MAX_Y_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_ORB_MAX_Y
    }
}

/// Current CPU time advance multiplier. Release: always [`DEFAULT_TIME_SCALE`].
pub fn time_scale() -> f32 {
    #[cfg(debug_assertions)]
    {
        f32::from_bits(TIME_SCALE_BITS.load(Ordering::Relaxed))
    }
    #[cfg(not(debug_assertions))]
    {
        DEFAULT_TIME_SCALE
    }
}

/// Per-frame ambient time advance (debug `time_scale` applied).
pub fn ambient_tick_dt() -> f32 {
    const DT: f32 = 1.0 / 60.0;
    DT * time_scale()
}

/// Ambient seconds for `elapsed` wall-clock (per-frame capped, debug
/// `time_scale` applied). Tick drivers must advance by this — never by fixed
/// steps per call: StartFrame (60Hz) and report-rate pad edges share
/// `tick_anim`, so per-call steps fast-forward time whenever edges outnumber
/// frames (~4ms sample path while hot).
pub fn ambient_dt_for_elapsed(elapsed: Duration) -> f32 {
    elapsed.min(crate::ui::motion::MAX_FRAME_DT).as_secs_f32() * time_scale()
}

/// Atomically add `dt` seconds to an `f32`-as-bits clock.
///
/// Deliberately a `compare_exchange` retry loop instead of
/// `AtomicU32::fetch_update` (renamed to `try_update` in Rust 1.99): the
/// loop compiles warning-free from MSRV (1.88) through latest stable, while
/// the new name breaks older toolchains and the old name errors under
/// `-D warnings` on new ones.
pub fn advance_time_bits(clock: &AtomicU32, dt: f32) {
    let mut bits = clock.load(Ordering::Relaxed);
    loop {
        let next = (f32::from_bits(bits) + dt).to_bits();
        match clock.compare_exchange_weak(bits, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => bits = actual,
        }
    }
}

/// True while any ambient tuning differs from the shipped defaults.
#[cfg(debug_assertions)]
pub fn tuning_active() -> bool {
    pulse_freq() != DEFAULT_PULSE_FREQ
        || veil_freq() != DEFAULT_VEIL_FREQ
        || pulse_min() != DEFAULT_PULSE_MIN
        || pulse_max() != DEFAULT_PULSE_MAX
        || grain_gain() != DEFAULT_GRAIN_GAIN
        || orb_speed() != DEFAULT_ORB_SPEED
        || glow_hue() != DEFAULT_GLOW_HUE
        || orb_min_x() != DEFAULT_ORB_MIN_X
        || orb_max_x() != DEFAULT_ORB_MAX_X
        || orb_min_y() != DEFAULT_ORB_MIN_Y
        || orb_max_y() != DEFAULT_ORB_MAX_Y
        || time_scale() != DEFAULT_TIME_SCALE
}

/// Release builds have no tuning: always inactive (lets utility code share
/// the same call path without `cfg` at every site).
#[cfg(not(debug_assertions))]
pub fn tuning_active() -> bool {
    false
}

/// Debug-only setters (Diagnostics sliders; shell-local, never persisted).
/// Values are clamped to the slider ranges.
#[cfg(debug_assertions)]
pub fn set_pulse_freq(v: f32) {
    PULSE_FREQ_BITS.store(v.clamp(0.0, 5.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_veil_freq(v: f32) {
    VEIL_FREQ_BITS.store(v.clamp(0.0, 8.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_pulse_min(v: f32) {
    PULSE_MIN_BITS.store(v.clamp(0.0, 0.6).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_pulse_max(v: f32) {
    PULSE_MAX_BITS.store(v.clamp(0.0, 0.6).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_grain_gain(v: f32) {
    GRAIN_GAIN_BITS.store(v.clamp(0.0, 5.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_orb_speed(v: f32) {
    ORB_SPEED_BITS.store(v.clamp(0.0, 3.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_glow_hue(v: f32) {
    GLOW_HUE_BITS.store(v.clamp(0.0, 360.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_orb_min_x(v: f32) {
    ORB_MIN_X_BITS.store(v.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_orb_max_x(v: f32) {
    ORB_MAX_X_BITS.store(v.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_orb_min_y(v: f32) {
    ORB_MIN_Y_BITS.store(v.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_orb_max_y(v: f32) {
    ORB_MAX_Y_BITS.store(v.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
}
#[cfg(debug_assertions)]
pub fn set_time_scale(v: f32) {
    TIME_SCALE_BITS.store(v.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
}
/// Restore shipped behavior (Diagnostics reset).
#[cfg(debug_assertions)]
pub fn reset_ambient_tuning() {
    set_pulse_freq(DEFAULT_PULSE_FREQ);
    set_veil_freq(DEFAULT_VEIL_FREQ);
    set_pulse_min(DEFAULT_PULSE_MIN);
    set_pulse_max(DEFAULT_PULSE_MAX);
    set_grain_gain(DEFAULT_GRAIN_GAIN);
    set_orb_speed(DEFAULT_ORB_SPEED);
    set_glow_hue(DEFAULT_GLOW_HUE);
    set_orb_min_x(DEFAULT_ORB_MIN_X);
    set_orb_max_x(DEFAULT_ORB_MAX_X);
    set_orb_min_y(DEFAULT_ORB_MIN_Y);
    set_orb_max_y(DEFAULT_ORB_MAX_Y);
    set_time_scale(DEFAULT_TIME_SCALE);
}

/// Test lock for the process-wide tuning globals (see `notes/flaky-tests.md`):
/// any debug test that mutates tuning must hold this while asserting, and
/// tests that assert shipped-default visuals must hold it too. Release tests
/// compile it to a pass-through (no tuning exists there).
#[cfg(test)]
pub(crate) fn with_tune_test_lock<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    {
        static TUNE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = TUNE_TEST_LOCK.lock().unwrap();
        reset_ambient_tuning_for_test();
        let result = f();
        reset_ambient_tuning_for_test();
        result
    }
    #[cfg(not(debug_assertions))]
    {
        f()
    }
}
#[cfg(all(test, debug_assertions))]
fn reset_ambient_tuning_for_test() {
    PULSE_FREQ_BITS.store(DEFAULT_PULSE_FREQ.to_bits(), Ordering::Relaxed);
    VEIL_FREQ_BITS.store(DEFAULT_VEIL_FREQ.to_bits(), Ordering::Relaxed);
    PULSE_MIN_BITS.store(DEFAULT_PULSE_MIN.to_bits(), Ordering::Relaxed);
    PULSE_MAX_BITS.store(DEFAULT_PULSE_MAX.to_bits(), Ordering::Relaxed);
    GRAIN_GAIN_BITS.store(DEFAULT_GRAIN_GAIN.to_bits(), Ordering::Relaxed);
    ORB_SPEED_BITS.store(DEFAULT_ORB_SPEED.to_bits(), Ordering::Relaxed);
    GLOW_HUE_BITS.store(DEFAULT_GLOW_HUE.to_bits(), Ordering::Relaxed);
    ORB_MIN_X_BITS.store(DEFAULT_ORB_MIN_X.to_bits(), Ordering::Relaxed);
    ORB_MAX_X_BITS.store(DEFAULT_ORB_MAX_X.to_bits(), Ordering::Relaxed);
    ORB_MIN_Y_BITS.store(DEFAULT_ORB_MIN_Y.to_bits(), Ordering::Relaxed);
    ORB_MAX_Y_BITS.store(DEFAULT_ORB_MAX_Y.to_bits(), Ordering::Relaxed);
    TIME_SCALE_BITS.store(DEFAULT_TIME_SCALE.to_bits(), Ordering::Relaxed);
}

impl AmbientUniforms {
    pub fn from_theme(time: f32, dock_progress: f32, veil: f32, aperture: f32) -> Self {
        Self {
            base_bg: color4(theme::BASE_BG),
            content: color4(theme::CONTENT),
            accent: color4(theme::ACCENT),
            time,
            dock_progress: dock_progress.clamp(0.0, 1.0),
            veil: veil.clamp(0.0, 1.0),
            aperture: aperture.clamp(0.0, 1.0),
            mask: [0.0; 4],
            tune: [pulse_freq(), veil_freq(), pulse_min(), pulse_max()],
            tune2: [grain_gain(), orb_speed(), glow_hue(), 0.0],
            tune3: [orb_min_x(), orb_max_x(), orb_min_y(), orb_max_y()],
        }
    }

    /// Copy with the corner mask resolved for a widget of `bounds` (logical)
    /// at `scale`; `radius` is logical px.
    fn with_mask(mut self, bounds: &Rectangle, scale: f32, radius: f32) -> Self {
        self.mask = [
            bounds.width * scale,
            bounds.height * scale,
            radius.max(0.0) * scale,
            0.0,
        ];
        self
    }
}

fn color4(c: Color) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

#[derive(Debug, Clone, Copy)]
pub struct AmbientProgram {
    pub uniforms: AmbientUniforms,
    /// Logical corner radius for transparent rounded windows (0 = square).
    pub corner_radius: f32,
}

impl AmbientProgram {
    pub fn new(time: f32, dock_progress: f32, veil: f32, aperture: f32) -> Self {
        Self {
            uniforms: AmbientUniforms::from_theme(time, dock_progress, veil, aperture),
            corner_radius: 0.0,
        }
    }

    /// Mask the output to a rounded rect (transparent corners).
    pub fn rounded(mut self, radius: f32) -> Self {
        self.corner_radius = radius.max(0.0);
        self
    }
}

impl<Message> shader::Program<Message> for AmbientProgram {
    type State = ();
    type Primitive = AmbientPrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        AmbientPrimitive {
            uniforms: self.uniforms,
            corner_radius: self.corner_radius,
        }
    }
}

#[derive(Debug)]
pub struct AmbientPrimitive {
    uniforms: AmbientUniforms,
    corner_radius: f32,
}

impl shader::Primitive for AmbientPrimitive {
    type Pipeline = AmbientPipeline;

    fn prepare(
        &self,
        pipeline: &mut AmbientPipeline,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let uniforms = self
            .uniforms
            .with_mask(bounds, viewport.scale_factor(), self.corner_radius);
        pipeline.update(queue, &uniforms);
    }

    fn draw(&self, pipeline: &AmbientPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, &pipeline.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}

pub struct AmbientPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl AmbientPipeline {
    fn update(&self, queue: &wgpu::Queue, uniforms: &AmbientUniforms) {
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(uniforms));
    }
}

impl shader::Pipeline for AmbientPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        if !PIPELINE_LOGGED.swap(true, Ordering::Relaxed) {
            crate::controller::hid::diag::diag_info("ui-diag: shader ambient pipeline ready");
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ambient atmosphere"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "ambient.wgsl"
            ))),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ambient uniforms"),
            size: std::mem::size_of::<AmbientUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ambient uniforms layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ambient uniforms bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ambient pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ambient pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_buffer,
            bind_group,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniforms_clamp_dock_veil_aperture() {
        let u = AmbientUniforms::from_theme(1.5, 2.0, -1.0, 1.5);
        assert_eq!(u.dock_progress, 1.0);
        assert_eq!(u.veil, 0.0);
        assert_eq!(u.aperture, 1.0);
        assert_eq!(u.time, 1.5);
        assert!((u.base_bg[0] - theme::BASE_BG.r).abs() < 0.001);
    }

    #[test]
    fn uniforms_size_matches_wgsl_layout() {
        // 3 × vec4 colors + 4 × f32 + vec4 mask + 3 × vec4 dev-tune tail
        // (16-byte aligned; the tail keeps existing field offsets stable).
        assert_eq!(std::mem::size_of::<AmbientUniforms>(), 128);
    }

    #[test]
    fn mask_scales_to_physical_pixels() {
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 380.0,
            height: 200.0,
        };
        let u = AmbientUniforms::from_theme(0.0, 0.0, 0.0, 1.0).with_mask(&bounds, 1.5, 12.0);
        assert_eq!(u.mask, [570.0, 300.0, 18.0, 0.0]);
        let square = AmbientUniforms::from_theme(0.0, 0.0, 0.0, 1.0).with_mask(&bounds, 2.0, -1.0);
        assert_eq!(square.mask[2], 0.0);
    }

    #[test]
    fn tune_tail_defaults_reproduce_release() {
        with_tune_test_lock(|| {
            let u = AmbientUniforms::from_theme(0.0, 0.0, 0.0, 1.0);
            assert_eq!(
                u.tune,
                [
                    DEFAULT_PULSE_FREQ,
                    DEFAULT_VEIL_FREQ,
                    DEFAULT_PULSE_MIN,
                    DEFAULT_PULSE_MAX
                ]
            );
            assert_eq!(
                u.tune2,
                [DEFAULT_GRAIN_GAIN, DEFAULT_ORB_SPEED, DEFAULT_GLOW_HUE, 0.0]
            );
            assert_eq!(
                u.tune3,
                [
                    DEFAULT_ORB_MIN_X,
                    DEFAULT_ORB_MAX_X,
                    DEFAULT_ORB_MIN_Y,
                    DEFAULT_ORB_MAX_Y
                ]
            );
            // Locked-in orb: wide shallow drift box (±0.35 x, ±0.05 y).
            assert!((u.tune3[0] + 0.35).abs() < 1e-6);
            assert!((u.tune3[1] - 0.34999996).abs() < 1e-6);
            assert!((u.tune3[2] + 0.05000002).abs() < 1e-6);
            assert!((u.tune3[3] - 0.05).abs() < 1e-6);
            assert!((ambient_tick_dt() - 1.0 / 60.0).abs() < 1e-6);
            assert!(!tuning_active());
        });
    }

    #[cfg(debug_assertions)]
    #[test]
    fn tune_setters_flow_into_uniforms_and_tick() {
        with_tune_test_lock(|| {
            set_pulse_freq(0.7);
            set_veil_freq(1.1);
            set_pulse_min(0.05);
            set_pulse_max(0.30);
            set_grain_gain(0.0);
            set_orb_speed(1.2);
            set_glow_hue(180.0);
            set_orb_min_x(-0.8);
            set_orb_max_x(0.8);
            set_orb_min_y(-0.5);
            set_orb_max_y(0.5);
            set_time_scale(0.0);
            assert!(tuning_active());
            let u = AmbientUniforms::from_theme(0.0, 0.0, 0.0, 1.0);
            assert_eq!(u.tune, [0.7, 1.1, 0.05, 0.30]);
            assert_eq!(u.tune2, [0.0, 1.2, 180.0, 0.0]);
            assert_eq!(u.tune3, [-0.8, 0.8, -0.5, 0.5]);
            // Frozen time scale stops the CPU clock (freeze toggle path).
            assert!(ambient_tick_dt().abs() < 1e-6);
            // Setters clamp to the Diagnostics slider ranges.
            set_pulse_freq(99.0);
            assert!((pulse_freq() - 5.0).abs() < 1e-6);
            set_pulse_max(99.0);
            assert!((pulse_max() - 0.6).abs() < 1e-6);
            set_orb_min_x(-99.0);
            assert!((orb_min_x() + 1.0).abs() < 1e-6);
        });
    }

    #[test]
    fn ambient_dt_for_elapsed_tracks_wall_clock_with_cap() {
        // Wall-clock mapping behind every ambient clock: exact under a frame,
        // clamped to one frame past a stall (never jumps), frozen at scale 0.
        with_tune_test_lock(|| {
            assert!((ambient_dt_for_elapsed(Duration::from_millis(16)) - 0.016).abs() < 1e-6);
            assert!((ambient_dt_for_elapsed(Duration::from_secs(10)) - 0.032).abs() < 1e-6);
            assert_eq!(ambient_dt_for_elapsed(Duration::ZERO), 0.0);
            #[cfg(debug_assertions)]
            {
                set_time_scale(0.0);
                assert_eq!(ambient_dt_for_elapsed(Duration::from_millis(16)), 0.0);
            }
        });
    }
}
