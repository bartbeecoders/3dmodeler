//! Off-screen render from a scene camera object (Blender F12 / live view).
//!
//! Builds a perspective camera from the object's world transform (look along
//! local −Z, up = local +Y) and draws solid scene meshes into a reusable
//! texture — no grid, outlines, reference images, or gizmo markers.
//!
//! # Why this owns a second renderer
//!
//! The viewport's renderer is sized to the window, and a camera render is not:
//! the live preview is 960x540 and an agent asking through MCP picks whatever
//! it likes. Resizing the viewport's targets to take a picture and back again
//! would reallocate every full-screen buffer twice per frame, which is the most
//! expensive thing the frame can do.
//!
//! So each target owns a renderer at its own size. They draw the *same*
//! [`RenderScene`] — the meshes live on the GPU and are shared — with a
//! different camera and the editor's gizmos hidden.
//!
//! Temporal jitter is off here. It exists to give TAA sub-pixel samples to
//! accumulate over many frames, and a single still frame has no history to
//! accumulate into: left on, it renders the picture through a half-pixel offset
//! that nothing resolves.

use aether_render::{Gpu, Renderer, RendererConfig};

use crate::gfx::*;
use crate::scene_render::SceneRender;
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

/// Build a camera from a scene camera object's world transform.
pub fn camera_from_object(
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

/// A renderer sized for one camera image, and the pixels it read back.
pub struct CameraRenderTarget {
    renderer: Renderer,
    width: u32,
    height: u32,
    /// Top-left RGBA, owned so it can be handed out as a slice.
    rgba: Vec<u8>,
}

impl CameraRenderTarget {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self {
            renderer: {
                let mut renderer = Renderer::new(
                    gpu.clone(),
                    RendererConfig { width, height, temporal_jitter: false },
                );
                // The same exposure calibration as the viewport, so what F12
                // renders is what the viewport was showing.
                crate::gfx::viewport::calibrate_exposure(&mut renderer);
                renderer
            },
            width,
            height,
            rgba: Vec::new(),
        }
    }

    /// Matches this renderer's exposure to the viewport's lighting mode, so
    /// what a camera renders is what the viewport was showing.
    pub fn sync_exposure(&mut self, scene_lighting: bool) {
        crate::gfx::viewport::sync_exposure(&mut self.renderer, scene_lighting);
    }

    /// Matches the target to a requested size, reallocating only on a change.
    pub fn ensure_size(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if self.width == width && self.height == height {
            return;
        }
        self.renderer.resize(width, height);
        self.width = width;
        self.height = height;
    }

    /// Renders `camera_id`'s view of `render`. Returns `(width, height, rgba)`.
    ///
    /// `render` is taken mutably because the gizmos are hidden for the duration
    /// and put back afterwards — the caller's viewport is drawing the same
    /// scene and still wants them.
    pub fn render(
        &mut self,
        scene: &Scene,
        camera_id: ObjectId,
        render: &mut SceneRender,
    ) -> Result<(u32, u32, &[u8]), String> {
        let object = scene
            .object(camera_id)
            .ok_or_else(|| "camera object not found".to_string())?;
        let (fov_deg, clip_start, clip_end) = match object.primitive {
            Primitive::Camera { fov_deg, clip_start, clip_end } => {
                (fov_deg, clip_start, clip_end)
            }
            _ => return Err("object is not a camera".into()),
        };
        let world = scene.world_transform(camera_id);
        let camera =
            camera_from_object(&world, fov_deg, clip_start, clip_end, self.width, self.height);

        let viewport_camera = render.scene.camera;
        render.scene.camera = crate::gfx::viewport::aether_camera(&camera);
        render.set_gizmos_visible(false);

        // A fixed step rather than the frame's: this is one still image, and
        // the only thing the renderer does with elapsed time is animate.
        self.renderer.render(&render.scene, 1.0 / 60.0);

        render.set_gizmos_visible(true);
        render.scene.camera = viewport_camera;

        self.rgba = self.renderer.read_output();
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if self.rgba.len() < expected {
            return Err(format!(
                "the camera render read back {} bytes for a {}x{} image",
                self.rgba.len(),
                self.width,
                self.height
            ));
        }
        Ok((self.width, self.height, &self.rgba[..expected]))
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
