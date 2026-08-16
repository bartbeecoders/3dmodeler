//! The app's graphics and input layer.
//!
//! This is what `three_d` used to be. The app took its window, its GL context,
//! its egui painter, its event types and its vector maths from that one crate;
//! this module supplies the same things over wgpu, winit, `egui-wgpu` and
//! Aether's renderer.
//!
//! The split is deliberate:
//!
//! | module | what it owns |
//! |---|---|
//! | [`math`] | vectors, angles, colours, viewport — on glam |
//! | [`camera`] | the view and projection matrices a draw call needs |
//! | [`event`] | input events, shaped as the tools already match on them |
//! | [`frame`] | what one frame is handed, and the winit translation behind it |
//! | [`window`] | the OS window's wgpu surface (native only) |
//! | [`egui_paint`] | egui's triangles, on wgpu |
//! | [`gui`] | the egui frame: app events in, painted interface out |
//!
//! Re-exported flat, because the call sites say `use crate::gfx::*` where they
//! used to say `use three_d::*` and the point is that the rest of the file did
//! not have to change.

// This layer is written to replace `three_d` whole, but its consumers arrive
// one commit at a time: the renderer has not been ported yet, so the camera's
// projection variants, half the maths vocabulary and the painter's diagnostics
// are all reachable only from the tests. Warning about each of them says
// nothing except "the port is not finished", which is already written down at
// the top of this file. Comes off with the renderer.
#![allow(dead_code, unused_imports)]

pub mod camera;
pub mod egui_paint;
pub mod event;
pub mod frame;
pub mod gui;
pub mod math;
pub mod texture;
pub mod viewport;
#[cfg(not(target_arch = "wasm32"))]
pub mod window;

pub use egui_paint::EguiPainter;
pub use gui::Gui;
pub use texture::{CpuTexture, TextureData};
pub use viewport::Viewport3d;
#[cfg(not(target_arch = "wasm32"))]
pub use window::GfxWindow;

pub use camera::{Camera, Projection};
pub use event::{Event, Key, Modifiers, MouseButton};
pub use frame::{FrameInput, FrameOutput, FramePaint};
#[cfg(not(target_arch = "wasm32"))]
pub use frame::FrameInputGenerator;
pub use math::{
    degrees, radians, vec2, vec3, vec4, Angle, Magnitude, Mat3, Mat4, PhysicalPoint, Quat, Srgba,
    ToVec, Vec2, Vec3, Vec4, Viewport,
};

/// egui, re-exported so call sites keep saying `gfx::egui` where they said
/// `three_d::egui`.
///
/// Both the widgets and the painter must come from *one* egui, and a second
/// copy in the tree is a type error a hundred lines from its cause. Naming it
/// here means there is one place to look.
pub use egui;
