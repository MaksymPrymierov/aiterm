//! GPU renderer: wgpu quads (backgrounds, cursor, decorations) + glyphon text.
//!
//! Text is laid out per row in *segments*: runs of ASCII in one style are shaped
//! together (the primary monospace font keeps them on the grid), while every
//! non-ASCII glyph (box drawing, emoji, CJK, fallback fonts) gets its own
//! segment pinned to its cell, so fallback advances never shift the grid.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use aiterm_vt::{CellFlags as Flags, CellView, CursorShape, Frame, GridSize, Rgb, Side};
use glyphon::{
    Attrs, Buffer, Cache, Color as GColor, Family, FontSystem, Metrics, Resolution, Shaping, Style, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

const PADDING: f32 = 8.0;
const LINE_HEIGHT: f32 = 1.25;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub font_px: f32,
    pub w: f32,
    pub h: f32,
}

struct Segment {
    x: f32,
    fg: Rgb,
    buffer: Buffer,
}

#[derive(Default)]
struct RowCache {
    key: u64,
    segments: Vec<Segment>,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    instance: wgpu::Instance,

    quad_pipeline: wgpu::RenderPipeline,
    quad_bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_cap: usize,
    quads: Vec<QuadInstance>,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    rows: Vec<RowCache>,

    font_family: String,
    font_size: f32,
    scale: f32,
    pub metrics: CellMetrics,

    // Dropped last: the surface borrows the window.
    window: Arc<Window>,
}

fn default_font_family() -> String {
    if let Ok(f) = std::env::var("AITERM_FONT") {
        return f;
    }
    let out = std::process::Command::new("fc-match")
        // Prefer a monospace font that also has Nerd Font icons (U+F115); fontconfig
        // falls back to the plain default monospace when none is installed.
        .args(["-f", "%{family[0]}", "monospace:charset=f115"])
        .output();
    match out {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => "monospace".into(),
    }
}

impl Renderer {
    pub async fn new(window: Arc<Window>, event_loop: &ActiveEventLoop) -> Self {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
            event_loop.owned_display_handle(),
        )));
        let surface = instance.create_surface(window.clone()).expect("create surface");
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .expect("no GPU adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Frame pacing comes from Wayland frame callbacks (`pre_present_notify`),
        // so prefer Mailbox: presenting never blocks the event loop. Fifo is the
        // always-available fallback.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        log::info!("surface: {format:?}, {present_mode:?}");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        // Quad pipeline.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(include_str!("quad.wgsl").into()),
        });
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad bgl"),
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
        let quad_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<QuadInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let instance_cap = 4096;
        let instance_buf = Self::make_instance_buf(&device, instance_cap);

        // Text.
        let font_system = FontSystem::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        let font_size = std::env::var("AITERM_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(13.0);
        let mut this = Self {
            device,
            queue,
            surface,
            config,
            instance,
            quad_pipeline,
            quad_bind_group,
            uniform_buf,
            instance_buf,
            instance_cap,
            quads: Vec::new(),
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
            rows: Vec::new(),
            font_family: default_font_family(),
            font_size,
            scale,
            metrics: CellMetrics {
                font_px: 1.0,
                w: 1.0,
                h: 1.0,
            },
            window,
        };
        log::info!("font: {:?} {}pt", this.font_family, this.font_size);
        this.update_metrics();
        this
    }

    fn make_instance_buf(device: &wgpu::Device, cap: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad instances"),
            size: (cap * std::mem::size_of::<QuadInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn update_metrics(&mut self) {
        let font_px = (self.font_size * self.scale).round().max(4.0);
        let h = (font_px * LINE_HEIGHT).ceil();
        let mut buf = Buffer::new(&mut self.font_system, Metrics::new(font_px, h));
        let attrs = Attrs::new().family(Family::Name(&self.font_family));
        buf.set_text("M", &attrs, Shaping::Advanced, None);
        buf.shape_until_scroll(&mut self.font_system, false);
        let w = buf
            .layout_runs()
            .next()
            .and_then(|r| r.glyphs.first().map(|g| g.w))
            .unwrap_or(font_px * 0.6);
        self.metrics = CellMetrics { font_px, w, h };
        self.rows.clear();
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_scale(&mut self, scale: f64) {
        self.scale = scale as f32;
        self.update_metrics();
    }

    /// Change font size by `delta` points; `None` resets to the default.
    pub fn adjust_font_size(&mut self, delta: Option<f32>) {
        self.font_size = match delta {
            Some(d) => (self.font_size + d).clamp(6.0, 72.0),
            None => 13.0,
        };
        self.update_metrics();
    }

    fn padding(&self) -> f32 {
        (PADDING * self.scale).round()
    }

    pub fn grid_size(&self) -> GridSize {
        let pad = self.padding();
        let cols = ((self.config.width as f32 - 2.0 * pad) / self.metrics.w)
            .floor()
            .max(2.0) as usize;
        let rows = ((self.config.height as f32 - 2.0 * pad) / self.metrics.h)
            .floor()
            .max(1.0) as usize;
        GridSize {
            cols,
            rows,
            cell_w: self.metrics.w,
            cell_h: self.metrics.h,
        }
    }

    /// Pixel position -> (row, col, side of the cell).
    pub fn cell_at(&self, x: f64, y: f64) -> (usize, usize, Side) {
        let pad = self.padding() as f64;
        let fx = ((x - pad) / self.metrics.w as f64).max(0.0);
        let fy = ((y - pad) / self.metrics.h as f64).max(0.0);
        let side = if fx.fract() < 0.5 { Side::Left } else { Side::Right };
        (fy as usize, fx as usize, side)
    }

    fn row_key(cells: &[CellView]) -> u64 {
        let mut h = std::hash::DefaultHasher::new();
        for c in cells {
            c.c.hash(&mut h);
            c.zerowidth.hash(&mut h);
            c.fg.hash(&mut h);
            (c.flags & (Flags::BOLD | Flags::ITALIC | Flags::WIDE_SPACER))
                .bits()
                .hash(&mut h);
        }
        h.finish()
    }

    fn build_row(&mut self, cells: &[CellView]) -> Vec<Segment> {
        let m = self.metrics;
        let family = self.font_family.clone();
        let mut segments = Vec::new();
        // (start col, fg, bold, italic, text)
        let mut run: Option<(usize, Rgb, bool, bool, String)> = None;

        let flush =
            |run: &mut Option<(usize, Rgb, bool, bool, String)>, fs: &mut FontSystem, out: &mut Vec<Segment>| {
                if let Some((col, fg, bold, italic, text)) = run.take() {
                    let text = text.trim_end();
                    if text.is_empty() {
                        return;
                    }
                    let mut attrs = Attrs::new().family(Family::Name(&family));
                    if bold {
                        attrs = attrs.weight(Weight::BOLD);
                    }
                    if italic {
                        attrs = attrs.style(Style::Italic);
                    }
                    let mut buffer = Buffer::new(fs, Metrics::new(m.font_px, m.h));
                    buffer.set_size(None, Some(m.h));
                    buffer.set_text(text, &attrs, Shaping::Advanced, None);
                    buffer.shape_until_scroll(fs, false);
                    out.push(Segment {
                        x: col as f32 * m.w,
                        fg,
                        buffer,
                    });
                }
            };

        for (col, cell) in cells.iter().enumerate() {
            if cell.flags.contains(Flags::WIDE_SPACER) {
                flush(&mut run, &mut self.font_system, &mut segments);
                continue;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            let c = cell.c;

            if c == ' ' || c == '\t' || c == '\0' {
                // Spaces join any run; they never start one.
                if let Some(r) = run.as_mut() {
                    r.4.push(' ');
                }
                continue;
            }

            let simple = c.is_ascii_graphic() && cell.zerowidth.is_none();
            if simple {
                match run.as_mut() {
                    Some(r) if r.1 == cell.fg && r.2 == bold && r.3 == italic => r.4.push(c),
                    _ => {
                        flush(&mut run, &mut self.font_system, &mut segments);
                        run = Some((col, cell.fg, bold, italic, c.to_string()));
                    }
                }
            } else {
                flush(&mut run, &mut self.font_system, &mut segments);
                let mut text = c.to_string();
                if let Some(z) = &cell.zerowidth {
                    text.push_str(z);
                }
                run = Some((col, cell.fg, bold, italic, text));
                flush(&mut run, &mut self.font_system, &mut segments);
            }
        }
        flush(&mut run, &mut self.font_system, &mut segments);
        segments
    }

    fn push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        self.quads.push(QuadInstance {
            pos: [x, y],
            size: [w, h],
            color: color.to_linear(),
        });
    }

    pub fn render(&mut self, frame: &Frame) {
        let m = self.metrics;
        let pad = self.padding();
        let (width, height) = (self.config.width, self.config.height);

        // --- Text layout (cached per row) ---
        self.rows.resize_with(frame.rows, RowCache::default);
        self.rows.truncate(frame.rows);
        for r in 0..frame.rows {
            let cells = frame.row(r);
            let key = Self::row_key(cells);
            if self.rows[r].key != key {
                let segments = self.build_row(cells);
                self.rows[r] = RowCache { key, segments };
            }
        }

        // --- Quads: backgrounds, decorations, cursor ---
        self.quads.clear();
        let thickness = (m.font_px / 14.0).round().max(1.0);
        for r in 0..frame.rows {
            let y = pad + r as f32 * m.h;
            let cells = frame.row(r);
            let mut c = 0;
            while c < frame.cols {
                let bg = cells[c].bg;
                let start = c;
                while c < frame.cols && cells[c].bg == bg {
                    c += 1;
                }
                if bg != frame.background {
                    // Extend edge cells into the padding so colored lines reach the window border.
                    let x0 = if start == 0 { 0.0 } else { pad + start as f32 * m.w };
                    let x1 = if c == frame.cols {
                        width as f32
                    } else {
                        pad + c as f32 * m.w
                    };
                    self.push_quad(x0, y, x1 - x0, m.h, bg);
                }
            }
            for (c, cell) in cells.iter().enumerate() {
                let x = pad + c as f32 * m.w;
                if cell.flags.contains(Flags::UNDERLINE) {
                    self.push_quad(x, y + m.h - thickness * 2.0, m.w, thickness, cell.fg);
                }
                if cell.flags.contains(Flags::STRIKEOUT) {
                    self.push_quad(x, y + m.h / 2.0, m.w, thickness, cell.fg);
                }
            }
        }
        if let Some(cur) = frame.cursor {
            let x = pad + cur.col as f32 * m.w;
            let y = pad + cur.row as f32 * m.h;
            let wide = frame.row(cur.row)[cur.col].flags.contains(Flags::WIDE_CHAR);
            let w = if wide { m.w * 2.0 } else { m.w };
            let t = (thickness * 2.0).max(2.0);
            match cur.shape {
                CursorShape::Block => self.push_quad(x, y, w, m.h, cur.color),
                CursorShape::Beam => self.push_quad(x, y, t, m.h, cur.color),
                CursorShape::Underline => self.push_quad(x, y + m.h - t, w, t, cur.color),
                CursorShape::HollowBlock => {
                    self.push_quad(x, y, w, thickness, cur.color);
                    self.push_quad(x, y + m.h - thickness, w, thickness, cur.color);
                    self.push_quad(x, y, thickness, m.h, cur.color);
                    self.push_quad(x + w - thickness, y, thickness, m.h, cur.color);
                }
            }
        }

        if self.quads.len() > self.instance_cap {
            self.instance_cap = self.quads.len().next_power_of_two();
            self.instance_buf = Self::make_instance_buf(&self.device, self.instance_cap);
        }
        self.queue
            .write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&self.quads));
        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]),
        );

        // --- Prepare text ---
        self.viewport.update(&self.queue, Resolution { width, height });
        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let areas = self.rows.iter().enumerate().flat_map(|(r, row)| {
            row.segments.iter().map(move |s| TextArea {
                buffer: &s.buffer,
                left: pad + s.x,
                top: pad + r as f32 * m.h,
                scale: 1.0,
                bounds,
                default_color: GColor::rgb(s.fg.r, s.fg.g, s.fg.b),
                custom_glyphs: &[],
            })
        });
        // Segment colors are carried in default_color; spans have no own color.
        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        ) {
            log::warn!("text prepare: {e:?}");
        }

        // --- Draw ---
        let mut reconfigure = false;
        let surface_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            // Still presentable: draw this frame, then reconfigure (the texture must be dropped first).
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                reconfigure = true;
                t
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self
                    .instance
                    .create_surface(self.window.clone())
                    .expect("recreate surface");
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => panic!("surface validation error"),
        };
        let view = surface_tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let [r, g, b, _] = frame.background.to_linear();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r as f64,
                            g: g as f64,
                            b: b as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !self.quads.is_empty() {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_bind_group(0, &self.quad_bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buf.slice(..));
                pass.draw(0..6, 0..self.quads.len() as u32);
            }
            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                log::warn!("text render: {e:?}");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        // Ask the compositor for a frame callback: winit then holds back the next
        // `RedrawRequested` until the compositor is ready for a new frame (one
        // render per vblank at most, none while the window is hidden).
        self.window.pre_present_notify();
        self.queue.present(surface_tex);
        if reconfigure {
            self.surface.configure(&self.device, &self.config);
        }
        self.atlas.trim();
    }
}
