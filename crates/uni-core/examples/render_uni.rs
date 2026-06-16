//! `cargo run -p uni-core --example render_uni`
//!
//! The full Uni-UI chain, end to end:
//!   `.uni` source → uni-dsl parses → uni-ir Document → uni-core lowers → uni-render draws.
//!
//! This is the first surface that renders from *words you wrote*, not a
//! hardcoded scene. Edit `SRC` and rerun to see it change.

use std::sync::Arc;

use uni_core::lower;
use uni_render::{RenderError, Renderer, WgpuRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const SRC: &str = r#"
Stack {
    padding: 24px;
    spacing: 12px;
    background: #0a0a0a;
    Text { content: "Uni-UI"; size: 48px; color: #ffffff; }
    Text { content: "rendered from .uni"; size: 18px; color: #a3a3a3; }
    Rect { width: 240px; height: 96px; color: #7d39eb; corner_radius: 16px; }
}
"#;

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    doc: Option<uni_ir::Document>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Uni-UI — rendered from .uni")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        match uni_dsl::parse(SRC) {
            Ok(doc) => self.doc = Some(doc),
            Err(e) => {
                eprintln!("uni-dsl parse error: {e:?}");
                event_loop.exit();
                return;
            }
        }
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

                if let Some(doc) = self.doc.as_ref() {
                    let scene = lower(doc, (logical_w, logical_h));
                    match renderer.render(&scene) {
                        Ok(()) => {}
                        Err(RenderError::SurfaceLost) => {
                            let s = window.inner_size();
                            renderer.resize(s.width, s.height, window.scale_factor());
                        }
                        Err(e) => eprintln!("render error: {e}"),
                    }
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
