//! The renderer-agnostic backend trait.

use crate::Scene;

/// A rendering backend. Implementors consume a [`Scene`] and put pixels
/// somewhere (a window surface, an offscreen target, an SVG buffer, …).
///
/// The trait is deliberately tiny and GPU-free so `uni-core` can depend on it
/// without depending on wgpu. The concrete [`crate::WgpuRenderer`] adds its own
/// constructor and window-surface plumbing on top.
pub trait Renderer {
    /// React to a surface-size change. `width`/`height` are **physical**
    /// pixels (what the window/surface reports); `scale_factor` is the HiDPI
    /// ratio so the backend can keep working in logical pixels.
    fn resize(&mut self, width: u32, height: u32, scale_factor: f64);

    /// Render one frame from `scene`. Returns `Err` on a recoverable surface
    /// error (e.g. lost/outdated swapchain) so the caller can retry next frame.
    fn render(&mut self, scene: &Scene) -> Result<(), RenderError>;
}

/// Errors a [`Renderer`] can surface to its caller.
#[derive(Debug)]
pub enum RenderError {
    /// The surface needs reconfiguring (lost/outdated). Caller should resize.
    SurfaceLost,
    /// The surface ran out of memory. Usually fatal.
    OutOfMemory,
    /// A frame was dropped (timeout / other transient). Safe to retry.
    Transient,
    /// Backend-specific failure with a message.
    Backend(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::SurfaceLost => write!(f, "render surface lost/outdated"),
            RenderError::OutOfMemory => write!(f, "render surface out of memory"),
            RenderError::Transient => write!(f, "transient frame error"),
            RenderError::Backend(m) => write!(f, "backend error: {m}"),
        }
    }
}

impl std::error::Error for RenderError {}
