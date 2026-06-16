//! `cargo run -p uni-render --example input`
//!
//! Proves the **input plumbing** (rung 3): it renders the same minimal scene as
//! `hello`, but every window event is fed through
//! [`uni_render::translate_window_event`] and the resulting renderer-agnostic
//! [`uni_render::InputEvent`]s are printed to stderr in **logical pixels**.
//!
//! Move the mouse, click (left/right/middle), scroll, and press keys — you'll
//! see the `InputEvent`s the rest of the engine (`uni-core` hit-testing / event
//! routing) will consume. Coordinates are logical px regardless of HiDPI scale.

use std::sync::Arc;

use uni_render::{translate_window_event, DrawCmd, InputEvent, Renderer, Scene, WgpuRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Build a minimal scene (background + a centered card + label) so there's
/// something on screen to point at.
fn build_scene(logical_w: f32, logical_h: f32) -> Scene {
    let mut scene: Scene = vec![DrawCmd::FilledRect {
        x: 0.0,
        y: 0.0,
        w: logical_w,
        h: logical_h,
        color: 0x0a0a0aff,
        corner_radius: 0.0,
        rotation: 0.0,
    }];

    let card_w = 360.0_f32.min(logical_w - 40.0).max(80.0);
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
    scene.push(DrawCmd::Text {
        x: card_x + 24.0,
        y: card_y + card_h / 2.0 - 18.0,
        content: "Input demo".to_string(),
        size: 32.0,
        color: 0x0a0a0aff,
    });

    scene
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    /// Last known cursor position in logical px, threaded through the
    /// translator so click events carry coordinates.
    cursor: (f32, f32),
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Uni-UI — input")
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
                eprintln!(
                    "uni-render input demo: move / click / scroll / type to see InputEvents."
                );
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

        // Surface input first: translate every window event and print the
        // resulting renderer-agnostic InputEvent (logical px).
        if let Some(input) = translate_window_event(&event, window.scale_factor(), &mut self.cursor)
        {
            match &input {
                InputEvent::PointerMoved { x, y } => {
                    eprintln!("input: PointerMoved ({x:.1}, {y:.1})")
                }
                InputEvent::PointerDown { x, y, button } => {
                    eprintln!("input: PointerDown ({x:.1}, {y:.1}) {button:?}")
                }
                InputEvent::PointerUp { x, y, button } => {
                    eprintln!("input: PointerUp ({x:.1}, {y:.1}) {button:?}")
                }
                InputEvent::Scroll { dx, dy } => {
                    eprintln!("input: Scroll (dx={dx:.1}, dy={dy:.1})")
                }
                InputEvent::KeyDown { key } => eprintln!("input: KeyDown {key:?}"),
                InputEvent::KeyUp { key } => eprintln!("input: KeyUp {key:?}"),
            }
        }

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
