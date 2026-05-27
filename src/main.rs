use std::time::{Duration, Instant};

use winit::dpi::LogicalSize;
use winit::event::{ElementState, Event, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::{Fullscreen, WindowBuilder};

#[cfg(all(
    feature = "renderer-gl",
    any(feature = "renderer-softbuffer", feature = "renderer-pixels")
))]
compile_error!("Select only one renderer feature at a time.");
#[cfg(all(feature = "renderer-softbuffer", feature = "renderer-pixels"))]
compile_error!("Select only one renderer feature at a time.");
#[cfg(not(any(
    feature = "renderer-gl",
    feature = "renderer-softbuffer",
    feature = "renderer-pixels"
)))]
compile_error!("Enable one of: renderer-gl, renderer-softbuffer, renderer-pixels.");

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const INITIAL_WIDTH: f64 = 600.0;
const INITIAL_HEIGHT: f64 = 400.0;

mod rng {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = Cell::new(initial_seed());
    }

    fn initial_seed() -> u64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
    }

    fn next() -> u64 {
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            x
        })
    }

    pub fn unit() -> f64 {
        ((next() >> 11) as f64) / ((1u64 << 53) as f64)
    }

    pub fn range(n: u64) -> u64 {
        if n == 0 { 0 } else { next() % n }
    }

    pub fn sign() -> f64 {
        if next() & 1 == 0 { 1.0 } else { -1.0 }
    }
}

mod goop {
    use std::f64::consts::TAU;

    use super::rng;

    pub struct Blob {
        pub x: f64,
        pub y: f64,
        dx: f64,
        dy: f64,
        torque: f64,
        th: f64,
        elasticity: f64,
        max_velocity: f64,
        min_r: f64,
        max_r: f64,
        r: Vec<f64>,
    }

    impl Blob {
        pub fn new(maxx: f64, maxy: f64, size: f64) -> Self {
            let max_r = size / 2.0;
            let min_r = (size / 10.0).max(5.0);
            let mid = (min_r + max_r) / 2.0;

            let elasticity = 1.8;
            let max_velocity = 1.2;
            let torque = 0.0075;
            let npoints = 5 + rng::range(5) as usize;

            let r = (0..npoints)
                .map(|_| (rng::unit() * mid + mid / 2.0) * rng::sign())
                .collect();

            Self {
                x: rng::unit() * maxx,
                y: rng::unit() * maxy,
                dx: rng::unit() * max_velocity * rng::sign(),
                dy: rng::unit() * max_velocity * rng::sign(),
                torque,
                th: TAU * rng::unit() * rng::sign(),
                elasticity,
                max_velocity,
                min_r,
                max_r,
                r,
            }
        }

        pub fn throb(&mut self) {
            for slot in self.r.iter_mut() {
                let r_old = *slot;
                let dir_strict = if r_old > 0.0 { 1.0 } else { -1.0 };
                let dir_nonneg = if r_old >= 0.0 { 1.0 } else { -1.0 };

                let mut ra = r_old.abs();
                ra += rng::unit() * self.elasticity * dir_strict;
                let mut r = ra * dir_nonneg;

                if (ra > self.max_r && r >= 0.0) || (ra < self.min_r && r < 0.0) {
                    r = -r;
                } else if rng::range(50) == 0 {
                    r = -r;
                }

                *slot = r;
            }
        }

        pub fn step(&mut self, maxx: f64, maxy: f64) {
            self.x += self.dx;
            self.y += self.dy;

            if (self.x > maxx && self.dx >= 0.0) || (self.x < 0.0 && self.dx < 0.0) {
                self.dx = -self.dx;
            }
            if (self.y > maxy && self.dy >= 0.0) || (self.y < 0.0 && self.dy < 0.0) {
                self.dy = -self.dy;
            }

            if rng::range(10) == 0 {
                self.dx += rng::unit() * (self.max_velocity / 2.0) * rng::sign();
                self.dy += rng::unit() * (self.max_velocity / 2.0) * rng::sign();
                if self.dx.abs() > self.max_velocity {
                    self.dx /= 2.0;
                }
                if self.dy.abs() > self.max_velocity {
                    self.dy /= 2.0;
                }
            }

            let d = self.torque * rng::unit();
            let mut th = if self.th < 0.0 { -(self.th + d) } else { self.th + d };
            if th > TAU {
                th -= TAU;
            } else if th < 0.0 {
                th += TAU;
            }
            self.th = if self.th > 0.0 { th } else { -th };

            if rng::range(100) == 0 {
                self.th = -self.th;
            }
        }

        pub fn control_points(&self) -> Vec<(f64, f64)> {
            let n = self.r.len();
            let frac = TAU / n as f64;
            let th = self.th.abs();
            (0..n)
                .map(|i| {
                    let ra = self.r[i].abs();
                    let a = i as f64 * frac + th;
                    (self.x + ra * a.cos(), self.y + ra * a.sin())
                })
                .collect()
        }
    }

    pub struct Layer {
        pub blobs: Vec<Blob>,
        pub color: [u8; 3],
    }

    pub struct Scene {
        pub layers: Vec<Layer>,
        pub width: f64,
        pub height: f64,
    }

    impl Scene {
        pub fn new(width: f64, height: f64) -> Self {
            // xscreensaver default: random nlayers in [2, depth-2]; with a 24-bit
            // visual the practical range that still distributes the 12 blobs
            // sensibly is 2..=6.
            let nlayers = 2 + rng::range(5) as usize;

            // Integer-truncated sizes matching the C math.
            let blob_max = (width.min(height) * 0.5).floor();
            let blob_min = (blob_max * 2.0 / 3.0).floor();
            let span = (blob_max - blob_min + 1.0).max(1.0);

            // Distribute DEF_COUNT=12 blobs round-robin across layers, exactly
            // as the C does when count <= 0.
            let mut blobs_per_layer = vec![0usize; nlayers];
            let mut remaining = 12usize;
            while remaining > 0 {
                for slot in blobs_per_layer.iter_mut() {
                    if remaining == 0 {
                        break;
                    }
                    *slot += 1;
                    remaining -= 1;
                }
            }

            let layers = (0..nlayers)
                .map(|i| {
                    let n = blobs_per_layer[i];
                    let blobs = (0..n)
                        .map(|_| {
                            let size = (rng::range(span as u64) as f64) + blob_min;
                            Blob::new(width, height, size)
                        })
                        .collect();
                    Layer {
                        blobs,
                        color: random_layer_color(),
                    }
                })
                .collect();

            Self {
                layers,
                width,
                height,
            }
        }

        pub fn advance(&mut self) {
            for layer in &mut self.layers {
                for blob in &mut layer.blobs {
                    blob.throb();
                    blob.step(self.width, self.height);
                }
            }
        }
    }

    fn random_layer_color() -> [u8; 3] {
        // Match the xlockmore palette: smooth hue ramp at full saturation and
        // value, picked at random per layer.
        hsl_to_rgb(rng::unit(), 1.0, 0.5)
    }

    fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let h_prime = (h * 6.0).rem_euclid(6.0);
        let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
        let (r1, g1, b1) = match h_prime as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = l - c / 2.0;
        [
            ((r1 + m) * 255.0).clamp(0.0, 255.0) as u8,
            ((g1 + m) * 255.0).clamp(0.0, 255.0) as u8,
            ((b1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ]
    }

    pub struct BezierSection {
        pub p0: (f64, f64),
        pub p1: (f64, f64),
        pub p2: (f64, f64),
        pub p3: (f64, f64),
    }

    pub fn closed_spline_sections(c: &[(f64, f64)]) -> Vec<BezierSection> {
        let n = c.len();
        if n < 3 {
            return Vec::new();
        }
        let n_i = n as isize;
        let cp = |i: isize| {
            let idx = ((i % n_i) + n_i) as usize % n;
            c[idx]
        };
        (0..n)
            .map(|i| {
                let cm1 = cp(i as isize - 1);
                let c0 = cp(i as isize);
                let c1 = cp(i as isize + 1);
                let c2 = cp(i as isize + 2);
                let p1 = third(c0, c1);
                let p2 = third(c1, c0);
                let t1 = third(c0, cm1);
                let p0 = mid(t1, p1);
                let t2 = third(c1, c2);
                let p3 = mid(t2, p2);
                BezierSection { p0, p1, p2, p3 }
            })
            .collect()
    }

    fn third(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        ((2.0 * a.0 + b.0) / 3.0, (2.0 * a.1 + b.1) / 3.0)
    }

    fn mid(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
    }
}

struct AppState {
    scene: goop::Scene,
}

fn is_key_press(event: &winit::event::KeyEvent) -> bool {
    event.state == ElementState::Pressed
}

fn main() {
    #[cfg(feature = "renderer-gl")]
    {
        gl_app::run();
    }

    #[cfg(feature = "renderer-softbuffer")]
    {
        softbuffer_app::run();
    }

    #[cfg(feature = "renderer-pixels")]
    {
        pixels_app::run();
    }
}

#[cfg(feature = "renderer-gl")]
mod gl_app {
    use super::*;
    use std::num::NonZeroU32;

    use femtovg::renderer::OpenGl;
    use femtovg::{Canvas, Color, CompositeOperation, Paint, Path};
    use glutin::config::ConfigTemplateBuilder;
    use glutin::context::{ContextApi, ContextAttributesBuilder, Version};
    use glutin::display::GetGlDisplay;
    use glutin::prelude::*;
    use glutin::surface::{SurfaceAttributesBuilder, WindowSurface};
    use glutin_winit::DisplayBuilder;
    use raw_window_handle::HasRawWindowHandle;
    use winit::dpi::PhysicalSize;

    pub fn run() {
        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let window_builder = WindowBuilder::new()
            .with_title("Goop")
            .with_inner_size(LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
            .with_fullscreen(Some(Fullscreen::Borderless(None)));

        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_depth_size(24)
            .with_stencil_size(8);

        let display_builder = DisplayBuilder::new().with_window_builder(Some(window_builder));

        let (window, gl_config) = display_builder
            .build(&event_loop, template, |configs| {
                configs
                    .reduce(|accum, config| {
                        if config.num_samples() > accum.num_samples() {
                            config
                        } else {
                            accum
                        }
                    })
                    .unwrap()
            })
            .expect("Failed to create display");

        let window = window.expect("Window wasn't created");
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        let raw_window_handle = window.raw_window_handle();

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .build(Some(raw_window_handle));
        let fallback_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
            .build(Some(raw_window_handle));

        let not_current_gl_context = unsafe {
            gl_config
                .display()
                .create_context(&gl_config, &context_attributes)
                .or_else(|_| {
                    gl_config
                        .display()
                        .create_context(&gl_config, &fallback_attributes)
                })
                .expect("Failed to create GL context")
        };

        let initial_size = window.inner_size();
        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            NonZeroU32::new(initial_size.width.max(1)).unwrap(),
            NonZeroU32::new(initial_size.height.max(1)).unwrap(),
        );

        let gl_surface = unsafe {
            gl_config
                .display()
                .create_window_surface(&gl_config, &surface_attributes)
                .expect("Failed to create GL surface")
        };

        let gl_context = not_current_gl_context
            .make_current(&gl_surface)
            .expect("Failed to make GL context current");

        let renderer = unsafe {
            OpenGl::new_from_function_cstr(|symbol| gl_config.display().get_proc_address(symbol))
                .expect("Failed to create renderer")
        };
        let mut canvas = Canvas::new(renderer).expect("Failed to create canvas");

        resize_canvas(&mut canvas, initial_size);
        window.set_cursor_visible(window.fullscreen().is_none());

        let mut state = AppState {
            scene: goop::Scene::new(initial_size.width as f64, initial_size.height as f64),
        };
        let mut next_tick = Instant::now();

        event_loop
            .run(move |event, elwt| {
                elwt.set_control_flow(ControlFlow::WaitUntil(next_tick));

                match event {
                    Event::WindowEvent { event, .. } => match event {
                        WindowEvent::CloseRequested => elwt.exit(),
                        WindowEvent::Resized(size) => {
                            resize_surface(&gl_surface, &gl_context, size);
                            resize_canvas(&mut canvas, size);
                            state.scene = goop::Scene::new(size.width as f64, size.height as f64);
                            window.set_cursor_visible(window.fullscreen().is_none());
                            window.request_redraw();
                        }
                        WindowEvent::ScaleFactorChanged {
                            mut inner_size_writer,
                            ..
                        } => {
                            let size = window.inner_size();
                            let _ = inner_size_writer.request_inner_size(size);
                            resize_surface(&gl_surface, &gl_context, size);
                            resize_canvas(&mut canvas, size);
                            state.scene = goop::Scene::new(size.width as f64, size.height as f64);
                            window.set_cursor_visible(window.fullscreen().is_none());
                            window.request_redraw();
                        }
                        WindowEvent::KeyboardInput { event, .. } => {
                            if is_key_press(&event) {
                                elwt.exit();
                            }
                        }
                        WindowEvent::MouseInput {
                            state: ElementState::Pressed,
                            ..
                        } => elwt.exit(),
                        WindowEvent::RedrawRequested => {
                            state.scene.advance();
                            let size = window.inner_size();
                            draw_scene(&mut canvas, &state.scene, size);
                            canvas.flush();
                            gl_surface.swap_buffers(&gl_context).ok();
                            next_tick = Instant::now() + FRAME_INTERVAL;
                        }
                        _ => {}
                    },
                    Event::NewEvents(StartCause::ResumeTimeReached { .. })
                    | Event::NewEvents(StartCause::Init) => {
                        window.request_redraw();
                    }
                    _ => {}
                }
            })
            .expect("Event loop error");
    }

    fn resize_surface(
        gl_surface: &glutin::surface::Surface<WindowSurface>,
        gl_context: &glutin::context::PossiblyCurrentContext,
        size: PhysicalSize<u32>,
    ) {
        let width = NonZeroU32::new(size.width.max(1)).unwrap();
        let height = NonZeroU32::new(size.height.max(1)).unwrap();
        gl_surface.resize(gl_context, width, height);
    }

    fn resize_canvas(canvas: &mut Canvas<OpenGl>, size: PhysicalSize<u32>) {
        canvas.set_size(size.width, size.height, 1.0);
    }

    fn draw_scene(canvas: &mut Canvas<OpenGl>, scene: &goop::Scene, size: PhysicalSize<u32>) {
        canvas.global_composite_operation(CompositeOperation::SourceOver);
        canvas.clear_rect(0, 0, size.width, size.height, Color::rgb(0, 0, 0));
        canvas.global_composite_operation(CompositeOperation::Lighter);

        for layer in &scene.layers {
            let paint = Paint::color(Color::rgb(layer.color[0], layer.color[1], layer.color[2]));
            for blob in &layer.blobs {
                let pts = blob.control_points();
                let sections = goop::closed_spline_sections(&pts);
                if sections.is_empty() {
                    continue;
                }
                let mut path = Path::new();
                path.move_to(sections[0].p0.0 as f32, sections[0].p0.1 as f32);
                for s in &sections {
                    path.bezier_to(
                        s.p1.0 as f32,
                        s.p1.1 as f32,
                        s.p2.0 as f32,
                        s.p2.1 as f32,
                        s.p3.0 as f32,
                        s.p3.1 as f32,
                    );
                }
                path.close();
                canvas.fill_path(&path, &paint);
            }
        }

        canvas.global_composite_operation(CompositeOperation::SourceOver);
    }
}

#[cfg(any(feature = "renderer-softbuffer", feature = "renderer-pixels"))]
mod cpu_renderer {
    use super::*;

    use tiny_skia::{BlendMode, Color, FillRule, Paint, PathBuilder, PixmapMut, Transform};

    pub fn draw_scene(pixmap: &mut PixmapMut<'_>, scene: &goop::Scene) {
        pixmap.fill(Color::BLACK);

        for layer in &scene.layers {
            let mut paint = Paint::default();
            paint.set_color_rgba8(layer.color[0], layer.color[1], layer.color[2], 255);
            paint.blend_mode = BlendMode::Plus;
            paint.anti_alias = true;

            for blob in &layer.blobs {
                let pts = blob.control_points();
                let sections = goop::closed_spline_sections(&pts);
                if sections.is_empty() {
                    continue;
                }
                let mut pb = PathBuilder::new();
                pb.move_to(sections[0].p0.0 as f32, sections[0].p0.1 as f32);
                for s in &sections {
                    pb.cubic_to(
                        s.p1.0 as f32,
                        s.p1.1 as f32,
                        s.p2.0 as f32,
                        s.p2.1 as f32,
                        s.p3.0 as f32,
                        s.p3.1 as f32,
                    );
                }
                pb.close();
                if let Some(path) = pb.finish() {
                    pixmap.fill_path(
                        &path,
                        &paint,
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }
    }
}

#[cfg(feature = "renderer-softbuffer")]
mod softbuffer_app {
    use super::*;

    use softbuffer::{Context, Surface};
    use std::num::NonZeroU32;
    use std::rc::Rc;
    use tiny_skia::Pixmap;

    use crate::cpu_renderer::draw_scene;

    pub fn run() {
        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let window = WindowBuilder::new()
            .with_title("Goop")
            .with_inner_size(LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
            .build(&event_loop)
            .expect("Failed to create window");
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        window.set_cursor_visible(window.fullscreen().is_none());
        let window = Rc::new(window);

        let mut state = AppState {
            scene: goop::Scene::new(
                window.inner_size().width as f64,
                window.inner_size().height as f64,
            ),
        };

        let context = Context::new(window.clone()).expect("Failed to create softbuffer context");
        let mut surface = Surface::new(&context, window.clone()).expect("Failed to create surface");

        let mut size = window.inner_size();
        surface
            .resize(
                NonZeroU32::new(size.width.max(1)).unwrap(),
                NonZeroU32::new(size.height.max(1)).unwrap(),
            )
            .expect("Failed to resize surface");
        let mut pixmap =
            Pixmap::new(size.width.max(1), size.height.max(1)).expect("Failed to create pixmap");

        let mut next_tick = Instant::now();

        event_loop
            .run(move |event, elwt| {
                elwt.set_control_flow(ControlFlow::WaitUntil(next_tick));

                match event {
                    Event::WindowEvent { event, .. } => match event {
                        WindowEvent::CloseRequested => elwt.exit(),
                        WindowEvent::Resized(new_size) => {
                            size = new_size;
                            surface
                                .resize(
                                    NonZeroU32::new(size.width.max(1)).unwrap(),
                                    NonZeroU32::new(size.height.max(1)).unwrap(),
                                )
                                .expect("Failed to resize surface");
                            pixmap = Pixmap::new(size.width.max(1), size.height.max(1))
                                .expect("Failed to create pixmap");
                            state.scene = goop::Scene::new(size.width as f64, size.height as f64);
                            window.set_cursor_visible(window.fullscreen().is_none());
                            window.request_redraw();
                        }
                        WindowEvent::ScaleFactorChanged {
                            mut inner_size_writer,
                            ..
                        } => {
                            let new_size = window.inner_size();
                            let _ = inner_size_writer.request_inner_size(new_size);
                            size = new_size;
                            surface
                                .resize(
                                    NonZeroU32::new(size.width.max(1)).unwrap(),
                                    NonZeroU32::new(size.height.max(1)).unwrap(),
                                )
                                .expect("Failed to resize surface");
                            pixmap = Pixmap::new(size.width.max(1), size.height.max(1))
                                .expect("Failed to create pixmap");
                            state.scene = goop::Scene::new(size.width as f64, size.height as f64);
                            window.set_cursor_visible(window.fullscreen().is_none());
                            window.request_redraw();
                        }
                        WindowEvent::KeyboardInput { event, .. } => {
                            if is_key_press(&event) {
                                elwt.exit();
                            }
                        }
                        WindowEvent::MouseInput {
                            state: ElementState::Pressed,
                            ..
                        } => elwt.exit(),
                        WindowEvent::RedrawRequested => {
                            state.scene.advance();

                            let mut canvas = pixmap.as_mut();
                            draw_scene(&mut canvas, &state.scene);

                            let mut buffer = surface
                                .buffer_mut()
                                .expect("Failed to get softbuffer buffer");
                            blit_pixmap_to_buffer(&pixmap, &mut buffer);
                            buffer.present().expect("Failed to present buffer");

                            next_tick = Instant::now() + FRAME_INTERVAL;
                        }
                        _ => {}
                    },
                    Event::NewEvents(StartCause::ResumeTimeReached { .. })
                    | Event::NewEvents(StartCause::Init) => {
                        window.request_redraw();
                    }
                    _ => {}
                }
            })
            .expect("Event loop error");
    }

    fn blit_pixmap_to_buffer(pixmap: &Pixmap, buffer: &mut [u32]) {
        let data = pixmap.data();
        for (i, chunk) in data.chunks_exact(4).enumerate() {
            let r = chunk[0] as u32;
            let g = chunk[1] as u32;
            let b = chunk[2] as u32;
            buffer[i] = (r << 16) | (g << 8) | b;
        }
    }
}

#[cfg(feature = "renderer-pixels")]
mod pixels_app {
    use super::*;

    use pixels::{Pixels, SurfaceTexture};
    use tiny_skia::PixmapMut;

    use crate::cpu_renderer::draw_scene;

    pub fn run() {
        let event_loop = EventLoop::new().expect("Failed to create event loop");
        let window = WindowBuilder::new()
            .with_title("Goop")
            .with_inner_size(LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
            .build(&event_loop)
            .expect("Failed to create window");
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        window.set_cursor_visible(window.fullscreen().is_none());

        let mut state = AppState {
            scene: goop::Scene::new(
                window.inner_size().width as f64,
                window.inner_size().height as f64,
            ),
        };

        let mut size = window.inner_size();
        let surface_texture = SurfaceTexture::new(size.width.max(1), size.height.max(1), &window);
        let mut pixels = Pixels::new(size.width.max(1), size.height.max(1), surface_texture)
            .expect("Failed to create Pixels surface");

        let mut next_tick = Instant::now();

        event_loop
            .run(move |event, elwt| {
                elwt.set_control_flow(ControlFlow::WaitUntil(next_tick));

                match event {
                    Event::WindowEvent { event, .. } => match event {
                        WindowEvent::CloseRequested => elwt.exit(),
                        WindowEvent::Resized(new_size) => {
                            size = new_size;
                            let _ = pixels.resize_surface(size.width.max(1), size.height.max(1));
                            let _ = pixels.resize_buffer(size.width.max(1), size.height.max(1));
                            state.scene = goop::Scene::new(size.width as f64, size.height as f64);
                            window.set_cursor_visible(window.fullscreen().is_none());
                            window.request_redraw();
                        }
                        WindowEvent::ScaleFactorChanged {
                            mut inner_size_writer,
                            ..
                        } => {
                            let new_size = window.inner_size();
                            let _ = inner_size_writer.request_inner_size(new_size);
                            size = new_size;
                            let _ = pixels.resize_surface(size.width.max(1), size.height.max(1));
                            let _ = pixels.resize_buffer(size.width.max(1), size.height.max(1));
                            state.scene = goop::Scene::new(size.width as f64, size.height as f64);
                            window.set_cursor_visible(window.fullscreen().is_none());
                            window.request_redraw();
                        }
                        WindowEvent::KeyboardInput { event, .. } => {
                            if is_key_press(&event) {
                                elwt.exit();
                            }
                        }
                        WindowEvent::MouseInput {
                            state: ElementState::Pressed,
                            ..
                        } => elwt.exit(),
                        WindowEvent::RedrawRequested => {
                            state.scene.advance();

                            let frame = pixels.frame_mut();
                            if let Some(mut pixmap) =
                                PixmapMut::from_bytes(frame, size.width.max(1), size.height.max(1))
                            {
                                draw_scene(&mut pixmap, &state.scene);
                            }
                            pixels.render().expect("Pixels render failed");
                            next_tick = Instant::now() + FRAME_INTERVAL;
                        }
                        _ => {}
                    },
                    Event::NewEvents(StartCause::ResumeTimeReached { .. })
                    | Event::NewEvents(StartCause::Init) => {
                        window.request_redraw();
                    }
                    _ => {}
                }
            })
            .expect("Event loop error");
    }
}
