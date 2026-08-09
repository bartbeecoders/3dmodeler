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
//! | [`event`] | input events, shaped as the tools already match on them |
//!
//! Re-exported flat, because the call sites say `use crate::gfx::*` where they
//! used to say `use three_d::*` and the point is that the rest of the file did
//! not have to change.

pub mod egui_paint;
pub mod event;
pub mod math;

pub use egui_paint::EguiPainter;

pub use event::{Event, Key, Modifiers, MouseButton};
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
