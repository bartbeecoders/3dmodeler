//! OpenGL context creation for the native window.
//!
//! Adapted from three-d's `WindowedContext::from_winit_window` (MIT) with
//! one behavioral change: `set_swap_interval` failures are tolerated.
//! three-d calls it unconditionally — even `vsync: false` sets an interval
//! of 0 — but VirtualBox / remote-desktop WGL has no swap-control extension
//! AT ALL, so any interval call fails and the app never started there. Here
//! the call failing just means "no vsync": the main loop paces itself.

#![cfg(not(target_arch = "wasm32"))]

use glutin::prelude::*;
use three_d::{Context, HardwareAcceleration, SurfaceSettings};

pub struct GlWindow {
    context: Context,
    surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    glutin_context: glutin::context::PossiblyCurrentContext,
    /// True when the driver honors swap-control: swap_buffers blocks on the
    /// display refresh and the render loop needs no pacing of its own.
    pub vsync: bool,
}

impl std::ops::Deref for GlWindow {
    type Target = Context;
    fn deref(&self) -> &Context {
        &self.context
    }
}

impl GlWindow {
    pub fn new(
        window: &winit::window::Window,
        settings: SurfaceSettings,
    ) -> Result<Self, String> {
        use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle};
        let raw_display_handle = window.raw_display_handle();
        let raw_window_handle = window.raw_window_handle();

        // display backend preference, exactly as three-d picks it
        #[cfg(target_os = "windows")]
        let preference =
            glutin::display::DisplayApiPreference::WglThenEgl(Some(raw_window_handle));
        #[cfg(target_os = "linux")]
        let preference = glutin::display::DisplayApiPreference::EglThenGlx(Box::new(
            winit::platform::x11::register_xlib_error_hook,
        ));
        #[cfg(target_os = "macos")]
        let preference = glutin::display::DisplayApiPreference::Cgl;

        let gl_display =
            unsafe { glutin::display::Display::new(raw_display_handle, preference) }
                .map_err(|e| format!("no GL display: {e}"))?;

        let hardware_acceleration = match settings.hardware_acceleration {
            HardwareAcceleration::Required => Some(true),
            HardwareAcceleration::Preferred => None,
            HardwareAcceleration::Off => Some(false),
        };
        let mut config_template = glutin::config::ConfigTemplateBuilder::new()
            .prefer_hardware_accelerated(hardware_acceleration)
            .with_depth_size(settings.depth_buffer);
        if settings.multisamples > 0 {
            config_template = config_template.with_multisampling(settings.multisamples);
        }
        let config_template = config_template
            .with_stencil_size(settings.stencil_buffer)
            .compatible_with_native_window(raw_window_handle)
            .build();
        let config = unsafe { gl_display.find_configs(config_template) }
            .map_err(|e| format!("no matching GL config: {e}"))?
            .next()
            .ok_or("no matching GL config")?;

        let context_attributes =
            glutin::context::ContextAttributesBuilder::new().build(Some(raw_window_handle));
        let (width, height): (u32, u32) = window.inner_size().into();
        let width = std::num::NonZeroU32::new(width.max(1)).unwrap();
        let height = std::num::NonZeroU32::new(height.max(1)).unwrap();
        let surface_attributes =
            glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
                .build(raw_window_handle, width, height);

        let gl_context = unsafe { gl_display.create_context(&config, &context_attributes) }
            .map_err(|e| format!("GL context creation failed: {e}"))?;
        let gl_surface =
            unsafe { gl_display.create_window_surface(&config, &surface_attributes) }
                .map_err(|e| format!("GL surface creation failed: {e}"))?;
        let gl_context = gl_context
            .make_current(&gl_surface)
            .map_err(|e| format!("make_current failed: {e}"))?;

        // THE deviation from three-d: swap-control may simply not exist
        // (VirtualBox, some remote desktops). That is not fatal.
        let interval = if settings.vsync {
            glutin::surface::SwapInterval::Wait(std::num::NonZeroU32::new(1).unwrap())
        } else {
            glutin::surface::SwapInterval::DontWait
        };
        let swap_control_ok = gl_surface.set_swap_interval(&gl_context, interval).is_ok();
        let vsync = settings.vsync && swap_control_ok;

        let context = Context::from_gl_context(std::sync::Arc::new(unsafe {
            three_d::context::Context::from_loader_function(|s| {
                let s = std::ffi::CString::new(s)
                    .expect("failed to construct C string from string for gl proc address");
                gl_display.get_proc_address(&s)
            })
        }))
        .map_err(|e| format!("GL feature detection failed: {e}"))?;

        Ok(Self { context, surface: gl_surface, glutin_context: gl_context, vsync })
    }

    pub fn resize(&self, physical_size: winit::dpi::PhysicalSize<u32>) {
        let width = std::num::NonZeroU32::new(physical_size.width.max(1)).unwrap();
        let height = std::num::NonZeroU32::new(physical_size.height.max(1)).unwrap();
        self.surface.resize(&self.glutin_context, width, height);
    }

    pub fn swap_buffers(&self) -> Result<(), String> {
        self.surface
            .swap_buffers(&self.glutin_context)
            .map_err(|e| format!("swap_buffers failed: {e}"))
    }
}
