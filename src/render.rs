//! wgpu: one device for the process, one swapchain per output, an immediate-mode
//! draw list of rounded quads (solid or textured) plus glyphon text, and a dual
//! Kawase blur for the background.

use glyphon::{Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight};
use std::ptr::NonNull;

pub const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
pub const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const BLUR_LEVELS: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Quad {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    radius: f32,
    border: f32,
    padding: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    padding: [f32; 2],
}

#[derive(Clone)]
pub struct Image {
    pub texture: wgpu::Texture,
    pub bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
    text_cache: Cache,
    image_layout: wgpu::BindGroupLayout,
    uniform_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    quad_pipeline: wgpu::RenderPipeline,
    blur_down_pipeline: wgpu::RenderPipeline,
    blur_up_pipeline: wgpu::RenderPipeline,
    pub white: Image,
}

pub struct Target {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    viewport: Viewport,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
}

/// One frame's worth of drawing. Coordinates are physical pixels.
#[derive(Default)]
pub struct DrawList {
    quads: Vec<Quad>,
    batches: Vec<(Option<wgpu::BindGroup>, std::ops::Range<u32>)>,
    texts: Vec<(Buffer, f32, f32, [f32; 4], [u8; 4])>,
}

pub fn rgba(hex: u32, alpha: f32) -> [f32; 4] {
    let channel = |shift: u32| ((hex >> shift) & 0xff) as f32 / 255.0 * alpha;
    [channel(16), channel(8), channel(0), alpha]
}

impl DrawList {
    fn push(&mut self, image: Option<&Image>, quad: Quad) {
        let index = self.quads.len() as u32;
        self.quads.push(quad);
        let bind_group = image.map(|image| image.bind_group.clone());
        match self.batches.last_mut() {
            Some((last, range)) if *last == bind_group => range.end = index + 1,
            _ => self.batches.push((bind_group, index..index + 1)),
        }
    }

    /// Premultiplied color.
    pub fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: [f32; 4], radius: f32) {
        self.push(None, Quad { rect: [x, y, width, height], uv: [0.0, 0.0, 1.0, 1.0], color, radius, border: 0.0, padding: [0.0; 2] });
    }

    pub fn outline(&mut self, x: f32, y: f32, width: f32, height: f32, color: [f32; 4], radius: f32, thickness: f32) {
        self.push(None, Quad { rect: [x, y, width, height], uv: [0.0, 0.0, 1.0, 1.0], color, radius, border: thickness, padding: [0.0; 2] });
    }

    /// `uv` is (u, v, width, height) in normalized texture coordinates; `tint` multiplies.
    pub fn image(&mut self, image: &Image, x: f32, y: f32, width: f32, height: f32, uv: [f32; 4], tint: [f32; 4], radius: f32) {
        self.push(Some(image), Quad { rect: [x, y, width, height], uv, color: tint, radius, border: 0.0, padding: [0.0; 2] });
    }

    /// Text clipped to `clip` (x, y, width, height). The buffer comes from `Gpu::text`.
    pub fn text(&mut self, buffer: Buffer, x: f32, y: f32, clip: [f32; 4], color: [u8; 4]) {
        self.texts.push((buffer, x, y, clip, color));
    }
}

pub fn text_width(buffer: &Buffer) -> f32 {
    buffer.layout_runs().map(|run| run.line_w).fold(0.0, f32::max)
}

const SHADER: &str = r#"
struct Uniforms { viewport: vec2<f32>, padding: vec2<f32> };
@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var image: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

struct Instance {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) shape: vec4<f32>,
};
struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) local: vec2<f32>,
    @location(3) half_size: vec2<f32>,
    @location(4) shape: vec2<f32>,
};

@vertex
fn vertex_main(@builtin(vertex_index) vertex: u32, instance: Instance) -> Varyings {
    var corners = array<vec2<f32>, 6>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let corner = corners[vertex];
    let pixel = instance.rect.xy + corner * instance.rect.zw;
    var out: Varyings;
    out.position = vec4(pixel / uniforms.viewport * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
    out.uv = instance.uv.xy + corner * instance.uv.zw;
    out.color = instance.color;
    out.half_size = instance.rect.zw * 0.5;
    out.local = (corner - 0.5) * instance.rect.zw;
    out.shape = instance.shape.xy;
    return out;
}

fn rounded_box(point: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let q = abs(point) - half_size + radius;
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

@fragment
fn fragment_main(in: Varyings) -> @location(0) vec4<f32> {
    let radius = min(in.shape.x, min(in.half_size.x, in.half_size.y));
    let distance = rounded_box(in.local, in.half_size, radius);
    var coverage = clamp(0.5 - distance, 0.0, 1.0);
    if (in.shape.y > 0.0) {
        coverage = coverage * clamp(distance + in.shape.y + 0.5, 0.0, 1.0);
    }
    return textureSample(image, image_sampler, in.uv) * in.color * coverage;
}

// Dual Kawase. The source texel size goes in uniforms.viewport.
struct BlurVaryings { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn blur_vertex(@builtin(vertex_index) vertex: u32) -> BlurVaryings {
    let uv = vec2(f32((vertex << 1u) & 2u), f32(vertex & 2u));
    var out: BlurVaryings;
    out.position = vec4(uv * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
    out.uv = uv;
    return out;
}
@fragment
fn blur_down(in: BlurVaryings) -> @location(0) vec4<f32> {
    let texel = uniforms.viewport;
    var sum = textureSample(image, image_sampler, in.uv) * 4.0;
    sum += textureSample(image, image_sampler, in.uv - texel);
    sum += textureSample(image, image_sampler, in.uv + texel);
    sum += textureSample(image, image_sampler, in.uv + vec2(texel.x, -texel.y));
    sum += textureSample(image, image_sampler, in.uv - vec2(texel.x, -texel.y));
    return sum / 8.0;
}
@fragment
fn blur_up(in: BlurVaryings) -> @location(0) vec4<f32> {
    let texel = uniforms.viewport;
    var sum = textureSample(image, image_sampler, in.uv + vec2(-texel.x * 2.0, 0.0));
    sum += textureSample(image, image_sampler, in.uv + vec2(-texel.x, texel.y)) * 2.0;
    sum += textureSample(image, image_sampler, in.uv + vec2(0.0, texel.y * 2.0));
    sum += textureSample(image, image_sampler, in.uv + vec2(texel.x, texel.y)) * 2.0;
    sum += textureSample(image, image_sampler, in.uv + vec2(texel.x * 2.0, 0.0));
    sum += textureSample(image, image_sampler, in.uv + vec2(texel.x, -texel.y)) * 2.0;
    sum += textureSample(image, image_sampler, in.uv + vec2(0.0, -texel.y * 2.0));
    sum += textureSample(image, image_sampler, in.uv + vec2(-texel.x, -texel.y)) * 2.0;
    return sum / 12.0;
}
"#;

fn create_texture(device: &wgpu::Device, width: u32, height: u32, render_target: bool) -> wgpu::Texture {
    let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    if render_target {
        usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: IMAGE_FORMAT,
        usage,
        view_formats: &[],
    })
}

fn bind_image(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, texture: wgpu::Texture) -> Image {
    let view = texture.create_view(&Default::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) }],
    });
    Image { width: texture.width(), height: texture.height(), texture, bind_group }
}

pub fn raw_handles(display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) -> wgpu::SurfaceTargetUnsafe {
    wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(raw_window_handle::RawDisplayHandle::Wayland(raw_window_handle::WaylandDisplayHandle::new(NonNull::new(display).unwrap()))),
        raw_window_handle: raw_window_handle::RawWindowHandle::Wayland(raw_window_handle::WaylandWindowHandle::new(NonNull::new(surface).unwrap())),
    }
}

impl Gpu {
    pub fn new(display: *mut std::ffi::c_void, probe_surface: *mut std::ffi::c_void) -> Gpu {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let probe = unsafe { instance.create_surface_unsafe(raw_handles(display, probe_surface)).expect("create surface") };
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&probe),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .expect("no Vulkan adapter");
        drop(probe);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("request device");
        log::info!("GPU: {}", adapter.get_info().name);

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("overlay"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&uniform_layout), Some(&image_layout)],
            immediate_size: 0,
        });
        let blend = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quads"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Quad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fragment_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: SURFACE_FORMAT, blend, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let blur_pipeline = |entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&layout),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("blur_vertex"), compilation_options: Default::default(), buffers: &[] },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: IMAGE_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let blur_down_pipeline = blur_pipeline("blur_down");
        let blur_up_pipeline = blur_pipeline("blur_up");
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let text_cache = Cache::new(&device);
        let white = bind_image(&device, &image_layout, &sampler, create_texture(&device, 1, 1, false));
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &white.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &[255; 4],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: None },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        Gpu {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            text_cache,
            image_layout,
            uniform_layout,
            sampler,
            quad_pipeline,
            blur_down_pipeline,
            blur_up_pipeline,
            white,
            instance,
            adapter,
            device,
            queue,
        }
    }

    fn create_image(&self, width: u32, height: u32, render_target: bool) -> Image {
        bind_image(&self.device, &self.image_layout, &self.sampler, create_texture(&self.device, width, height, render_target))
    }

    /// Premultiplied RGBA8 pixels.
    pub fn upload(&self, width: u32, height: u32, pixels: &[u8]) -> Image {
        let image = self.create_image(width, height, false);
        self.write_image(&image, pixels);
        image
    }

    /// Hands pending texture writes to the GPU and frees resources whose last use finished.
    /// While the overlay is hidden nothing renders, so without this every write_texture
    /// staging copy and every dropped texture stays allocated (it grew to 12 GB in 3 hours).
    pub fn flush(&self) {
        self.queue.submit(std::iter::empty());
        let _ = self.device.poll(wgpu::PollType::Poll);
    }

    pub fn write_image(&self, image: &Image, pixels: &[u8]) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &image.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            pixels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(image.width * 4), rows_per_image: None },
            wgpu::Extent3d { width: image.width, height: image.height, depth_or_array_layers: 1 },
        );
    }

    fn uniform(&self, values: [f32; 2]) -> wgpu::BindGroup {
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        self.queue.write_buffer(&buffer, 0, bytemuck::bytes_of(&Uniforms { viewport: values, padding: [0.0; 2] }));
        self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &self.uniform_layout, entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }] })
    }

    /// Dual Kawase blur. Returns an image at half the source resolution.
    pub fn blur(&self, source: &Image) -> Image {
        let mut levels = vec![source.clone()];
        for level in 1..=BLUR_LEVELS {
            levels.push(self.create_image((source.width >> level).max(1), (source.height >> level).max(1), true));
        }
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let pass = |encoder: &mut wgpu::CommandEncoder, pipeline: &wgpu::RenderPipeline, from: &Image, to: &Image| {
            let view = to.texture.create_view(&Default::default());
            let uniforms = self.uniform([0.5 / from.width as f32, 0.5 / from.height as f32]);
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blur"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, &uniforms, &[]);
            render_pass.set_bind_group(1, &from.bind_group, &[]);
            render_pass.draw(0..3, 0..1);
        };
        for level in 1..=BLUR_LEVELS {
            pass(&mut encoder, &self.blur_down_pipeline, &levels[level - 1], &levels[level]);
        }
        for level in (1..BLUR_LEVELS).rev() {
            pass(&mut encoder, &self.blur_up_pipeline, &levels[level + 1], &levels[level]);
        }
        self.queue.submit(Some(encoder.finish()));
        levels.swap_remove(1)
    }

    /// Shape a single- or multi-line string. `width` wraps; None means no wrapping.
    pub fn text(&mut self, text: &str, size: f32, width: Option<f32>, weight: Weight, monospace: bool) -> Buffer {
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(size, (size * 1.3).round()));
        buffer.set_size(width, None);
        let family = if monospace { Family::Monospace } else { Family::SansSerif };
        buffer.set_text(text, &Attrs::new().family(family).weight(weight), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    pub fn create_target(&self, display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) -> Target {
        let surface = unsafe { self.instance.create_surface_unsafe(raw_handles(display, surface)).expect("create surface") };
        let capabilities = surface.get_capabilities(&self.adapter);
        let alpha_mode = if capabilities.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            log::warn!("no premultiplied alpha on this surface: {:?}", capabilities.alpha_modes);
            capabilities.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: SURFACE_FORMAT,
            width: 1,
            height: 1,
            present_mode: if capabilities.present_modes.contains(&wgpu::PresentMode::Mailbox) { wgpu::PresentMode::Mailbox } else { wgpu::PresentMode::Fifo },
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let mut atlas = TextAtlas::with_color_mode(&self.device, &self.queue, &self.text_cache, SURFACE_FORMAT, ColorMode::Web);
        let text_renderer = TextRenderer::new(&mut atlas, &self.device, wgpu::MultisampleState::default(), None);
        let viewport = Viewport::new(&self.device, &self.text_cache);
        let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let uniform_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &self.uniform_layout, entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform_buffer.as_entire_binding() }] });
        let instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 64 * 1024, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Target { surface, config, atlas, text_renderer, viewport, uniform_buffer, uniform_bind_group, instance_buffer }
    }

    pub fn resize(&self, target: &mut Target, width: u32, height: u32) {
        if (target.config.width, target.config.height) != (width, height) {
            target.config.width = width.max(1);
            target.config.height = height.max(1);
            target.surface.configure(&self.device, &target.config);
        }
    }

    /// Returns false if no frame could be presented.
    pub fn render(&mut self, target: &mut Target, list: &DrawList, before_present: impl FnOnce()) -> bool {
        let frame = match target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                target.surface.configure(&self.device, &target.config);
                return false;
            }
            other => {
                log::warn!("get_current_texture: {:?}", std::mem::discriminant(&other));
                return false;
            }
        };
        let (width, height) = (target.config.width, target.config.height);
        self.queue.write_buffer(&target.uniform_buffer, 0, bytemuck::bytes_of(&Uniforms { viewport: [width as f32, height as f32], padding: [0.0; 2] }));
        let bytes: &[u8] = bytemuck::cast_slice(&list.quads);
        if bytes.len() as u64 > target.instance_buffer.size() {
            target.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (bytes.len() as u64).next_power_of_two(), usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        }
        self.queue.write_buffer(&target.instance_buffer, 0, bytes);

        target.viewport.update(&self.queue, Resolution { width, height });
        let areas = list.texts.iter().map(|(buffer, x, y, clip, color)| TextArea {
            buffer,
            left: *x,
            top: *y,
            scale: 1.0,
            bounds: TextBounds { left: clip[0] as i32, top: clip[1] as i32, right: (clip[0] + clip[2]) as i32, bottom: (clip[1] + clip[3]) as i32 },
            default_color: Color::rgba(color[0], color[1], color[2], color[3]),
            custom_glyphs: &[],
        });
        if let Err(error) = target.text_renderer.prepare(&self.device, &self.queue, &mut self.font_system, &mut target.atlas, &target.viewport, areas, &mut self.swash_cache) {
            log::warn!("text prepare: {error:?}");
        }

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.quad_pipeline);
            pass.set_bind_group(0, &target.uniform_bind_group, &[]);
            pass.set_vertex_buffer(0, target.instance_buffer.slice(..));
            for (bind_group, range) in &list.batches {
                pass.set_bind_group(1, bind_group.as_ref().unwrap_or(&self.white.bind_group), &[]);
                pass.draw(0..6, range.clone());
            }
            if let Err(error) = target.text_renderer.render(&target.atlas, &target.viewport, &mut pass) {
                log::warn!("text render: {error:?}");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        before_present();
        self.queue.present(frame);
        target.atlas.trim();
        true
    }
}
