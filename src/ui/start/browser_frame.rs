//! Stable GPU texture for the browser page frame.
//!
//! Pattern from `valorant-companion`'s video preview: a custom
//! `iced_wgpu` primitive owns one RGBA texture per tab and uploads it with
//! `queue.write_texture` only when the slot's generation changes. Minting a
//! fresh `iced::image::Handle` per frame (the old path) guarantees an atlas
//! cache miss — full allocation + upload + evict every frame — and the page
//! visibly blinks at wide viewports because the raster worker cannot keep
//! up. The slot bytes never become an iced image handle.

use crate::browser::frame_slot::{FrameData, FrameSlot};
use iced::Rectangle;
use iced_wgpu::primitive::{Pipeline, Primitive};
use iced_wgpu::wgpu;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// NDC quad placement. The shader reads only `rect`; the pad keeps the
/// uniform block at 32 bytes.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    rect: [f32; 4],
    _pad: [f32; 4],
}

struct FrameEntry {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    dims: (u32, u32),
    uploaded_gen: u64,
    alive: Arc<AtomicBool>,
}

pub struct BrowserFramePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    frames: BTreeMap<u64, FrameEntry>,
}

impl Pipeline for BrowserFramePipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("browser frame shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("browser_frame.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("browser frame bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("browser frame pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("browser frame pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("browser frame sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            frames: BTreeMap::new(),
        }
    }

    fn trim(&mut self) {
        let dead: Vec<u64> = self
            .frames
            .iter()
            .filter_map(|(id, e)| {
                (!e.alive.load(std::sync::atomic::Ordering::SeqCst)).then_some(*id)
            })
            .collect();
        for id in dead {
            if let Some(entry) = self.frames.remove(&id) {
                entry.texture.destroy();
                entry.uniforms.destroy();
            }
        }
    }
}

impl BrowserFramePipeline {
    fn ensure_entry(
        &mut self,
        device: &wgpu::Device,
        id: u64,
        alive: &Arc<AtomicBool>,
        w: u32,
        h: u32,
    ) -> Option<&mut FrameEntry> {
        let needs_new = self
            .frames
            .get(&id)
            .is_none_or(|entry| entry.dims != (w, h));
        if needs_new {
            if let Some(old) = self.frames.remove(&id) {
                old.texture.destroy();
                old.uniforms.destroy();
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("browser frame texture"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // The engine's software output is already sRGB-encoded RGBA.
                // iced renders into a non-sRGB target and its own image atlas
                // is plain `Rgba8Unorm` too; declaring this texture sRGB made
                // the hardware linearize the samples, and the shader then
                // wrote those linear values raw — visibly darkening every
                // midtone (a #808080 page read back as #373737). Plain Unorm
                // passes the bytes through unchanged.
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("browser frame uniforms"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("browser frame bind group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &uniforms,
                            offset: 0,
                            size: None,
                        }),
                    },
                ],
            });
            self.frames.insert(
                id,
                FrameEntry {
                    texture,
                    bind_group,
                    uniforms,
                    dims: (w, h),
                    uploaded_gen: u64::MAX,
                    alive: Arc::clone(alive),
                },
            );
        }
        self.frames.get_mut(&id)
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: u64,
        alive: &Arc<AtomicBool>,
        generation: u64,
        frame: &FrameData,
    ) {
        let Some(entry) = self.ensure_entry(device, id, alive, frame.width, frame.height) else {
            return;
        };
        if entry.uploaded_gen == generation {
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &entry.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
        entry.uploaded_gen = generation;
    }

    fn prepare(&self, queue: &wgpu::Queue, id: u64, rect_ndc: [f32; 4]) {
        let Some(entry) = self.frames.get(&id) else {
            return;
        };
        let uniforms = Uniforms {
            rect: rect_ndc,
            _pad: [0.0; 4],
        };
        queue.write_buffer(&entry.uniforms, 0, bytemuck::bytes_of(&uniforms));
    }

    fn draw(
        &self,
        target: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
        clip: &Rectangle<u32>,
        id: u64,
    ) {
        let Some(entry) = self.frames.get(&id) else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("browser frame render pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &entry.bind_group, &[]);
        pass.set_scissor_rect(clip.x, clip.y, clip.width, clip.height);
        pass.draw(0..6, 0..1);
    }
}

#[derive(Debug, Clone)]
pub struct BrowserFramePrimitive {
    id: u64,
    alive: Arc<AtomicBool>,
    slot: Arc<FrameSlot>,
    upload: bool,
}

impl BrowserFramePrimitive {
    fn new(id: u64, alive: Arc<AtomicBool>, slot: Arc<FrameSlot>, upload: bool) -> Self {
        Self {
            id,
            alive,
            slot,
            upload,
        }
    }
}

impl Primitive for BrowserFramePrimitive {
    type Pipeline = BrowserFramePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &iced_wgpu::graphics::Viewport,
    ) {
        if self.upload {
            let generation = self.slot.generation();
            self.slot.with_frame(|frame| {
                pipeline.upload(device, queue, self.id, &self.alive, generation, frame);
            });
        }
        let ndc = *bounds
            * iced::Transformation::orthographic(
                viewport.logical_size().width as _,
                viewport.logical_size().height as _,
            );
        pipeline.prepare(
            queue,
            self.id,
            [ndc.x, ndc.y, ndc.x + ndc.width, ndc.y + ndc.height],
        );
    }

    fn render(
        &self,
        pipeline: &Self::Pipeline,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        clip_bounds: &Rectangle<u32>,
    ) {
        pipeline.draw(target, encoder, clip_bounds, self.id);
    }
}

/// `shader`-widget program for the browser page. Frames are uploaded from
/// the shared slot in `prepare` (unseen generations only); the app's frame
/// events rebuild the view, so `update` only asks for a redraw when the
/// slot advanced between builds.
pub struct BrowserFrameProgram {
    id: u64,
    slot: Arc<FrameSlot>,
    alive: Arc<AtomicBool>,
}

impl BrowserFrameProgram {
    pub fn new(slot: Arc<FrameSlot>, id: u64) -> Self {
        let alive = slot.alive();
        Self { id, slot, alive }
    }
}

#[derive(Debug, Default)]
pub struct BrowserFrameState {
    last_seen: std::sync::atomic::AtomicU64,
    last_id: std::sync::atomic::AtomicU64,
}

impl<Message> iced::widget::shader::Program<Message> for BrowserFrameProgram {
    type State = BrowserFrameState;
    type Primitive = BrowserFramePrimitive;

    fn update(
        &self,
        state: &mut Self::State,
        _event: &iced::Event,
        _bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<iced::widget::Action<Message>> {
        // Pull-model redraw clock: the browser owns a 60Hz presentation
        // loop while mounted (no per-frame engine messages or view
        // rebuilds). A new generation presents immediately; otherwise the
        // loop polls again next frame period.
        if self.slot.generation() != state.last_seen.load(std::sync::atomic::Ordering::SeqCst) {
            Some(iced::widget::Action::request_redraw())
        } else {
            Some(iced::widget::Action::request_redraw_at(
                iced::time::Instant::now() + iced::time::Duration::from_millis(16),
            ))
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        _cursor: iced::mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        use std::sync::atomic::Ordering;
        let generation = self.slot.generation();
        let same_id = state.last_id.load(Ordering::SeqCst) == self.id;
        let upload = self.slot.has_frame()
            && (!same_id || generation != state.last_seen.load(Ordering::SeqCst));
        state.last_seen.store(generation, Ordering::SeqCst);
        state.last_id.store(self.id, Ordering::SeqCst);
        BrowserFramePrimitive::new(
            self.id,
            Arc::clone(&self.alive),
            Arc::clone(&self.slot),
            upload,
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn browser_frame_shader_parses_and_validates() {
        // The pipeline builds on the first real frame with no headless
        // coverage; parse + validate the exact shipped WGSL here.
        let src = include_str!("browser_frame.wgsl");
        let module = naga::front::wgsl::parse_str(src).expect("WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("WGSL validates");
    }
}
