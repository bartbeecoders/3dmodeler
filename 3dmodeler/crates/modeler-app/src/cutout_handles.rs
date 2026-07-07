//! Draggable handles on wall openings.
//!
//! Every door / window of a selected wall shows a round grab handle at the
//! opening's center. Dragging it slides the opening along the wall — doors
//! move horizontally (the sill stays on the floor), windows also move
//! vertically. While dragging, the center / sill position is shown next to
//! the handle; Esc cancels and restores the original position.
//!
//! Event flow mirrors the other viewport tools: `handle_events` (main.rs,
//! after the pie wheels, before click-selection) consumes the press so a
//! grab never changes the selection; drawing happens from the egui pass.
//! The scene edits bump the object's `mesh_revision`, so the wall mesh and
//! physics rebuild live, and the undo watcher batches the whole drag into
//! one step (see `dragging()` in main.rs's `undo.on_frame` call).

use crate::camera::BlenderCamera;
use crate::selection::Selection;
use crate::settings::Unit;
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Primitive, Scene};
use three_d::egui;
use three_d::{Event, Key, MouseButton, Viewport};

/// Handle disc radius / pick radius, logical points.
const HANDLE_RADIUS: f32 = 8.0;
const PICK_RADIUS: f32 = 13.0;
/// Windows keep at least this sill height so they never silently turn into
/// doors (`WallCutout::is_door` keys on `bottom == 0`).
const WINDOW_MIN_SILL: f32 = 0.05;

struct Drag {
    id: ObjectId,
    index: usize,
    /// Wall-local (x, z) offset between the grab point and opening center.
    grab: (f32, f32),
    /// Original (offset, bottom), restored on Esc.
    orig: (f32, f32),
    /// Captured at grab time — windows move vertically, doors do not.
    window: bool,
}

pub struct CutoutHandles {
    drag: Option<Drag>,
}

impl CutoutHandles {
    pub fn new() -> Self {
        Self { drag: None }
    }

    /// True while an opening is being dragged (suppresses undo checkpoints).
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// One entry per opening of every selected, unedited wall:
    /// (object, cutout index, world-space center, is_window).
    fn handles(scene: &Scene, selection: &Selection) -> Vec<(ObjectId, usize, Vec3, bool)> {
        let mut out = Vec::new();
        for object in scene.objects() {
            if !selection.is_selected(object.id) || !object.visible {
                continue;
            }
            if !matches!(object.primitive, Primitive::Wall { .. }) || object.edited_mesh.is_some()
            {
                continue;
            }
            let world = scene.world_transform(object.id);
            for (i, cut) in object.cutouts.iter().enumerate() {
                let local = Vec3::new(
                    cut.offset + 0.5 * cut.width,
                    0.0,
                    cut.bottom + 0.5 * cut.height,
                );
                out.push((object.id, i, world.transform_point(local), !cut.is_door()));
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        scene: &mut Scene,
        selection: &Selection,
        camera: &BlenderCamera,
        viewport: Viewport,
        device_pixel_ratio: f32,
        pointer_over_ui: bool,
    ) {
        for event in events.iter_mut() {
            match event {
                Event::MousePress {
                    button: MouseButton::Left,
                    position,
                    handled,
                    ..
                } if !*handled && !pointer_over_ui && self.drag.is_none() => {
                    // pick the closest handle under the cursor (physical px,
                    // bottom-left origin — the same space as world_to_screen)
                    let pick = PICK_RADIUS * device_pixel_ratio;
                    let mut best: Option<(f32, ObjectId, usize)> = None;
                    for (id, i, world_pos, _) in Self::handles(scene, selection) {
                        let Some((sx, sy)) = camera.project(
                            viewport,
                            three_d::vec3(world_pos.x, world_pos.y, world_pos.z),
                        ) else {
                            continue;
                        };
                        let d = (egui::vec2(sx - position.x, sy - position.y)).length();
                        if d < pick && best.is_none_or(|(bd, ..)| d < bd) {
                            best = Some((d, id, i));
                        }
                    }
                    if let Some((_, id, index)) = best {
                        if let Some((lx, lz)) =
                            local_hit(scene, id, camera, viewport, position.x, position.y)
                        {
                            let Some(object) = scene.object(id) else { continue };
                            let Some(cut) = object.cutouts.get(index).copied() else {
                                continue;
                            };
                            self.drag = Some(Drag {
                                id,
                                index,
                                grab: (
                                    lx - (cut.offset + 0.5 * cut.width),
                                    lz - (cut.bottom + 0.5 * cut.height),
                                ),
                                orig: (cut.offset, cut.bottom),
                                window: !cut.is_door(),
                            });
                            *handled = true; // the grab must not re-select
                        }
                    }
                }
                Event::MouseMotion { position, .. } if self.drag.is_some() => {
                    let drag = self.drag.as_ref().unwrap();
                    if let Some((lx, lz)) =
                        local_hit(scene, drag.id, camera, viewport, position.x, position.y)
                    {
                        apply_drag(scene, drag, lx, lz);
                    }
                }
                Event::MouseRelease {
                    button: MouseButton::Left,
                    ..
                } if self.drag.is_some() => {
                    self.drag = None;
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.drag.is_some() => {
                    let drag = self.drag.take().unwrap();
                    if let Some(object) = scene.object_mut(drag.id) {
                        if let Some(cut) = object.cutouts.get_mut(drag.index) {
                            cut.offset = drag.orig.0;
                            cut.bottom = drag.orig.1;
                            object.mesh_revision += 1;
                        }
                    }
                    *handled = true;
                }
                _ => {}
            }
        }
    }

    /// Viewport handles: a disc with move arrows per opening; the value
    /// readout while dragging.
    pub fn draw(
        &self,
        ctx: &egui::Context,
        scene: &Scene,
        selection: &Selection,
        camera: &BlenderCamera,
        viewport: Viewport,
        device_pixel_ratio: f32,
        unit: Unit,
    ) {
        let handles = Self::handles(scene, selection);
        if handles.is_empty() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::background());
        let visuals = ctx.global_style().visuals.clone();
        let accent = visuals.hyperlink_color;
        let pointer = ctx.pointer_hover_pos();

        for (id, i, world_pos, is_window) in handles {
            let Some((sx, sy)) =
                camera.project(viewport, three_d::vec3(world_pos.x, world_pos.y, world_pos.z))
            else {
                continue;
            };
            let pos = egui::pos2(
                sx / device_pixel_ratio,
                (viewport.height as f32 - sy) / device_pixel_ratio,
            );
            let active = self
                .drag
                .as_ref()
                .is_some_and(|d| d.id == id && d.index == i);
            let hot =
                active || (self.drag.is_none() && pointer.is_some_and(|p| p.distance(pos) < PICK_RADIUS));
            let r = if hot { HANDLE_RADIUS + 2.0 } else { HANDLE_RADIUS };

            painter.circle_filled(
                pos + egui::vec2(1.0, 1.5),
                r,
                egui::Color32::from_black_alpha(80),
            );
            let (fill, ring, arrows) = if hot {
                (accent, visuals.window_fill, visuals.window_fill)
            } else {
                (visuals.window_fill, accent, visuals.text_color())
            };
            painter.circle_filled(pos, r, fill);
            painter.circle_stroke(pos, r, egui::Stroke::new(1.5, ring));
            draw_arrows(&painter, pos, r, arrows, is_window);

            // value readout while dragging this handle
            if active {
                if let Some(object) = scene.object(id) {
                    if let Some(cut) = object.cutouts.get(i) {
                        let text = if is_window {
                            format!(
                                "{} · sill {}",
                                unit.format(cut.offset + 0.5 * cut.width),
                                unit.format(cut.bottom),
                            )
                        } else {
                            unit.format(cut.offset + 0.5 * cut.width)
                        };
                        let galley = painter.layout_no_wrap(
                            text,
                            egui::FontId::proportional(12.0),
                            visuals.text_color(),
                        );
                        let text_pos = pos + egui::vec2(r + 8.0, -0.5 * galley.size().y);
                        let bg = egui::Rect::from_min_size(text_pos, galley.size()).expand(4.0);
                        painter.rect_filled(bg, 4.0, visuals.window_fill.gamma_multiply(0.92));
                        painter.galley(text_pos, galley, visuals.text_color());
                    }
                }
            }
        }
    }
}

/// Intersect the pick ray with the wall's mid-thickness plane (local y = 0);
/// returns the wall-local (x, z) hit.
fn local_hit(
    scene: &Scene,
    id: ObjectId,
    camera: &BlenderCamera,
    viewport: Viewport,
    x_px: f32,
    y_px: f32,
) -> Option<(f32, f32)> {
    let world = scene.world_transform(id);
    let (origin, dir) = camera.pick_ray(viewport, x_px, y_px);
    let origin = Vec3::new(origin.x, origin.y, origin.z);
    let dir = Vec3::new(dir.x, dir.y, dir.z);
    let lo = world.inverse_transform_point(origin);
    let ld = world.inverse_transform_point(origin + dir) - lo;
    if ld.y.abs() < 1e-6 {
        return None; // wall seen exactly edge-on
    }
    let t = -lo.y / ld.y;
    let p = lo + ld * t;
    Some((p.x, p.z))
}

/// Clamp the dragged center into the wall and write it back.
fn apply_drag(scene: &mut Scene, drag: &Drag, lx: f32, lz: f32) {
    let Some(object) = scene.object(drag.id) else { return };
    let Primitive::Wall { length, height, .. } = object.primitive else {
        return;
    };
    let Some(cut) = object.cutouts.get(drag.index).copied() else { return };

    let cx = lx - drag.grab.0;
    let offset = (cx - 0.5 * cut.width).clamp(0.0, (length - cut.width).max(0.0));
    let bottom = if drag.window {
        let cz = lz - drag.grab.1;
        (cz - 0.5 * cut.height)
            .clamp(WINDOW_MIN_SILL, (height - cut.height).max(WINDOW_MIN_SILL))
    } else {
        0.0
    };

    if (offset - cut.offset).abs() > 1e-5 || (bottom - cut.bottom).abs() > 1e-5 {
        if let Some(object) = scene.object_mut(drag.id) {
            if let Some(cut) = object.cutouts.get_mut(drag.index) {
                cut.offset = offset;
                cut.bottom = bottom;
            }
            object.mesh_revision += 1; // rebuild mesh & physics caches
        }
    }
}

/// Move arrows inside the handle: ↔ for doors, plus ↕ for windows.
fn draw_arrows(
    painter: &egui::Painter,
    pos: egui::Pos2,
    r: f32,
    color: egui::Color32,
    vertical: bool,
) {
    let s = 0.62 * r;
    let head = 0.32 * r;
    let stroke = egui::Stroke::new(1.4, color);
    painter.line_segment([pos + egui::vec2(-s, 0.0), pos + egui::vec2(s, 0.0)], stroke);
    for sign in [-1.0f32, 1.0] {
        let tip = pos + egui::vec2(sign * s, 0.0);
        painter.line_segment([tip, tip + egui::vec2(-sign * head, -head)], stroke);
        painter.line_segment([tip, tip + egui::vec2(-sign * head, head)], stroke);
    }
    if vertical {
        painter.line_segment([pos + egui::vec2(0.0, -s), pos + egui::vec2(0.0, s)], stroke);
        for sign in [-1.0f32, 1.0] {
            let tip = pos + egui::vec2(0.0, sign * s);
            painter.line_segment([tip, tip + egui::vec2(-head, -sign * head)], stroke);
            painter.line_segment([tip, tip + egui::vec2(head, -sign * head)], stroke);
        }
    }
}
