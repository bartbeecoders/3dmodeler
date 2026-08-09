//! The view and projection matrices a draw call needs.
//!
//! [`crate::camera::BlenderCamera`] is the *interactive* camera — orbit, pan,
//! zoom, numpad views — and it answers screen-space questions (`project`,
//! `pick_ray`, `world_per_pixel_at`) with direct trigonometry rather than with
//! matrices. This type is the other half: what that camera looks like to the
//! GPU, produced by `BlenderCamera::camera` once per frame and handed to the
//! render passes.
//!
//! Two things here differ from the `three_d` camera it replaces, and both are
//! deliberate.
//!
//! **Depth is reverse-Z: the near plane maps to 1 and the far plane to 0.**
//! cgmath — and so three-d — emits OpenGL's symmetric `[-1, 1]`. Aether's
//! `CONTRACT.md` rule 1 makes reverse-Z non-negotiable and every one of its
//! pipelines compares depth with `Greater`, so a camera built the ordinary way
//! sorts the scene inside out. That failure does not look like a matrix bug: it
//! reads as z-fighting and as near geometry disappearing behind far geometry.
//!
//! The matrices therefore come from [`aether_math`] rather than from glam or
//! from anything written here — the convention needs exactly one definition,
//! and it belongs with the pipelines that depend on it. Both functions assume a
//! right-handed view space looking down `-Z`, which is what
//! [`Mat4::look_at_rh`] produces.
//!
//! **Orthographic height is absolute world units.** three-d multiplied the
//! height it was given by the camera-to-target distance, so every call site had
//! to pass a height *per unit distance* and explain itself in a comment. Here
//! the height is the world-space height of the visible box, and the caller that
//! wants ortho to frame the same content as perspective computes that directly.

use aether_math::{ortho_reverse_z, perspective_reverse_z};

use super::math::{Angle, Mat4, Vec3, Viewport};

/// How the camera flattens the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Projection {
    /// Vertical field of view.
    Perspective { fov_y: Angle },
    /// World-space height of the visible box. See the module note.
    Orthographic { height: f32 },
}

/// A positioned camera and the matrices it projects with.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    viewport: Viewport,
    position: Vec3,
    target: Vec3,
    up: Vec3,
    projection_kind: Projection,
    z_near: f32,
    z_far: f32,
    view: Mat4,
    projection: Mat4,
}

impl Camera {
    /// A camera with a perspective projection, looking from `position` at
    /// `target`.
    pub fn new_perspective(
        viewport: Viewport,
        position: Vec3,
        target: Vec3,
        up: Vec3,
        fov_y: Angle,
        z_near: f32,
        z_far: f32,
    ) -> Self {
        Self::new(
            viewport,
            position,
            target,
            up,
            Projection::Perspective { fov_y },
            z_near,
            z_far,
        )
    }

    /// A camera with an orthographic projection showing `height` world units
    /// vertically, regardless of how far away `target` is.
    pub fn new_orthographic(
        viewport: Viewport,
        position: Vec3,
        target: Vec3,
        up: Vec3,
        height: f32,
        z_near: f32,
        z_far: f32,
    ) -> Self {
        Self::new(
            viewport,
            position,
            target,
            up,
            Projection::Orthographic { height },
            z_near,
            z_far,
        )
    }

    fn new(
        viewport: Viewport,
        position: Vec3,
        target: Vec3,
        up: Vec3,
        projection_kind: Projection,
        z_near: f32,
        z_far: f32,
    ) -> Self {
        let aspect = viewport.aspect();
        let projection = match projection_kind {
            Projection::Perspective { fov_y } => {
                perspective_reverse_z(fov_y.radians(), aspect, z_near, z_far)
            }
            Projection::Orthographic { height } => {
                let half_h = 0.5 * height;
                let half_w = half_h * aspect;
                ortho_reverse_z(-half_w, half_w, -half_h, half_h, z_near, z_far)
            }
        };
        Self {
            viewport,
            position,
            target,
            up,
            projection_kind,
            z_near,
            z_far,
            view: Mat4::look_at_rh(position, target, up),
            projection,
        }
    }

    pub fn view(&self) -> Mat4 {
        self.view
    }

    pub fn projection(&self) -> Mat4 {
        self.projection
    }

    /// The single matrix most shaders actually want.
    pub fn view_projection(&self) -> Mat4 {
        self.projection * self.view
    }

    pub fn position(&self) -> Vec3 {
        self.position
    }

    pub fn target(&self) -> Vec3 {
        self.target
    }

    pub fn up(&self) -> Vec3 {
        self.up
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn projection_kind(&self) -> Projection {
        self.projection_kind
    }

    pub fn z_near(&self) -> f32 {
        self.z_near
    }

    pub fn z_far(&self) -> f32 {
        self.z_far
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::math::degrees;

    fn vp() -> Viewport {
        Viewport::new_at_origo(800, 400)
    }

    /// Clip-space position of a world point, perspective divide applied.
    fn ndc(cam: &Camera, p: Vec3) -> Vec3 {
        let clip = cam.view_projection() * p.extend(1.0);
        clip.truncate() / clip.w
    }

    #[test]
    fn looking_down_an_axis_puts_the_target_at_the_centre() {
        let cam = Camera::new_perspective(
            vp(),
            Vec3::new(0.0, -10.0, 0.0),
            Vec3::ZERO,
            Vec3::Z,
            degrees(45.0),
            0.1,
            100.0,
        );
        let c = ndc(&cam, Vec3::ZERO);
        assert!(c.x.abs() < 1e-5 && c.y.abs() < 1e-5, "target landed at {c:?}");
    }

    #[test]
    fn depth_is_reverse_z_as_every_aether_pipeline_expects() {
        // The bug this guards: the ordinary conventions both map the near plane
        // to the *low* end — cgmath (and so three-d) to -1, glam's
        // `perspective_rh` to 0. Aether compares depth with `Greater`, so
        // either one sorts the scene inside out: near geometry loses to far
        // geometry and vanishes, which does not read as a matrix bug.
        let (near, far) = (1.0, 51.0);
        let cam = Camera::new_perspective(
            vp(),
            Vec3::ZERO,
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::Z,
            degrees(45.0),
            near,
            far,
        );
        let at_near = ndc(&cam, Vec3::new(0.0, -near, 0.0)).z;
        let at_far = ndc(&cam, Vec3::new(0.0, -far, 0.0)).z;
        assert!((at_near - 1.0).abs() < 1e-4, "near plane mapped to {at_near}, expected 1");
        assert!(at_far.abs() < 1e-4, "far plane mapped to {at_far}, expected 0");
    }

    #[test]
    fn orthographic_depth_runs_the_same_way_as_perspective() {
        // Two projections disagreeing about which end is near is the same bug,
        // visible only when the user presses numpad 1 and the scene inverts.
        let cam = Camera::new_orthographic(
            vp(),
            Vec3::ZERO,
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::Z,
            4.0,
            1.0,
            51.0,
        );
        assert!((ndc(&cam, Vec3::new(0.0, -1.0, 0.0)).z - 1.0).abs() < 1e-4);
        assert!(ndc(&cam, Vec3::new(0.0, -51.0, 0.0)).z.abs() < 1e-4);
    }

    #[test]
    fn orthographic_height_is_absolute_world_units() {
        // The three-d version scaled this by the camera-target distance. Half
        // the height above the target must land exactly on the top edge, at
        // whatever distance the camera happens to sit.
        let height = 4.0;
        for distance in [5.0_f32, 50.0] {
            let cam = Camera::new_orthographic(
                vp(),
                Vec3::new(0.0, -distance, 0.0),
                Vec3::ZERO,
                Vec3::Z,
                height,
                0.1,
                1000.0,
            );
            let top = ndc(&cam, Vec3::new(0.0, 0.0, 0.5 * height));
            assert!(
                (top.y - 1.0).abs() < 1e-5,
                "at distance {distance} the top edge landed at {}",
                top.y
            );
        }
    }

    #[test]
    fn the_visible_box_widens_with_the_viewport_aspect() {
        // A 2:1 viewport must show twice as much horizontally as vertically,
        // or every non-square window stretches the scene.
        let height = 4.0;
        let cam = Camera::new_orthographic(
            vp(),
            Vec3::new(0.0, -10.0, 0.0),
            Vec3::ZERO,
            Vec3::Z,
            height,
            0.1,
            100.0,
        );
        let right = ndc(&cam, Vec3::new(height, 0.0, 0.0)); // aspect 2.0 -> half-width 4.0
        assert!((right.x - 1.0).abs() < 1e-5, "right edge landed at {}", right.x);
    }
}
