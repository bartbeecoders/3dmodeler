//! Off-screen render from a scene camera object (Blender F12 / live view).
//!
//! Builds a three-d perspective camera from the object's world transform
//! (look along local −Z, up = local +Y) and draws solid scene meshes into
//! a reusable texture — no grid, outlines, reference images, or gizmo markers.

use crate::gfx::*;
use crate::scene_render::{SceneLights, SceneRender};
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Primitive, Scene, Transform};

/// Live camera preview resolution (physical pixels).
pub const LIVE_WIDTH: u32 = 960;
pub const LIVE_HEIGHT: u32 = 540;

/// Pick which camera to render: selected/active camera if any, else the
/// first camera in the scene.
pub fn resolve_camera(
    scene: &Scene,
    selected: impl IntoIterator<Item = ObjectId>,
    active: Option<ObjectId>,
) -> Option<ObjectId> {
    if let Some(id) = active {
        if scene.object(id).is_some_and(|o| o.primitive.is_camera() && o.visible) {
            return Some(id);
        }
    }
    for id in selected {
        if scene.object(id).is_some_and(|o| o.primitive.is_camera() && o.visible) {
            return Some(id);
        }
    }
    scene
        .objects()
        .iter()
        .find(|o| o.primitive.is_camera() && o.visible)
        .map(|o| o.id)
}

/// Build a three-d camera from a scene camera object's world transform.
pub fn three_d_camera(
    world: &Transform,
    fov_deg: f32,
    clip_start: f32,
    clip_end: f32,
    width: u32,
    height: u32,
) -> Camera {
    let pos = world.location;
    let rot = world.rotation;
    // Blender: look along local −Z, up = local +Y
    let forward = rot * Vec3::new(0.0, 0.0, -1.0);
    let up = rot * Vec3::Y;
    let target = pos + forward;
    let viewport = Viewport::new_at_origo(width, height);
    let near = clip_start.max(0.001);
    let far = clip_end.max(near + 0.01);
    Camera::new_perspective(
        viewport,
        vec3(pos.x, pos.y, pos.z),
        vec3(target.x, target.y, target.z),
        vec3(up.x, up.y, up.z),
        degrees(fov_deg.clamp(1.0, 170.0)),
        near,
        far,
    )
}

/// Reusable off-screen color + depth targets for camera rendering.
/// Avoids allocating GPU textures every frame during live preview.
pub struct CameraRenderTarget {
    width: u32,
    height: u32,
    color: Texture2D,
    depth: DepthTexture2D,
    /// Flipped top-left RGBA scratch (owned; returned as a slice).
    rgba: Vec<u8>,
}

impl CameraRenderTarget {
    pub fn new(context: &Context, width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            color: Texture2D::new_empty::<[u8; 4]>(
                context,
                width,
                height,
                Interpolation::Nearest,
                Interpolation::Nearest,
                None,
                Wrapping::ClampToEdge,
                Wrapping::ClampToEdge,
            ),
            depth: DepthTexture2D::new::<f32>(
                context,
                width,
                height,
                Wrapping::ClampToEdge,
                Wrapping::ClampToEdge,
            ),
            rgba: vec![0u8; (width * height * 4) as usize],
            width,
            height,
        }
    }

    pub fn ensure_size(&mut self, context: &Context, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height {
            return;
        }
        *self = Self::new(context, width, height);
    }

    /// Render from `camera_id`. Returns `(width, height, rgba top-left)`.
    pub fn render(
        &mut self,
        scene: &Scene,
        camera_id: ObjectId,
        scene_render: &SceneRender,
        lights: &SceneLights,
        bg: [f32; 3],
    ) -> Result<(u32, u32, &[u8]), String> {
        let object = scene
            .object(camera_id)
            .ok_or_else(|| "camera object not found".to_string())?;
        let (fov_deg, clip_start, clip_end) = match object.primitive {
            Primitive::Camera {
                fov_deg,
                clip_start,
                clip_end,
            } => (fov_deg, clip_start, clip_end),
            _ => return Err("object is not a camera".into()),
        };
        let world = scene.world_transform(camera_id);
        let cam = three_d_camera(
            &world,
            fov_deg,
            clip_start,
            clip_end,
            self.width,
            self.height,
        );

        let models = scene_render.camera_render_models();
        let target =
            RenderTarget::new(self.color.as_color_target(None), self.depth.as_depth_target());
        target
            .clear(ClearState::color_and_depth(bg[0], bg[1], bg[2], 1.0, 1.0))
            .render(&cam, &models, &lights.active());

        // three-d's read() already flips Y (OpenGL bottom-left → top-left).
        // Do not flip again or the live view appears upside-down.
        let pixels: Vec<[u8; 4]> = self.color.as_color_target(None).read();
        let needed = pixels.len() * 4;
        if self.rgba.len() != needed {
            self.rgba.resize(needed, 0);
        }
        for (i, p) in pixels.iter().enumerate() {
            let o = i * 4;
            self.rgba[o] = p[0];
            self.rgba[o + 1] = p[1];
            self.rgba[o + 2] = p[2];
            self.rgba[o + 3] = 255;
        }
        Ok((self.width, self.height, &self.rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::glam::Quat;

    #[test]
    fn default_camera_looks_toward_positive_y() {
        // +90° about X: local −Z → world +Y, local +Y → world +Z
        let rot = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        let forward = rot * Vec3::new(0.0, 0.0, -1.0);
        let up = rot * Vec3::Y;
        assert!(
            (forward - Vec3::Y).length() < 1e-4,
            "forward={forward:?}"
        );
        assert!((up - Vec3::Z).length() < 1e-4, "up={up:?}");
    }

    #[test]
    fn resolve_prefers_selected_camera() {
        let mut scene = Scene::new();
        let a = scene.add_object(Primitive::default_camera(), Transform::default());
        let mut t = Transform::default();
        t.location = Vec3::new(1.0, 0.0, 0.0);
        let b = scene.add_object(Primitive::default_camera(), t);
        assert_eq!(resolve_camera(&scene, [b], None), Some(b));
        assert_eq!(resolve_camera(&scene, [], Some(b)), Some(b));
        assert_eq!(resolve_camera(&scene, [], None), Some(a));
    }
}
