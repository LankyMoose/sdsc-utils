//! Full-bleed immersive atmosphere via iced's custom shader widget.

use crate::ui::theme;
use iced::mouse;
use iced::widget::shader::{self, Viewport};
use iced::{Color, Rectangle};
use std::sync::atomic::{AtomicBool, Ordering};

static PIPELINE_LOGGED: AtomicBool = AtomicBool::new(false);

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
        }
    }
}

fn color4(c: Color) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

#[derive(Debug, Clone, Copy)]
pub struct AmbientProgram {
    pub uniforms: AmbientUniforms,
}

impl AmbientProgram {
    pub fn new(time: f32, dock_progress: f32, veil: f32, aperture: f32) -> Self {
        Self {
            uniforms: AmbientUniforms::from_theme(time, dock_progress, veil, aperture),
        }
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
        }
    }
}

#[derive(Debug)]
pub struct AmbientPrimitive {
    uniforms: AmbientUniforms,
}

impl shader::Primitive for AmbientPrimitive {
    type Pipeline = AmbientPipeline;

    fn prepare(
        &self,
        pipeline: &mut AmbientPipeline,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        pipeline.update(queue, &self.uniforms);
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
    fn uniforms_size_is_64_bytes() {
        assert_eq!(std::mem::size_of::<AmbientUniforms>(), 64);
    }
}
