//! `cargo run -p uni-render --example hello`
//!
//! Opens a winit window and renders the "first pixels" milestone scene:
//!   * a near-black background (`0x0a0a0aff`),
//!   * a white rounded rect (`0xffffffff`, radius ~16),
//!   * the text "Uni-UI" inside it.
//!
//! It builds the [`Scene`] purely from the renderer-agnostic [`DrawCmd`] API —
//! exactly what `uni-core` will emit when it lowers a `uni-ir` Document.

use std::sync::Arc;

use uni_render::{DrawCmd, Renderer, Scene, WgpuRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Build the demo scene in logical pixels for the given logical window size.
fn build_scene(logical_w: f32, logical_h: f32) -> Scene {
    // Background fills the whole viewport (the backend uses a full-cover rect
    // as the clear color).
    let mut scene: Scene = vec![DrawCmd::FilledRect {
        x: 0.0,
        y: 0.0,
        w: logical_w,
        h: logical_h,
        color: 0x0a0a0aff,
        corner_radius: 0.0,
        rotation: 0.0,
    }];

    // A centered white rounded card.
    let card_w = 320.0_f32.min(logical_w - 40.0).max(80.0);
    let card_h = 140.0_f32.min(logical_h - 40.0).max(60.0);
    let card_x = (logical_w - card_w) / 2.0;
    let card_y = (logical_h - card_h) / 2.0;

    scene.push(DrawCmd::FilledRect {
        x: card_x,
        y: card_y,
        w: card_w,
        h: card_h,
        color: 0xffffffff,
        corner_radius: 16.0,
        rotation: 0.0,
    });

    // Label inside the card (dark text on the white card).
    scene.push(DrawCmd::Text {
        x: card_x + 28.0,
        y: card_y + card_h / 2.0 - 24.0,
        content: "Uni-UI".to_string(),
        size: 48.0,
        color: 0x0a0a0aff,
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
            .with_title("Uni-UI — first pixels")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
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

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(window), Some(renderer)) = (self.window.as_ref(), self.renderer.as_mut()) else {
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
