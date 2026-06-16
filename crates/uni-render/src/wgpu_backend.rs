//! The concrete wgpu rendering backend.
//!
//! Pipeline per frame:
//! 1. Walk the [`Scene`]; for each [`DrawCmd::FilledRect`] tessellate a rounded
//!    rectangle with `lyon` into a vertex/index buffer (position + linear RGBA).
//! 2. For each [`DrawCmd::Text`], shape/lay out a `cosmic-text` [`Buffer`] and
//!    feed it to `glyphon`'s [`TextRenderer`] (which owns the glyph atlas).
//! 3. Open one render pass: clear to the requested background, draw the rect
//!    triangles through our flat-color pipeline, then draw the text on top.
//!
//! Everything works in **logical pixels**. The orthographic projection in
//! `shader.wgsl` maps logical px -> clip space; glyphon is told to scale by the
//! window's `scale_factor` so text lands on the same logical grid.

use std::borrow::Cow;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use lyon::math::{Box2D, Point};
use lyon::path::builder::BorderRadii;
use lyon::path::Winding;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, VertexBuffers,
};
use wgpu::util::DeviceExt;

use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics,
    Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};

use crate::color::Rgba;
use crate::renderer::{RenderError, Renderer};
use crate::scene::{DrawCmd, Scene};

/// A vertex for the flat-color triangle pipeline: logical-px position + linear RGBA.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

/// The orthographic-projection uniform (column-major 4x4).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    proj: [[f32; 4]; 4],
}

impl Globals {
    /// Top-left-origin, y-down orthographic projection from
    /// `(0,0)..(w,h)` logical pixels to clip space `[-1,1]`.
    fn ortho(w: f32, h: f32) -> Self {
        let w = w.max(1.0);
        let h = h.max(1.0);
        // x: [0,w] -> [-1,1] ; y: [0,h] -> [1,-1] (flip for y-down)
        Self {
            proj: [
                [2.0 / w, 0.0, 0.0, 0.0],
                [0.0, -2.0 / h, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [-1.0, 1.0, 0.0, 1.0],
            ],
        }
    }
}

/// A wgpu-backed [`Renderer`] that draws into a winit window surface.
pub struct WgpuRenderer {
    // Core wgpu state.
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    // Flat-color rect pipeline.
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,

    // Logical size + HiDPI scale.
    logical_width: f32,
    logical_height: f32,
    scale_factor: f64,

    // glyphon / cosmic-text text stack.
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
}

impl WgpuRenderer {
    /// Build a renderer for the given winit window. `window` is kept alive for
    /// the lifetime of the surface via `Arc`.
    ///
    /// Blocks on adapter/device acquisition with `pollster`.
    pub fn new(window: Arc<winit::window::Window>) -> Result<Self, RenderError> {
        pollster::block_on(Self::new_async(window))
    }

    async fn new_async(window: Arc<winit::window::Window>) -> Result<Self, RenderError> {
        let phys = window.inner_size();
        let scale_factor = window.scale_factor();
        let width = phys.width.max(1);
        let height = phys.height.max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| RenderError::Backend(format!("create_surface: {e}")))?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| RenderError::Backend("no suitable GPU adapter found".into()))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("uni-render device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(|e| RenderError::Backend(format!("request_device: {e}")))?;

        let surface_caps = surface.get_capabilities(&adapter);
        // Prefer an sRGB surface so our linear-light vertex colors encode right.
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(surface_caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let logical_width = width as f32 / scale_factor as f32;
        let logical_height = height as f32 / scale_factor as f32;

        // --- flat-color rect pipeline ---
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("uni-render flat shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("shader.wgsl"))),
        });

        let globals = Globals::ortho(logical_width, logical_height);
        let globals_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uni-render globals"),
            contents: bytemuck::bytes_of(&globals),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let globals_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("uni-render globals layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uni-render globals bind group"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("uni-render pipeline layout"),
                bind_group_layouts: &[&globals_layout],
                push_constant_ranges: &[],
            });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("uni-render flat pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[Vertex::layout()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // --- text stack ---
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let glyphon_cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &glyphon_cache);
        let mut atlas = TextAtlas::new(&device, &queue, &glyphon_cache, surface_format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            globals_buffer,
            globals_bind_group,
            logical_width,
            logical_height,
            scale_factor,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
        })
    }

    /// Tessellate every [`DrawCmd::FilledRect`] in the scene into one shared
    /// vertex/index buffer. Returns `(vertices, indices)`.
    fn tessellate_rects(&self, scene: &Scene) -> (Vec<Vertex>, Vec<u32>) {
        let mut geometry: VertexBuffers<Vertex, u32> = VertexBuffers::new();
        let mut tessellator = FillTessellator::new();

        for cmd in scene {
            if let DrawCmd::FilledRect {
                x,
                y,
                w,
                h,
                color,
                corner_radius,
            } = cmd
            {
                if *w <= 0.0 || *h <= 0.0 {
                    continue;
                }
                let linear = Rgba::from_u32(*color).to_linear_array();
                let radius = corner_radius.max(0.0).min(w.min(*h) / 2.0);

                let mut builder = lyon::path::Path::builder();
                let rect = Box2D::new(Point::new(*x, *y), Point::new(*x + *w, *y + *h));
                if radius > 0.0 {
                    builder.add_rounded_rectangle(
                        &rect,
                        &BorderRadii::new(radius),
                        Winding::Positive,
                    );
                } else {
                    builder.add_rectangle(&rect, Winding::Positive);
                }
                let path = builder.build();

                tessellator
                    .tessellate_path(
                        &path,
                        &FillOptions::tolerance(0.1),
                        &mut BuffersBuilder::new(&mut geometry, move |v: FillVertex| Vertex {
                            pos: v.position().to_array(),
                            color: linear,
                        }),
                    )
                    .expect("lyon tessellation failed");
            }
        }

        (geometry.vertices, geometry.indices)
    }
}

impl Renderer for WgpuRenderer {
    fn resize(&mut self, width: u32, height: u32, scale_factor: f64) {
        let width = width.max(1);
        let height = height.max(1);
        self.scale_factor = scale_factor;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);

        self.logical_width = width as f32 / scale_factor as f32;
        self.logical_height = height as f32 / scale_factor as f32;

        let globals = Globals::ortho(self.logical_width, self.logical_height);
        self.queue
            .write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
    }

    fn render(&mut self, scene: &Scene) -> Result<(), RenderError> {
        // Background = first FilledRect that covers the full logical viewport,
        // else fully transparent. (The example issues a full-screen rect.)
        let clear = scene
            .iter()
            .find_map(|cmd| match cmd {
                DrawCmd::FilledRect {
                    x,
                    y,
                    w,
                    h,
                    color,
                    ..
                } if *x <= 0.0
                    && *y <= 0.0
                    && *w >= self.logical_width
                    && *h >= self.logical_height =>
                {
                    Some(Rgba::from_u32(*color))
                }
                _ => None,
            })
            .map(|c| {
                // The surface is sRGB; wgpu's clear color is interpreted as
                // linear and re-encoded, so feed linear here.
                let l = c.to_linear_array();
                wgpu::Color {
                    r: l[0] as f64,
                    g: l[1] as f64,
                    b: l[2] as f64,
                    a: l[3] as f64,
                }
            })
            .unwrap_or(wgpu::Color::TRANSPARENT);

        // --- tessellate rects ---
        let (vertices, indices) = self.tessellate_rects(scene);
        let vertex_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uni-render vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uni-render indices"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        // --- shape text buffers ---
        let scale = self.scale_factor as f32;
        let mut text_buffers: Vec<(TextBuffer, f32, f32, GlyphColor)> = Vec::new();
        for cmd in scene {
            if let DrawCmd::Text {
                x,
                y,
                content,
                size,
                color,
            } = cmd
            {
                let metrics = Metrics::new(*size, *size * 1.2);
                let mut buffer = TextBuffer::new(&mut self.font_system, metrics);
                // Lay out in logical px; glyphon scales by `scale` at draw time,
                // so set the layout bounds in logical px too.
                buffer.set_size(
                    &mut self.font_system,
                    Some(self.logical_width.max(1.0)),
                    Some(self.logical_height.max(1.0)),
                );
                buffer.set_text(
                    &mut self.font_system,
                    content,
                    Attrs::new().family(Family::SansSerif),
                    Shaping::Advanced,
                );
                buffer.shape_until_scroll(&mut self.font_system, false);

                let rgba = Rgba::from_u32(*color);
                let gc = GlyphColor::rgba(
                    (rgba.r * 255.0) as u8,
                    (rgba.g * 255.0) as u8,
                    (rgba.b * 255.0) as u8,
                    (rgba.a * 255.0) as u8,
                );
                text_buffers.push((buffer, *x, *y, gc));
            }
        }

        // Update glyphon viewport with the PHYSICAL resolution.
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        let text_areas: Vec<TextArea> = text_buffers
            .iter()
            .map(|(buffer, x, y, gc)| TextArea {
                buffer,
                left: *x * scale,
                top: *y * scale,
                scale,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: self.config.width as i32,
                    bottom: self.config.height as i32,
                },
                default_color: *gc,
                custom_glyphs: &[],
            })
            .collect();

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .map_err(|e| RenderError::Backend(format!("glyphon prepare: {e}")))?;

        // --- acquire frame ---
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                return Err(RenderError::SurfaceLost)
            }
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(RenderError::OutOfMemory),
            Err(wgpu::SurfaceError::Timeout) => return Err(RenderError::Transient),
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("uni-render encoder"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("uni-render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Rects.
            if !indices.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.globals_bind_group, &[]);
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
            }

            // Text on top.
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .map_err(|e| RenderError::Backend(format!("glyphon render: {e}")))?;
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        self.atlas.trim();

        Ok(())
    }
}
