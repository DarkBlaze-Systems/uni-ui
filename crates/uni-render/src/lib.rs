//! uni-render — the Uni-UI rendering backend ("first pixels").
//!
//! This crate is intentionally split in two layers:
//!
//! 1. A **renderer-agnostic** layer ([`Scene`], [`DrawCmd`], [`Renderer`]).
//!    This is the only surface `uni-core` needs to know about: it lowers a
//!    `uni-ir` `Document` into a [`Scene`] (a flat `Vec<DrawCmd>`) and hands
//!    that to whatever [`Renderer`] is wired up. Nothing here mentions wgpu,
//!    glyphon, winit, or any GPU type — so an alternative backend (software,
//!    test-capture, SVG, …) can implement [`Renderer`] without pulling the
//!    GPU stack.
//!
//! 2. A concrete [`WgpuRenderer`] backend that tessellates rounded rects with
//!    `lyon`, draws them through a small wgpu pipeline, and renders text via
//!    `cosmic-text` (shaping/layout) + `glyphon` (atlas + draw into the wgpu
//!    render pass).
//!
//! Coordinates are **logical pixels**, origin top-left, y-down. The backend
//! sets up an orthographic projection that maps `(0,0)..(width,height)` to
//! clip space, so a `DrawCmd` placed at `(x, y)` lands at that logical pixel
//! regardless of the surface's physical/HiDPI scale.

mod color;
pub use color::Rgba;

mod scene;
pub use scene::{DrawCmd, Scene};

mod renderer;
pub use renderer::{RenderError, Renderer};

mod wgpu_backend;
pub use wgpu_backend::WgpuRenderer;
