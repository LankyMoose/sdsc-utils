//! Soft rectangular edge vignette over immersive splash / atmosphere.

use iced::Rectangle;
use iced::mouse;
use iced::widget::shader::{self, Viewport};
use std::sync::atomic::{AtomicBool, Ordering};

static PIPELINE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Reset one-shot pipeline diag (process Exit) so the next run logs again.
pub fn reset_pipeline_diag() {
    PIPELINE_LOGGED.store(false, Ordering::Relaxed);
}

/// CPU-side uniforms (must match [`vignette.wgsl`] layout / alignment).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VignetteUniforms {
    pub strength: f32,
    pub _pad: [f32; 3],
}

impl VignetteUniforms {
    pub fn new(strength: f32) -> Self {
        Self {
            strength: strength.clamp(0.0, 1.0),
            _pad: [0.0; 3],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VignetteProgram {
    pub uniforms: VignetteUniforms,
}

impl VignetteProgram {
    pub fn new(strength: f32) -> Self {
        Self {
            uniforms: VignetteUniforms::new(strength),
        }
    }
}

impl<Message> shader::Program<Message> for VignetteProgram {
    type State = ();
    type Primitive = VignettePrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        VignettePrimitive {
            uniforms: self.uniforms,
        }
    }
}

#[derive(Debug)]
pub struct VignettePrimitive {
    uniforms: VignetteUniforms,
}

impl shader::Primitive for VignettePrimitive {
    type Pipeline = VignettePipeline;

    fn prepare(
        &self,
        pipeline: &mut VignettePipeline,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        pipeline.update(queue, &self.uniforms);
    }

    fn draw(&self, pipeline: &VignettePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, &pipeline.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}

pub struct VignettePipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl VignettePipeline {
    fn update(&self, queue: &wgpu::Queue, uniforms: &VignetteUniforms) {
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(uniforms));
    }
}

impl shader::Pipeline for VignettePipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        if !PIPELINE_LOGGED.swap(true, Ordering::Relaxed) {
            crate::controller::hid::diag::diag_info("ui-diag: shader vignette pipeline ready");
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("immersive vignette"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "vignette.wgsl"
            ))),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vignette uniforms"),
            size: std::mem::size_of::<VignetteUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vignette uniforms layout"),
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
            label: Some("vignette uniforms bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vignette pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vignette pipeline"),
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
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
    fn uniforms_clamp_strength() {
        let u = VignetteUniforms::new(2.0);
        assert_eq!(u.strength, 1.0);
        let u0 = VignetteUniforms::new(-1.0);
        assert_eq!(u0.strength, 0.0);
    }

    #[test]
    fn uniforms_size_is_16_bytes() {
        assert_eq!(std::mem::size_of::<VignetteUniforms>(), 16);
    }
}
