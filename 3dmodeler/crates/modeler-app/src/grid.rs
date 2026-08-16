//! Reference grid on the XY ground plane (Z up, Blender convention).
//!
//! The lines themselves come from the engine's overlay pass, which draws them
//! after tonemapping and against the scene's depth buffer: a grid line is a
//! fact about the tool rather than about the light in the room, so it is not
//! exposed or graded, but a line behind a wall is still behind the wall.
//!
//! What is left here is the part that is this app's rather than the engine's:
//! which plane, how far, and the axis colours Blender users expect.

use aether_render::passes::overlay::{GridPlane, OverlayDraws};

use crate::gfx::*;

/// How far the grid reaches from the origin, in metres.
const EXTENT: f32 = 50.0;
/// Every tenth line is a major one.
const MAJOR_EVERY: u32 = 10;

const X_AXIS_COLOR: Srgba = Srgba::new(174, 66, 55, 255); // Blender-ish red
const Y_AXIS_COLOR: Srgba = Srgba::new(96, 148, 58, 255); // Blender-ish green
const Z_AXIS_COLOR: Srgba = Srgba::new(58, 98, 170, 255); // Blender-ish blue

fn linear(color: Srgba) -> aether_math::Vec4 {
    let v = color.to_linear();
    aether_math::Vec4::new(v.x, v.y, v.z, v.w)
}

/// Draws the ground grid and the two ground axes.
pub fn draw(draws: &mut OverlayDraws, spacing: f32, minor: [u8; 3], major: [u8; 3]) {
    let spacing = spacing.clamp(0.05, 10.0);
    draws.grid(
        GridPlane::Xy,
        EXTENT,
        spacing,
        linear(Srgba::new(minor[0], minor[1], minor[2], 255)),
        linear(Srgba::new(major[0], major[1], major[2], 255)),
        MAJOR_EVERY,
    );
    // `grid` skips the centre lines so the axes can go there instead.
    axis(draws, aether_math::Vec3::X, X_AXIS_COLOR);
    axis(draws, aether_math::Vec3::Y, Y_AXIS_COLOR);
}

/// The zero axes for a side-on orthographic view, where the floor grid is
/// edge-on and shows nothing: the ground axis of the faced plane, plus Z.
pub fn draw_zero_lines(draws: &mut OverlayDraws, plane: crate::camera::VerticalPlane) {
    let ground = match plane {
        crate::camera::VerticalPlane::Xz => (aether_math::Vec3::X, X_AXIS_COLOR),
        crate::camera::VerticalPlane::Yz => (aether_math::Vec3::Y, Y_AXIS_COLOR),
    };
    axis(draws, ground.0, ground.1);
    axis(draws, aether_math::Vec3::Z, Z_AXIS_COLOR);
}

fn axis(draws: &mut OverlayDraws, direction: aether_math::Vec3, color: Srgba) {
    draws.line(direction * -EXTENT, direction * EXTENT, linear(color));
}
