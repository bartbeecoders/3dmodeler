//! Object-level operations: delete via X (with Blender's confirm popup) or
//! the Delete key (immediate).

use crate::selection::Selection;
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Primitive, Scene, Transform};
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

/// Replace a wall with individual DYNAMIC bricks in a running bond (odd
/// courses start with a half brick), openings respected. The bricks keep the
/// wall's material (with a subtle per-brick shade variation) and density;
/// they collide and can tumble once the simulation plays. Returns the new
/// brick ids — None when the object is not a pristine wall.
pub fn break_wall_into_bricks(scene: &mut Scene, id: ObjectId) -> Option<Vec<ObjectId>> {
    let object = scene.object(id)?;
    let Primitive::Wall { length, height, thickness } = object.primitive else {
        return None;
    };
    if object.edited_mesh.is_some() {
        return None;
    }
    let wall = scene.world_transform(id);
    let material = object.material;
    let density = object.density;
    let base_name = object.name.clone();
    let cutouts = object.cutouts.clone();

    // brick module ≈ 0.42 × 0.21 m, enlarged for big walls to cap the count
    // (physics with thousands of bodies would crawl)
    let mut cell_x = 0.42_f32;
    let mut cell_z = 0.21_f32;
    let estimate = (length / cell_x).max(1.0) * (height / cell_z).max(1.0);
    const MAX_BRICKS: f32 = 600.0;
    if estimate > MAX_BRICKS {
        let grow = (estimate / MAX_BRICKS).sqrt();
        cell_x *= grow;
        cell_z *= grow;
    }
    let rows = ((height / cell_z).round().max(1.0)) as usize;
    let cell_z = height / rows as f32;
    const GAP: f32 = 0.006; // mortar joint, keeps stacked bricks collision-free

    // course layout in the wall's local frame (X along the length, Z up)
    let mut layout: Vec<(f32, f32, f32)> = Vec::new(); // (center x, center z, width)
    for row in 0..rows {
        let z0 = row as f32 * cell_z;
        let z1 = z0 + cell_z;
        // openings overlapping this course block their x-range
        let blocked: Vec<(f32, f32)> = cutouts
            .iter()
            .filter(|c| c.bottom < z1 - 1e-3 && c.bottom + c.height > z0 + 1e-3)
            .map(|c| (c.offset, c.offset + c.width))
            .collect();
        let mut x = 0.0_f32;
        let mut half_first = row % 2 == 1;
        while x < length - 1e-3 {
            let step = if half_first { 0.5 * cell_x } else { cell_x };
            half_first = false;
            let end = (x + step).min(length);
            for (s0, s1) in subtract_ranges(x, end, &blocked) {
                if s1 - s0 < 0.22 * cell_x {
                    continue; // skip slivers at opening edges
                }
                layout.push((0.5 * (s0 + s1), 0.5 * (z0 + z1), s1 - s0));
            }
            x = end;
        }
    }
    if layout.is_empty() {
        return None;
    }

    let mut ids = Vec::with_capacity(layout.len());
    let folder = scene.add_folder(&format!("{base_name} bricks"));
    for (i, (cx, cz, w)) in layout.into_iter().enumerate() {
        let transform = Transform {
            location: wall.transform_point(Vec3::new(cx, 0.0, cz)),
            rotation: wall.rotation,
            scale: wall.scale
                * Vec3::new(
                    (w - GAP).max(0.02),
                    (thickness - GAP).max(0.02),
                    (cell_z - GAP).max(0.02),
                ),
        };
        let brick = scene.add_object(Primitive::Cube { size: 1.0 }, transform);
        if let Some(o) = scene.object_mut(brick) {
            o.name = format!("{base_name} brick {}", i + 1);
            o.folder = Some(folder);
            o.dynamic = true;
            o.density = density;
            o.material = material;
            // subtle deterministic shade variation so the bond reads
            let shade = 0.88 + 0.24 * ((i as f32) * 0.618_034).fract();
            for channel in &mut o.material.base_color {
                *channel = (*channel * shade).clamp(0.0, 1.0);
            }
        }
        ids.push(brick);
    }
    scene.remove_object(id);
    Some(ids)
}

/// The parts of `[x0, x1]` not covered by any blocked range.
fn subtract_ranges(x0: f32, x1: f32, blocked: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut free = vec![(x0, x1)];
    for &(b0, b1) in blocked {
        let mut next = Vec::new();
        for (f0, f1) in free {
            if b1 <= f0 || b0 >= f1 {
                next.push((f0, f1));
                continue;
            }
            if b0 > f0 {
                next.push((f0, b0));
            }
            if b1 < f1 {
                next.push((b1, f1));
            }
        }
        free = next;
    }
    free
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_breaks_into_dynamic_bricks_around_openings() {
        let mut scene = Scene::new();
        let wall = scene.add_object(
            Primitive::Wall { length: 4.0, height: 2.5, thickness: 0.2 },
            Transform::default(),
        );
        // a 1 m door in the middle: 1.5..2.5 × 0..2.1
        scene.object_mut(wall).unwrap().cutouts.push(modeler_core::WallCutout {
            offset: 1.5,
            width: 1.0,
            bottom: 0.0,
            height: 2.1,
        });

        let bricks = break_wall_into_bricks(&mut scene, wall).unwrap();
        assert!(scene.object(wall).is_none(), "the wall is replaced");
        assert!(bricks.len() > 40, "got {} bricks", bricks.len());

        // the bricks land in their own outliner folder
        let folder = scene
            .folders()
            .iter()
            .find(|f| f.name == "Wall bricks")
            .expect("brick folder created");

        for &id in &bricks {
            let o = scene.object(id).unwrap();
            assert!(o.dynamic, "bricks must simulate");
            assert_eq!(o.folder, Some(folder.id), "bricks are filed in the folder");
            // no brick may reach into the door opening (interior overlap)
            let (cx, cz) = (o.transform.location.x, o.transform.location.z);
            let (hw, hh) = (0.5 * o.transform.scale.x, 0.5 * o.transform.scale.z);
            let outside = cx + hw <= 1.5 + 1e-3
                || cx - hw >= 2.5 - 1e-3
                || cz - hh >= 2.1 - 1e-3;
            assert!(outside, "brick at ({cx}, {cz}) w={hw} h={hh} is inside the door");
        }
    }

    #[test]
    fn subtract_ranges_cuts_blocked_spans() {
        assert_eq!(subtract_ranges(0.0, 1.0, &[]), vec![(0.0, 1.0)]);
        assert_eq!(
            subtract_ranges(0.0, 1.0, &[(0.4, 0.6)]),
            vec![(0.0, 0.4), (0.6, 1.0)]
        );
        assert_eq!(subtract_ranges(0.0, 1.0, &[(-1.0, 2.0)]), vec![]);
    }

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
