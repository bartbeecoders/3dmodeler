//! Roof drawing tool (Add ▸ Roof with no wall selected): click on the floor
//! to place the first corner of the roof's footprint, move the mouse to
//! rubber-band the rectangle (the real roof object updates live), click
//! again to plant the opposite corner — that finishes the tool. Enter also
//! confirms, Esc/RMB cancels. Corners honor the grid-snap toggle (Ctrl
//! inverts, like the modal operators). The roof kind comes from the Add
//! menu entry that armed the tool; the sidebar can change it afterwards.

use crate::selection::Selection;
use crate::wall_tool::ground_point;
use modeler_core::glam::{Vec2, Vec3};
use modeler_core::{ObjectId, Primitive, RoofKind, Scene, Transform};
use three_d::{Event, Key, MouseButton, Viewport};

const MIN_SIDE: f32 = 0.1; // meters; smaller rectangles cancel the roof

pub struct RoofTool {
    active: bool,
    kind: RoofKind,
    /// The rectangle being rubber-banded: object id + fixed corner (z=0).
    pending: Option<(ObjectId, Vec3)>,
    last_mouse: (f32, f32), // physical px, bottom-left origin
    ctrl_down: bool,
    /// Current footprint, for the status line.
    current: Vec2,
}

impl RoofTool {
    pub fn new() -> Self {
        Self {
            active: false,
            kind: RoofKind::Gable,
            pending: None,
            last_mouse: (0.0, 0.0),
            ctrl_down: false,
            current: Vec2::ZERO,
        }
    }

    /// Arm the tool for the given roof kind.
    pub fn start(&mut self, kind: RoofKind) {
        self.active = true;
        self.kind = kind;
        self.pending = None;
        self.current = Vec2::ZERO;
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// A rectangle is being rubber-banded (used to batch undo checkpoints).
    pub fn drawing(&self) -> bool {
        self.pending.is_some()
    }

    /// Cancel everything (e.g. the simulation started): the pending roof is
    /// removed and the tool turns off.
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
            None => format!(
                "Roof ({}): click the floor to place the first corner   |   \
                 RMB/Esc cancel",
                self.kind.label()
            ),
            Some(_) => format!(
                "Roof ({}): {} × {} · click to plant the opposite corner   |   \
                 Enter confirm · RMB/Esc cancel",
                self.kind.label(),
                unit.format(self.current.x),
                unit.format(self.current.y)
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
                    None => self.begin_rect(scene, selection, point),
                    Some((id, anchor)) => {
                        // second click: commit (too-small rectangles cancel)
                        self.update_rect(scene, id, anchor, point);
                        if self.current.min_element() < MIN_SIDE {
                            scene.remove_object(id);
                        }
                        self.active = false;
                    }
                }
            }
        }

        // rubber-band the pending rectangle to the mouse every frame
        if let Some((id, anchor)) = self.pending {
            if let Some(point) = ground_point(
                camera,
                viewport,
                self.last_mouse.0,
                self.last_mouse.1,
                snap,
                grid_spacing,
            ) {
                self.update_rect(scene, id, anchor, point);
            }
        }

        if finish_keep {
            if let Some((id, _)) = self.pending.take() {
                if self.current.min_element() < MIN_SIDE {
                    scene.remove_object(id);
                }
            }
            self.active = false;
        }
    }

    fn begin_rect(&mut self, scene: &mut Scene, selection: &mut Selection, anchor: Vec3) {
        let id = scene.add_object(
            Primitive::Roof {
                kind: self.kind,
                width: MIN_SIDE,
                depth: MIN_SIDE,
                height: self.kind.default_height(MIN_SIDE),
                overhang: 0.0,
                ridge_x: true,
            },
            Transform { location: anchor, ..Default::default() },
        );
        selection.set(vec![id], Some(id));
        self.pending = Some((id, anchor));
        self.current = Vec2::ZERO;
    }

    /// Fit the roof to the rectangle between `anchor` and `corner` (both on
    /// the floor). The height follows the footprint while drawing; the kind
    /// is kept — the sidebar can change it mid-draw.
    fn update_rect(&mut self, scene: &mut Scene, id: ObjectId, anchor: Vec3, corner: Vec3) {
        let size = (corner - anchor).truncate().abs();
        self.current = size;
        let width = size.x.max(MIN_SIDE);
        let depth = size.y.max(MIN_SIDE);
        let center = 0.5 * (anchor + corner);
        let Some(object) = scene.object(id) else { return };
        let Primitive::Roof { kind, overhang, .. } = object.primitive else { return };
        let wanted = Primitive::Roof {
            kind,
            width,
            depth,
            height: kind.default_height(width.min(depth)),
            overhang,
            ridge_x: width >= depth,
        };
        let location = Vec3::new(center.x, center.y, 0.0);
        if object.primitive == wanted && object.transform.location.abs_diff_eq(location, 1e-6)
        {
            return; // avoid version bumps (and physics rebuilds) on idle frames
        }
        if let Some(object) = scene.object_mut(id) {
            object.primitive = wanted;
            object.transform.location = location;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangle_rubber_bands_and_cancel_removes_the_pending_roof() {
        let mut scene = Scene::new();
        let mut selection = Selection::default();
        let mut tool = RoofTool::new();
        tool.start(RoofKind::Hip);
        assert!(tool.active() && !tool.drawing());

        // first click: a pending roof appears at the anchor
        tool.begin_rect(&mut scene, &mut selection, Vec3::new(1.0, 2.0, 0.0));
        assert_eq!(scene.objects().len(), 1);
        let (id, anchor) = tool.pending.unwrap();
        assert_eq!(selection.active(), Some(id));

        // rubber-band to the opposite corner: footprint and center follow
        tool.update_rect(&mut scene, id, anchor, Vec3::new(5.0, -1.0, 0.0));
        let object = scene.object(id).unwrap();
        let Primitive::Roof { kind, width, depth, height, ridge_x, .. } = object.primitive
        else {
            panic!("not a roof")
        };
        assert_eq!(kind, RoofKind::Hip);
        assert!((width - 4.0).abs() < 1e-5);
        assert!((depth - 3.0).abs() < 1e-5);
        assert!(height > 0.0 && ridge_x, "ridge follows the longer side");
        let loc = object.transform.location;
        assert!((loc - Vec3::new(3.0, 0.5, 0.0)).length() < 1e-5, "{loc:?}");

        // abort removes the pending roof and turns the tool off
        tool.abort(&mut scene);
        assert!(scene.objects().is_empty());
        assert!(!tool.active());
    }
}
