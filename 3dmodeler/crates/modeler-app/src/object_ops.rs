//! Object-level operations: delete via X (with Blender's confirm popup) or
//! the Delete key (immediate).

use crate::selection::Selection;
use modeler_core::Scene;
use three_d::egui;
use three_d::{Event, Key, Viewport};

pub struct DeleteTool {
    confirm_open: bool,
    position: egui::Pos2,
    last_mouse: egui::Pos2,
}

/// Convert a three-d event position (physical px, bottom-left origin) to
/// egui logical coordinates (top-left origin).
pub fn event_pos_to_egui(
    x: f32,
    y: f32,
    viewport: Viewport,
    device_pixel_ratio: f32,
) -> egui::Pos2 {
    egui::Pos2::new(
        x / device_pixel_ratio,
        (viewport.height as f32 - y) / device_pixel_ratio,
    )
}

impl DeleteTool {
    pub fn new() -> Self {
        Self {
            confirm_open: false,
            position: egui::Pos2::new(200.0, 200.0),
            last_mouse: egui::Pos2::new(200.0, 200.0),
        }
    }

    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        viewport: Viewport,
        device_pixel_ratio: f32,
        egui_owns_keyboard: bool,
        scene: &mut Scene,
        selection: &mut Selection,
    ) {
        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, .. } => {
                    self.last_mouse =
                        event_pos_to_egui(position.x, position.y, viewport, device_pixel_ratio);
                }
                // Blender: X opens a small "Delete" confirmation at the cursor
                Event::Text(text) if text == "x" && !egui_owns_keyboard => {
                    if !selection.is_empty() {
                        self.confirm_open = true;
                        self.position = self.last_mouse;
                        text.clear();
                    }
                }
                // Delete key removes immediately
                Event::KeyPress {
                    kind: Key::Delete,
                    handled,
                    ..
                } if !*handled => {
                    delete_selected(scene, selection);
                    *handled = true;
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.confirm_open => {
                    self.confirm_open = false;
                    *handled = true;
                }
                Event::MousePress { handled, .. } if self.confirm_open => {
                    // click anywhere else closes the popup; the popup's own
                    // button click is consumed by egui before we see it
                    if !*handled {
                        self.confirm_open = false;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, scene: &mut Scene, selection: &mut Selection) {
        if !self.confirm_open {
            return;
        }
        // the selection can empty out under the popup (e.g. Delete key)
        if selection.is_empty() {
            self.confirm_open = false;
            return;
        }
        egui::Area::new(egui::Id::new("delete-confirm"))
            .fixed_pos(self.position)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(100.0);
                    let count = selection.selected().len();
                    let label = if count == 1 {
                        "Delete?".to_string()
                    } else {
                        format!("Delete {count} objects?")
                    };
                    ui.label(egui::RichText::new(label).strong().size(12.0));
                    ui.separator();
                    if ui.button("Delete").clicked() {
                        delete_selected(scene, selection);
                        self.confirm_open = false;
                    }
                });
            });
    }
}

pub fn delete_selected(scene: &mut Scene, selection: &mut Selection) {
    for id in selection.selected().to_vec() {
        scene.remove_object(id);
    }
    selection.retain_existing(|id| scene.object(id).is_some());
}

/// Place the selection on the ground (End key): each selection root (a
/// selected object whose parent is not selected) is moved vertically so the
/// lowest point of its whole subtree sits at z = 0 — a grouped assembly
/// lands as one piece, keeping its internal offsets.
pub fn place_on_ground(scene: &mut Scene, selection: &Selection) {
    let selected = selection.selected().to_vec();
    let roots: Vec<_> = selected
        .iter()
        .copied()
        .filter(|&id| {
            scene
                .object(id)
                .is_some_and(|o| o.parent.map_or(true, |p| !selected.contains(&p)))
        })
        .collect();
    for root in roots {
        let lowest = scene
            .subtree(root)
            .iter()
            .map(|&member| scene.lowest_point_z(member))
            .fold(f32::INFINITY, f32::min);
        if !lowest.is_finite() {
            continue;
        }
        let mut world = scene.world_transform(root);
        world.location.z -= lowest;
        scene.set_world_transform(root, world);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::glam::Vec3;
    use modeler_core::{Primitive, Transform};

    #[test]
    fn end_places_selection_roots_on_the_ground() {
        let mut scene = Scene::new();
        let mut at = |location: Vec3| {
            let mut t = Transform::default();
            t.location = location;
            scene.add_object(Primitive::Cube { size: 2.0 }, t)
        };
        // a floating "door"-like pair: root high up, child hanging below
        let root = at(Vec3::new(0.0, 0.0, 5.0));
        let child = at(Vec3::new(0.0, 3.0, 2.0));
        // and an independent floating cube
        let loose = at(Vec3::new(4.0, 0.0, -3.0));
        scene.set_parent(child, Some(root));

        let mut selection = Selection::default();
        selection.set(vec![root, child, loose], Some(root));
        place_on_ground(&mut scene, &selection);

        // the pair moved as ONE piece: the child's bottom (the lowest point
        // of the subtree) sits at z = 0 and the 3 m gap is preserved
        assert!((scene.lowest_point_z(child) - 0.0).abs() < 1e-4);
        let root_z = scene.world_transform(root).location.z;
        let child_z = scene.world_transform(child).location.z;
        assert!((root_z - child_z - 3.0).abs() < 1e-4, "{root_z} vs {child_z}");
        // the loose cube (below ground before) came UP to rest at z = 0
        let w = scene.world_transform(loose);
        assert!((w.location.z - 1.0).abs() < 1e-4, "{:?}", w.location);
    }
}
