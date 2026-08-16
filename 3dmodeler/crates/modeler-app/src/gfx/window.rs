//! The native window's GPU surface.
//!
//! This is what `gl_window.rs` was, one API down: it takes the winit window and
//! hands back something you can draw into once per frame. What changed is the
//! shape of "once per frame". GL had a context that was implicitly current and a
//! `swap_buffers` at the end; wgpu has an explicit swapchain, so a frame is
//! *acquired*, drawn into through a view, and *presented*.
//!
//! Three decisions in here are load-bearing.
//!
//! **The surface format must be sRGB.** Everything the app draws — the engine's
//! tonemapped output and [`super::EguiPainter`]'s interface — writes linear
//! values and relies on the target encoding them on the way out. Configure the
//! surface with a plain `Rgba8Unorm` and every colour lands too bright, which
//! reads as "the theme looks washed out" rather than as a format mistake.
//!
//! **The window has to outlive the surface**, which is why it is held here as an
//! `Arc` rather than borrowed. `wgpu::Surface<'static>` is not a lifetime dodge:
//! the surface genuinely refers to the OS window for as long as it exists, and
//! sharing ownership is the only way to say that once the window also has to be
//! reachable from the event loop.
//!
//! **A lost surface is normal.** Resizing, moving between monitors, locking the
//! screen and waking from sleep all invalidate the swapchain. Every one of them
//! arrives as an error from [`Self::acquire`], and the answer to all of them is
//! to reconfigure and skip the frame rather than to fail.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use aether_render::Gpu;

pub struct GfxWindow {
    /// Shared with the event loop: the surface below borrows this window for as
    /// long as it lives.
    window: Arc<winit::window::Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Gpu,
    /// True when the surface is presenting on the display's refresh, so the
    /// render loop needs no pacing of its own. `AutoNoVsync` would spin the GPU
    /// as fast as it can, so the loop sleeps instead — the same arrangement
    /// `gl_window` had for drivers with no swap-control extension.
    pub vsync: bool,
}

impl GfxWindow {
    pub fn new(window: Arc<winit::window::Window>) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: aether_render::aether_gpu::backends(),
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("no drawable surface for this window: {e}"))?;

        // The adapter is chosen against the surface it will present to: on a
        // multi-GPU laptop the fastest adapter is not always the one wired to
        // the display, and one that cannot present is useless here however fast
        // it is.
        let gpu = pollster::block_on(Gpu::from_instance(&instance, Some(&surface)))
            .map_err(|e| format!("{e}"))?;

        let caps = surface.get_capabilities(&gpu.adapter);
        let format = preferred_format(&caps.formats)
            .ok_or("this GPU offers no sRGB surface format")?;

        // AutoVsync is guaranteed present by wgpu, so this is a preference
        // rather than a search: it is named here so the fallback below is a
        // deliberate choice and not whatever `Default` happened to mean.
        let vsync = caps.present_modes.contains(&wgpu::PresentMode::AutoVsync)
            || caps.present_modes.contains(&wgpu::PresentMode::Fifo);
        let present_mode =
            if vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync };

        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&gpu.device, &config);

        Ok(Self { window, surface, config, gpu, vsync })
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.gpu.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.gpu.queue
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn window(&self) -> &winit::window::Window {
        &self.window
    }

    /// Reconfigures the swapchain for a new window size.
    ///
    /// A minimised window reports 0x0 and a surface of that size is a
    /// validation error, so the zero is clamped away and the stale
    /// configuration kept until the window comes back.
    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        let max = self.gpu.limits.max_texture_dimension_2d;
        self.config.width = size.width.min(max);
        self.config.height = size.height.min(max);
        self.surface.configure(&self.gpu.device, &self.config);
    }

    /// Reconfigures at the current size, after the swapchain has been lost.
    pub fn reconfigure(&mut self) {
        self.surface.configure(&self.gpu.device, &self.config);
    }

    /// The next image to draw into, or `None` if this frame should be skipped.
    ///
    /// Skipping is the correct handling for every recoverable case: the frame
    /// that was going to be drawn is already out of date, and the next one is
    /// microseconds away.
    pub fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            Ok(frame) => Some(frame),
            // The window changed under us — reconfigure and let the next frame
            // have it.
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.reconfigure();
                None
            }
            // The compositor is busy, or the window is not on screen at all.
            Err(wgpu::SurfaceError::Timeout | wgpu::SurfaceError::Other) => None,
            Err(e @ wgpu::SurfaceError::OutOfMemory) => {
                panic!("the GPU ran out of memory acquiring a frame: {e}")
            }
        }
    }
}

/// The first sRGB format the surface offers, preferring the one it lists first.
///
/// Returns `None` rather than falling back to a linear format: drawing the whole
/// interface through the wrong transfer function is worse than a clear error,
/// and there is no adapter in wgpu's supported set that offers neither
/// `Bgra8UnormSrgb` nor `Rgba8UnormSrgb`.
fn preferred_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    formats.iter().copied().find(|f| f.is_srgb())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_srgb_format_is_chosen_over_a_linear_one_listed_first() {
        // Surfaces commonly list the linear format first. Taking `formats[0]`
        // — the obvious thing, and what most examples do — leaves every colour
        // un-encoded on write and the whole interface too bright.
        let offered = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ];
        assert_eq!(preferred_format(&offered), Some(wgpu::TextureFormat::Bgra8UnormSrgb));
    }

    #[test]
    fn no_srgb_format_is_reported_rather_than_guessed_at() {
        let offered = [wgpu::TextureFormat::Bgra8Unorm];
        assert_eq!(preferred_format(&offered), None);
    }
}
