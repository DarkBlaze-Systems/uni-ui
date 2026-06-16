//! `cargo run -p uni-render --example frost`
//!
//! Demonstrates the **frosted-glass** primitive ([`DrawCmd::FrostedRect`]).
//!
//! The scene draws a bunch of colorful overlapping rounded rects (the kind of
//! busy, high-contrast content that makes a *real* backdrop blur obvious), then
//! floats a `FrostedRect` panel over the middle of it. Because the blur is a
//! genuine backdrop blur (render-to-texture + separable Gaussian, see
//! `wgpu_backend.rs`), the colors *bleed* softly through the panel — it is not
//! a flat translucent rectangle. A label is drawn on top of the glass.
//!
//! Set `UNI_GPU_POWER=high` to request a high-performance adapter; the chosen
//! adapter (name / backend / driver) is logged to stderr at startup.

use std::sync::Arc;

use uni_render::{DrawCmd, Renderer, Scene, WgpuRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Build the demo scene in logical pixels for the given logical window size.
fn build_scene(logical_w: f32, logical_h: f32) -> Scene {
    let mut scene: Scene = Vec::new();

    // Dark background (full-cover -> used as the clear color).
    scene.push(DrawCmd::FilledRect {
        x: 0.0,
        y: 0.0,
        w: logical_w,
        h: logical_h,
        color: 0x101018ff,
        corner_radius: 0.0,
    });

    // Colorful content behind the glass: a grid of vivid overlapping blobs.
    let palette: [u32; 6] = [
        0xff3b5cff, // red
        0xffb13bff, // orange
        0x3bd1ffff, // cyan
        0x8a3bffff, // violet
        0x3bff87ff, // green
        0xff3bd1ff, // magenta
    ];
    let cols = 4usize;
    let rows = 3usize;
    let cell_w = logical_w / cols as f32;
    let cell_h = logical_h / rows as f32;
    let mut i = 0usize;
    for r in 0..rows {
        for c in 0..cols {
            let blob = (cell_w.min(cell_h)) * 0.9;
            let cx = c as f32 * cell_w + cell_w / 2.0;
            let cy = r as f32 * cell_h + cell_h / 2.0;
            scene.push(DrawCmd::FilledRect {
                x: cx - blob / 2.0,
                y: cy - blob / 2.0,
                w: blob,
                h: blob,
                color: palette[i % palette.len()],
                corner_radius: blob * 0.35,
            });
            i += 1;
        }
    }

    // A bright diagonal stripe to make the blur's softening unmistakable.
    scene.push(DrawCmd::FilledRect {
        x: logical_w * 0.1,
        y: logical_h * 0.45,
        w: logical_w * 0.8,
        h: 24.0,
        color: 0xffffffff,
        corner_radius: 12.0,
    });

    // The frosted-glass panel, centered, floating over the colorful content.
    let panel_w = (logical_w * 0.6).clamp(160.0, logical_w - 40.0);
    let panel_h = (logical_h * 0.4).clamp(120.0, logical_h - 40.0);
    let panel_x = (logical_w - panel_w) / 2.0;
    let panel_y = (logical_h - panel_h) / 2.0;
    scene.push(DrawCmd::FrostedRect {
        x: panel_x,
        y: panel_y,
        w: panel_w,
        h: panel_h,
        corner_radius: 28.0,
        // Translucent light glass: a touch of white at ~22% to read as "frost".
        tint: 0xffffff38,
        blur_radius: 24.0,
    });

    // Label on top of the glass (drawn AFTER the FrostedRect -> over it).
    scene.push(DrawCmd::Text {
        x: panel_x + 32.0,
        y: panel_y + panel_h / 2.0 - 22.0,
        content: "Frosted Glass".to_string(),
        size: 40.0,
        color: 0x0a0a12ff,
    });

    scene
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Uni-UI — frosted glass")
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 640.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        match WgpuRenderer::new(window.clone()) {
            Ok(r) => {
                self.renderer = Some(r);
                self.window = Some(window);
            }
            Err(e) => {
                eprintln!("renderer init failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let (Some(window), Some(renderer)) = (self.window.as_ref(), self.renderer.as_mut())
        else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                renderer.resize(size.width, size.height, window.scale_factor());
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = window.inner_size();
                renderer.resize(size.width, size.height, scale_factor);
            }
            WindowEvent::RedrawRequested => {
                let scale = window.scale_factor() as f32;
                let phys = window.inner_size();
                let logical_w = phys.width as f32 / scale;
                let logical_h = phys.height as f32 / scale;
                let scene = build_scene(logical_w, logical_h);
                match renderer.render(&scene) {
                    Ok(()) => {}
                    Err(uni_render::RenderError::SurfaceLost) => {
                        let s = window.inner_size();
                        renderer.resize(s.width, s.height, window.scale_factor());
                    }
                    Err(e) => eprintln!("render error: {e}"),
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().expect("failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::default();
    event_loop.run_app(&mut app).expect("event loop error");
}
