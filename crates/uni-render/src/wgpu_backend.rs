//! The concrete wgpu rendering backend.
//!
//! ## Frame as a render-graph
//!
//! Older versions drew straight to the swapchain in one pass. To support real
//! backdrop blur ([`DrawCmd::FrostedRect`]) the frame is now a small
//! render-graph that renders to an **offscreen scene texture** and only blits
//! to the surface at the very end:
//!
//! 1. Walk the [`Scene`] front-to-back, splitting it into **segments** at each
//!    `FrostedRect`. Each segment's `FilledRect`/`Text` commands are drawn into
//!    the offscreen `scene_tex` (rects through the flat-color pipeline, text via
//!    glyphon), exactly preserving painter's order.
//! 2. When a `FrostedRect` is reached, the *current* contents of `scene_tex`
//!    (everything drawn so far — the backdrop) are blurred: copied to a
//!    half-resolution texture, then run through a separable Gaussian blur
//!    (horizontal pass then vertical pass). The frosted panel is then composited
//!    back onto `scene_tex`: blurred backdrop clipped to the rounded rect, the
//!    translucent tint over it, and a subtle light inner edge.
//! 3. After all segments, `scene_tex` is blitted to the swapchain frame.
//!
//! Because the blur reads `scene_tex` as it stood *before* the panel, a
//! `FrostedRect` frosts whatever was painted before it and nothing after — the
//! painter's-algorithm contract the [`Scene`] promises.
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

use crate::blur;
use crate::color::Rgba;
use crate::renderer::{RenderError, Renderer};
use crate::scene::{DrawCmd, Scene};

/// Downsample factor for the blur backdrop. Blurring at half resolution on each
/// axis quarters the work and is invisible after a Gaussian — good for Intel
/// iGPUs. Must be >= 1.
const BLUR_DOWNSAMPLE: u32 = 2;

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

/// Uniform for the separable Gaussian blur (`frost.wgsl::fs_blur`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlurParams {
    texel_size: [f32; 2],
    direction: [f32; 2],
    sigma: f32,
    radius: i32,
    _pad: [f32; 2],
}

/// Uniform for the frosted-panel composite (`frost.wgsl::fs_frost`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FrostParams {
    rect_min: [f32; 2],
    rect_max: [f32; 2],
    resolution: [f32; 2],
    backdrop_res: [f32; 2],
    tint: [f32; 4],
    corner_radius: f32,
    edge: f32,
    _pad: [f32; 2],
}

/// An offscreen color target plus its view.
struct Target {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// A wgpu-backed [`Renderer`] that draws into a winit window surface.
pub struct WgpuRenderer {
    // Core wgpu state.
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// The (sRGB) format used for the surface *and* every offscreen target, so
    /// the flat pipeline / glyphon atlas / blit all agree.
    color_format: wgpu::TextureFormat,

    // Flat-color rect pipeline.
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,

    // Frosted-glass render-graph pipelines.
    sampler: wgpu::Sampler,
    tex_bgl: wgpu::BindGroupLayout,
    blur_uniform_bgl: wgpu::BindGroupLayout,
    frost_uniform_bgl: wgpu::BindGroupLayout,
    blur_pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    frost_pipeline: wgpu::RenderPipeline,

    // Offscreen targets, (re)allocated on resize.
    scene_tex: Target,
    blur_a: Target,
    blur_b: Target,

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

/// Create a sampleable + render-attachment offscreen color target.
fn make_target(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    label: &str,
) -> Target {
    let width = width.max(1);
    let height = height.max(1);
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Target {
        texture,
        view,
        width,
        height,
    }
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

        // --- Backend + adapter selection (Intel iGPU friendly) ---
        //
        // On Linux prefer Vulkan (Mesa ANV on Intel) and fall back to GL; on
        // other platforms let wgpu pick. Power preference defaults to LowPower
        // (great for UI on Intel Iris Xe / Arc), overridable via UNI_GPU_POWER.
        let backends = if cfg!(target_os = "linux") {
            wgpu::Backends::VULKAN | wgpu::Backends::GL
        } else {
            wgpu::Backends::all()
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| RenderError::Backend(format!("create_surface: {e}")))?;

        let power_preference = match std::env::var("UNI_GPU_POWER").ok().as_deref() {
            Some("high") | Some("HIGH") | Some("high_performance") => {
                wgpu::PowerPreference::HighPerformance
            }
            _ => wgpu::PowerPreference::LowPower,
        };

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| RenderError::Backend("no suitable GPU adapter found".into()))?;

        // Log the chosen adapter so users can confirm Vulkan/Intel selection.
        let info = adapter.get_info();
        eprintln!(
            "uni-render: adapter='{}' backend={:?} device_type={:?} power={:?} driver='{}' ({})",
            info.name, info.backend, info.device_type, power_preference, info.driver, info.driver_info
        );
        if info.name.to_lowercase().contains("intel") {
            eprintln!(
                "uni-render: Intel GPU detected ('{}') — using LowPower-friendly limits.",
                info.name
            );
        }

        // Conservative limits that Intel Iris Xe / Arc comfortably support.
        // Start from downlevel defaults (widest compatibility) and only raise
        // dimensions to what the adapter actually offers.
        let adapter_limits = adapter.limits();
        let mut required_limits = wgpu::Limits::downlevel_defaults();
        required_limits.max_texture_dimension_2d = adapter_limits
            .max_texture_dimension_2d
            .max(required_limits.max_texture_dimension_2d);

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("uni-render device"),
                    required_features: wgpu::Features::empty(),
                    required_limits,
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
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        let color_format = surface_format;

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
                    format: color_format,
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

        // --- frosted-glass render-graph pipelines ---
        let frost_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("uni-render frost shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("frost.wgsl"))),
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("uni-render linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // group(0): sampled texture + sampler (shared by blur/blit/frost).
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uni-render tex bgl"),
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
            ],
        });

        let uniform_entry = |size: u64| wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        };
        let blur_uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uni-render blur uniform bgl"),
            entries: &[uniform_entry(std::mem::size_of::<BlurParams>() as u64)],
        });
        let frost_uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uni-render frost uniform bgl"),
            entries: &[uniform_entry(std::mem::size_of::<FrostParams>() as u64)],
        });

        let blur_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("uni-render blur layout"),
            bind_group_layouts: &[&tex_bgl, &blur_uniform_bgl],
            push_constant_ranges: &[],
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("uni-render blit layout"),
            bind_group_layouts: &[&tex_bgl],
            push_constant_ranges: &[],
        });
        let frost_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("uni-render frost layout"),
            bind_group_layouts: &[&tex_bgl, &frost_uniform_bgl],
            push_constant_ranges: &[],
        });

        // Helper to build a fullscreen-triangle pipeline with a given fragment
        // entry point, target format, blend, and layout.
        let make_fs_pipeline = |label: &str,
                                layout: &wgpu::PipelineLayout,
                                fs_entry: &'static str,
                                format: wgpu::TextureFormat,
                                blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &frost_shader,
                    entry_point: "vs_fullscreen",
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &frost_shader,
                    entry_point: fs_entry,
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
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
            })
        };

        let blur_pipeline = make_fs_pipeline(
            "uni-render blur pipeline",
            &blur_layout,
            "fs_blur",
            color_format,
            None,
        );
        let blit_pipeline = make_fs_pipeline(
            "uni-render blit pipeline",
            &blit_layout,
            "fs_blit",
            color_format,
            None,
        );
        // Frost composites onto scene_tex with straight-alpha blending.
        let frost_pipeline = make_fs_pipeline(
            "uni-render frost pipeline",
            &frost_layout,
            "fs_frost",
            color_format,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );

        // Offscreen targets.
        let scene_tex = make_target(&device, color_format, width, height, "uni-render scene_tex");
        let bw = (width / BLUR_DOWNSAMPLE).max(1);
        let bh = (height / BLUR_DOWNSAMPLE).max(1);
        let blur_a = make_target(&device, color_format, bw, bh, "uni-render blur_a");
        let blur_b = make_target(&device, color_format, bw, bh, "uni-render blur_b");

        // --- text stack ---
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let glyphon_cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &glyphon_cache);
        let mut atlas = TextAtlas::new(&device, &queue, &glyphon_cache, color_format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            color_format,
            pipeline,
            globals_buffer,
            globals_bind_group,
            sampler,
            tex_bgl,
            blur_uniform_bgl,
            frost_uniform_bgl,
            blur_pipeline,
            blit_pipeline,
            frost_pipeline,
            scene_tex,
            blur_a,
            blur_b,
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

    /// Tessellate the given `FilledRect` commands into one vertex/index buffer.
    fn tessellate_rects(&self, cmds: &[&DrawCmd]) -> (Vec<Vertex>, Vec<u32>) {
        let mut geometry: VertexBuffers<Vertex, u32> = VertexBuffers::new();
        let mut tessellator = FillTessellator::new();

        for cmd in cmds {
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

    /// Bind a texture view as group(0) for the blur/blit/frost pipelines.
    fn tex_bind_group(&self, view: &wgpu::TextureView, label: &str) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.tex_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Run one fullscreen pass: draw 3 verts of `pipeline` into `target`,
    /// optionally clearing first, with the given bind groups.
    fn fullscreen_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        pipeline: &wgpu::RenderPipeline,
        bind_groups: &[&wgpu::BindGroup],
        clear: Option<wgpu::Color>,
        label: &str,
    ) {
        let load = match clear {
            Some(c) => wgpu::LoadOp::Clear(c),
            None => wgpu::LoadOp::Load,
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        for (i, bg) in bind_groups.iter().enumerate() {
            pass.set_bind_group(i as u32, *bg, &[]);
        }
        pass.draw(0..3, 0..1);
    }

    /// Blur the current contents of `scene_tex` into `blur_b` (down/upsampled).
    /// Returns a bind group over the blurred result, sized to the full surface
    /// (the frost pass samples it in shared 0..1 UVs).
    ///
    /// Pipeline: scene_tex -> (downsample copy) blur_a -> H blur -> blur_b
    ///           -> V blur -> blur_a (final blurred backdrop at blur res).
    fn blur_backdrop(&self, encoder: &mut wgpu::CommandEncoder, blur_radius: f32) {
        // Blur math (radius expressed in blur-target px).
        let blur_radius_bt = blur_radius / BLUR_DOWNSAMPLE as f32;
        let sigma = blur::sigma_for_radius(blur_radius_bt);
        let radius = blur::taps_for_sigma(sigma);

        let bw = self.blur_a.width as f32;
        let bh = self.blur_a.height as f32;

        // 1) Downsample scene_tex -> blur_a via the blit pipeline (linear filter
        //    averages, giving a cheap first blur).
        let scene_bg = self.tex_bind_group(&self.scene_tex.view, "blur src scene");
        self.fullscreen_pass(
            encoder,
            &self.blur_a.view,
            &self.blit_pipeline,
            &[&scene_bg],
            Some(wgpu::Color::TRANSPARENT),
            "uni-render downsample",
        );

        // 2) Horizontal blur: blur_a -> blur_b.
        let h_params = BlurParams {
            texel_size: [1.0 / bw, 1.0 / bh],
            direction: [1.0, 0.0],
            sigma,
            radius,
            _pad: [0.0, 0.0],
        };
        let h_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("blur h params"),
            contents: bytemuck::bytes_of(&h_params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let h_uniform_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blur h uniform"),
            layout: &self.blur_uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: h_buf.as_entire_binding(),
            }],
        });
        let a_bg = self.tex_bind_group(&self.blur_a.view, "blur a src");
        self.fullscreen_pass(
            encoder,
            &self.blur_b.view,
            &self.blur_pipeline,
            &[&a_bg, &h_uniform_bg],
            Some(wgpu::Color::TRANSPARENT),
            "uni-render blur H",
        );

        // 3) Vertical blur: blur_b -> blur_a (final blurred backdrop).
        let v_params = BlurParams {
            texel_size: [1.0 / bw, 1.0 / bh],
            direction: [0.0, 1.0],
            sigma,
            radius,
            _pad: [0.0, 0.0],
        };
        let v_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("blur v params"),
            contents: bytemuck::bytes_of(&v_params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let v_uniform_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blur v uniform"),
            layout: &self.blur_uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: v_buf.as_entire_binding(),
            }],
        });
        let b_bg = self.tex_bind_group(&self.blur_b.view, "blur b src");
        self.fullscreen_pass(
            encoder,
            &self.blur_a.view,
            &self.blur_pipeline,
            &[&b_bg, &v_uniform_bg],
            Some(wgpu::Color::TRANSPARENT),
            "uni-render blur V",
        );
        // Final blurred backdrop now lives in self.blur_a.
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

        // Reallocate offscreen targets to the new physical size.
        self.scene_tex =
            make_target(&self.device, self.color_format, width, height, "uni-render scene_tex");
        let bw = (width / BLUR_DOWNSAMPLE).max(1);
        let bh = (height / BLUR_DOWNSAMPLE).max(1);
        self.blur_a = make_target(&self.device, self.color_format, bw, bh, "uni-render blur_a");
        self.blur_b = make_target(&self.device, self.color_format, bw, bh, "uni-render blur_b");
    }

    fn render(&mut self, scene: &Scene) -> Result<(), RenderError> {
        // Background = first full-cover FilledRect, else transparent.
        let clear = scene
            .iter()
            .find_map(|cmd| match cmd {
                DrawCmd::FilledRect {
                    x, y, w, h, color, ..
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
                let l = c.to_linear_array();
                wgpu::Color {
                    r: l[0] as f64,
                    g: l[1] as f64,
                    b: l[2] as f64,
                    a: l[3] as f64,
                }
            })
            .unwrap_or(wgpu::Color::TRANSPARENT);

        // Update glyphon viewport with the PHYSICAL resolution.
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        // --- acquire frame ---
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                return Err(RenderError::SurfaceLost)
            }
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(RenderError::OutOfMemory),
            Err(wgpu::SurfaceError::Timeout) => return Err(RenderError::Transient),
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let scale = self.scale_factor as f32;
        let phys_w = self.config.width as f32;
        let phys_h = self.config.height as f32;

        // Split the scene into segments at each FrostedRect. Each segment is the
        // run of FilledRect/Text commands preceding a frosted panel (the last
        // segment has `frost: None`).
        struct Segment<'a> {
            draws: Vec<&'a DrawCmd>,
            frost: Option<&'a DrawCmd>,
        }
        let mut segments: Vec<Segment> = Vec::new();
        let mut cur: Vec<&DrawCmd> = Vec::new();
        for cmd in scene {
            match cmd {
                DrawCmd::FrostedRect { .. } => {
                    segments.push(Segment {
                        draws: std::mem::take(&mut cur),
                        frost: Some(cmd),
                    });
                }
                other => cur.push(other),
            }
        }
        segments.push(Segment {
            draws: cur,
            frost: None,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("uni-render encoder"),
            });

        let mut first_pass = true;
        // glyphon `prepare` mutates atlas; keep TextBuffers alive per segment.
        for seg in &segments {
            // 1) Draw this segment's rects + text into scene_tex.
            let (vertices, indices) = self.tessellate_rects(&seg.draws);
            let vertex_buffer =
                self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("uni-render vertices"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
            let index_buffer =
                self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("uni-render indices"),
                    contents: bytemuck::cast_slice(&indices),
                    usage: wgpu::BufferUsages::INDEX,
                });

            // Shape text for this segment.
            let mut text_buffers: Vec<(TextBuffer, f32, f32, GlyphColor)> = Vec::new();
            for cmd in &seg.draws {
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

            let has_text = !text_areas.is_empty();
            if has_text {
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
            }

            {
                let load = if first_pass {
                    wgpu::LoadOp::Clear(clear)
                } else {
                    wgpu::LoadOp::Load
                };
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("uni-render scene pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.scene_tex.view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

                if !indices.is_empty() {
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &self.globals_bind_group, &[]);
                    pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                    pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
                }

                if has_text {
                    self.text_renderer
                        .render(&self.atlas, &self.viewport, &mut pass)
                        .map_err(|e| RenderError::Backend(format!("glyphon render: {e}")))?;
                }
            }
            first_pass = false;

            // 2) If this segment ends with a frosted panel, blur the backdrop
            //    (scene_tex as it stands now) and composite the panel onto it.
            if let Some(DrawCmd::FrostedRect {
                x,
                y,
                w,
                h,
                corner_radius,
                tint,
                blur_radius,
            }) = seg.frost
            {
                if *w <= 0.0 || *h <= 0.0 {
                    continue;
                }

                // Blur scene_tex -> self.blur_a.
                self.blur_backdrop(&mut encoder, *blur_radius);

                // Panel rect in PHYSICAL pixels (scene_tex / frag space).
                let px = *x * scale;
                let py = *y * scale;
                let pw = *w * scale;
                let ph = *h * scale;
                let radius_px = (corner_radius.max(0.0) * scale).min(pw.min(ph) / 2.0);

                let tint_rgba = Rgba::from_u32(*tint);
                let tint_linear = tint_rgba.to_linear_array();

                let params = FrostParams {
                    rect_min: [px, py],
                    rect_max: [px + pw, py + ph],
                    resolution: [phys_w, phys_h],
                    backdrop_res: [self.blur_a.width as f32, self.blur_a.height as f32],
                    tint: tint_linear,
                    corner_radius: radius_px,
                    // Light inner edge strength scaled by tint alpha presence.
                    edge: 0.12,
                    _pad: [0.0, 0.0],
                };
                let params_buf =
                    self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("frost params"),
                        contents: bytemuck::bytes_of(&params),
                        usage: wgpu::BufferUsages::UNIFORM,
                    });
                let frost_uniform_bg =
                    self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("frost uniform"),
                        layout: &self.frost_uniform_bgl,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: params_buf.as_entire_binding(),
                        }],
                    });
                let blurred_bg = self.tex_bind_group(&self.blur_a.view, "frost backdrop");

                // Composite the panel onto scene_tex (alpha blend, no clear).
                self.fullscreen_pass(
                    &mut encoder,
                    &self.scene_tex.view,
                    &self.frost_pipeline,
                    &[&blurred_bg, &frost_uniform_bg],
                    None,
                    "uni-render frost composite",
                );
            }
        }

        // Final: blit scene_tex -> swapchain frame.
        let scene_bg = self.tex_bind_group(&self.scene_tex.view, "final blit src");
        self.fullscreen_pass(
            &mut encoder,
            &frame_view,
            &self.blit_pipeline,
            &[&scene_bg],
            Some(wgpu::Color::TRANSPARENT),
            "uni-render final blit",
        );

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        self.atlas.trim();

        Ok(())
    }
}
