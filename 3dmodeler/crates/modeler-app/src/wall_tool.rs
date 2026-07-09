//! Wall drawing tool (Add ▸ Wall): click on the floor to place the wall's
//! start, move the mouse to rubber-band the segment (the real wall object
//! updates live), click again to plant it — the next segment chains from
//! that corner. Enter finishes keeping the current segment, Esc/RMB cancels
//! it and ends the tool. Segments honor the grid-snap toggle (Ctrl inverts,
//! like the modal operators); height and thickness come from the settings
//! (per-wall overrides live in the sidebar and the right-click menu).

use crate::selection::Selection;
use modeler_core::glam::{Quat, Vec3};
use modeler_core::{ObjectId, Primitive, Scene, Transform};
use three_d::{Event, Key, MouseButton, Viewport};

const MIN_SEGMENT: f32 = 0.05; // meters; shorter clicks end the tool

pub struct WallTool {
    active: bool,
    /// The segment being rubber-banded: object id + fixed start point (z=0).
    pending: Option<(ObjectId, Vec3)>,
    last_mouse: (f32, f32), // physical px, bottom-left origin
    ctrl_down: bool,
    height: f32,
    thickness: f32,
    /// Length of the pending segment, for the status line.
    current_length: f32,
}

impl WallTool {
    pub fn new() -> Self {
        Self {
            active: false,
            pending: None,
            last_mouse: (0.0, 0.0),
            ctrl_down: false,
            height: 2.5,
            thickness: 0.2,
            current_length: 0.0,
        }
    }

    /// Arm the tool; new walls use the settings' default height/thickness.
    pub fn start(&mut self, settings: &crate::settings::Settings) {
        self.active = true;
        self.pending = None;
        self.height = settings.default_wall_height.max(0.1);
        self.thickness = settings.default_wall_thickness.max(0.01);
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// A segment is being rubber-banded (used to batch undo checkpoints).
    pub fn drawing(&self) -> bool {
        self.pending.is_some()
    }

    /// Cancel everything (e.g. the simulation started): the pending segment
    /// is removed and the tool turns off.
    pub fn abort(&mut self, scene: &mut Scene) {
        if let Some((id, _)) = self.pending.take() {
            scene.remove_object(id);
        }
        self.active = false;
    }

    pub fn status_line(&self, unit: crate::settings::Unit) -> Option<String> {
        if !self.active {
            return None;
        }
        Some(match self.pending {
            None => "Wall: click the floor to start a wall   |   RMB/Esc cancel".to_string(),
            Some(_) => format!(
                "Wall: {} · click to plant the corner and continue   |   \
                 Enter finish · RMB/Esc cancel",
                unit.format(self.current_length)
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        camera: &crate::camera::BlenderCamera,
        viewport: Viewport,
        scene: &mut Scene,
        selection: &mut Selection,
        egui_owns_keyboard: bool,
        pointer_over_ui: bool,
        snap_to_grid: bool,
        grid_spacing: f32,
    ) {
        if !self.active {
            return;
        }
        let mut finish_keep = false;
        let mut cancel = false;
        let mut place: Option<(f32, f32)> = None;

        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, modifiers, .. } => {
                    self.last_mouse = (position.x, position.y);
                    self.ctrl_down = modifiers.ctrl;
                }
                Event::MousePress { button, position, modifiers, handled }
                    if !*handled && !pointer_over_ui =>
                {
                    self.last_mouse = (position.x, position.y);
                    self.ctrl_down = modifiers.ctrl;
                    match button {
                        MouseButton::Left => place = Some((position.x, position.y)),
                        MouseButton::Right => cancel = true,
                        MouseButton::Middle => continue, // camera keeps orbiting
                    }
                    *handled = true;
                }
                Event::KeyPress { kind, handled, .. } if !*handled => match kind {
                    Key::Escape => {
                        cancel = true;
                        *handled = true;
                    }
                    Key::Enter => {
                        finish_keep = true;
                        *handled = true;
                    }
                    _ => {} // numpad views etc. stay available
                },
                // the tool owns typed input: keep G/R/S/X/… inert while drawing
                Event::Text(text) if !egui_owns_keyboard && !text.is_empty() => {
                    text.clear();
                }
                _ => {}
            }
        }

        if cancel {
            self.abort(scene);
            return;
        }

        let snap = snap_to_grid != self.ctrl_down; // Ctrl inverts, like modal
        if let Some((x, y)) = place {
            if let Some(point) = ground_point(camera, viewport, x, y, snap, grid_spacing) {
                match self.pending.take() {
                    None => self.begin_segment(scene, selection, point),
                    Some((id, start)) => {
                        if (point - start).truncate().length() < MIN_SEGMENT {
                            // clicking in place: nothing to plant — done
                            scene.remove_object(id);
                            self.active = false;
                        } else {
                            self.update_segment(scene, id, start, point);
                            // chain: the next wall starts at this corner
                            self.begin_segment(scene, selection, point);
                        }
                    }
                }
            }
        }

        // rubber-band the pending segment to the mouse every frame
        if let Some((id, start)) = self.pending {
            if let Some(point) = ground_point(
                camera,
                viewport,
                self.last_mouse.0,
                self.last_mouse.1,
                snap,
                grid_spacing,
            ) {
                self.update_segment(scene, id, start, point);
            }
        }

        if finish_keep {
            if let Some((id, _)) = self.pending.take() {
                if self.current_length < MIN_SEGMENT {
                    scene.remove_object(id);
                }
            }
            self.active = false;
        }
    }

    fn begin_segment(&mut self, scene: &mut Scene, selection: &mut Selection, start: Vec3) {
        let id = scene.add_object(
            Primitive::Wall {
                length: MIN_SEGMENT,
                height: self.height,
                thickness: self.thickness,
            },
            Transform { location: start, ..Default::default() },
        );
        selection.set(vec![id], Some(id));
        self.pending = Some((id, start));
        self.current_length = 0.0;
    }

    /// Point the wall from `start` toward `end` (both on the floor). Height
    /// and thickness are kept — the sidebar can change them mid-draw.
    fn update_segment(&mut self, scene: &mut Scene, id: ObjectId, start: Vec3, end: Vec3) {
        let delta = (end - start).truncate();
        let length = delta.length().max(MIN_SEGMENT);
        self.current_length = length;
        let Some(object) = scene.object(id) else { return };
        let Primitive::Wall { height, thickness, .. } = object.primitive else { return };
        let rotation = Quat::from_rotation_z(delta.y.atan2(delta.x));
        let wanted = Primitive::Wall { length, height, thickness };
        if object.primitive == wanted
            && object.transform.rotation.abs_diff_eq(rotation, 1e-6)
            && object.transform.location.abs_diff_eq(start, 1e-6)
        {
            return; // avoid version bumps (and physics rebuilds) on idle frames
        }
        if let Some(object) = scene.object_mut(id) {
            object.primitive = wanted;
            object.transform.location = start;
            object.transform.rotation = rotation;
        }
    }
}

/// Intersect a viewport pick ray with the floor plane (z = 0), optionally
/// snapped to the grid.
fn ground_point(
    camera: &crate::camera::BlenderCamera,
    viewport: Viewport,
    x_px: f32,
    y_px: f32,
    snap: bool,
    grid_spacing: f32,
) -> Option<Vec3> {
    let (origin, direction) = camera.pick_ray(viewport, x_px, y_px);
    let (o, d) = (
        Vec3::new(origin.x, origin.y, origin.z),
        Vec3::new(direction.x, direction.y, direction.z),
    );
    if d.z.abs() < 1e-6 {
        return None; // looking along the floor
    }
    let t = -o.z / d.z;
    if t <= 0.0 {
        return None;
    }
    let mut p = o + d * t;
    p.z = 0.0;
    if snap && grid_spacing > 1e-6 {
        p.x = (p.x / grid_spacing).round() * grid_spacing;
        p.y = (p.y / grid_spacing).round() * grid_spacing;
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    #[test]
    fn segments_chain_and_cancel_removes_the_pending_wall() {
        let mut scene = Scene::new();
        let mut selection = Selection::default();
        let mut tool = WallTool::new();
        tool.start(&Settings::default());
        assert!(tool.active() && !tool.drawing());

        // first click: a pending wall appears at the start point
        tool.begin_segment(&mut scene, &mut selection, Vec3::new(1.0, 2.0, 0.0));
        assert_eq!(scene.objects().len(), 1);
        let (id, start) = tool.pending.unwrap();
        assert_eq!(selection.active(), Some(id));

        // rubber-band 3 m along +Y: length and rotation follow
        tool.update_segment(&mut scene, id, start, Vec3::new(1.0, 5.0, 0.0));
        let object = scene.object(id).unwrap();
        let Primitive::Wall { length, height, thickness } = object.primitive else {
            panic!("not a wall")
        };
        assert!((length - 3.0).abs() < 1e-5);
        assert!((height - 2.5).abs() < 1e-5, "default height must be 2.5 m");
        assert!((thickness - 0.2).abs() < 1e-5);
        let (axis, angle) = object.transform.rotation.to_axis_angle();
        assert!(axis.z.abs() > 0.99 && (angle.abs() - std::f32::consts::FRAC_PI_2).abs() < 1e-4);

        // abort removes the pending segment and turns the tool off
        tool.abort(&mut scene);
        assert!(scene.objects().is_empty());
        assert!(!tool.active());
    }
}
