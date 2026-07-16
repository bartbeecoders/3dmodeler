//! Draggable start/end handles for selected ropes.
//!
//! When a rope is selected (edit mode, simulation stopped), each endpoint
//! shows a disc you can drag in the viewport:
//! - Drag freely in space to place the end.
//! - Drop on another object to **attach** that end (pin lands on the hit
//!   point). Drop on empty space to leave the end free.
//! - While already attached, drag to re-pin on the same object or a new one.
//!
//! Esc cancels an in-progress drag and restores the pre-drag state.

use crate::camera::BlenderCamera;
use crate::physics::PhysicsMirror;
use crate::selection::Selection;
use modeler_core::glam::{Quat, Vec3};
use modeler_core::{ObjectId, Primitive, RopeEnd, Scene, Transform};
use three_d::egui;
use three_d::{Event, Key, MouseButton, Viewport};

const HANDLE_RADIUS: f32 = 7.0;
const PICK_RADIUS: f32 = 14.0;
/// Magnetic snap distance (meters): if the free-plane probe is this close
/// to an object's surface, stick the end there even when the ray misses.
const MAGNET_DIST: f32 = 0.45;
const START_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);
const END_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 160, 255);
const LINE_COLOR: egui::Color32 = egui::Color32::from_rgb(180, 160, 120);
const SNAP_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 220, 80);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Start,
    End,
}

/// Object under the cursor while dragging an end (for attach-on-release).
#[derive(Clone, Copy)]
struct HoverTarget {
    object: ObjectId,
    /// World-space hit on that object's surface.
    world_point: Vec3,
}

struct Drag {
    id: ObjectId,
    which: Which,
    /// Snapshot restored on Esc.
    orig_transform: Transform,
    orig_length: f32,
    orig_start: RopeEnd,
    orig_end: RopeEnd,
    /// Valid attach target under the pointer, if any.
    hover: Option<HoverTarget>,
}

pub struct RopeHandles {
    drag: Option<Drag>,
}

impl RopeHandles {
    pub fn new() -> Self {
        Self { drag: None }
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// Selected visible ropes with their current world-space endpoints.
    fn ropes(scene: &Scene, selection: &Selection) -> Vec<(ObjectId, Vec3, Vec3)> {
        let mut out = Vec::new();
        for object in scene.objects() {
            if !object.visible || !object.primitive.is_rope() {
                continue;
            }
            if !selection.is_selected(object.id) {
                continue;
            }
            let start = scene.rope_end_world(object.id, true);
            let end = scene.rope_end_world(object.id, false);
            out.push((object.id, start, end));
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        scene: &mut Scene,
        selection: &Selection,
        physics: &PhysicsMirror,
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
                    let pick = PICK_RADIUS * device_pixel_ratio;
                    let mut best: Option<(f32, ObjectId, Which)> = None;
                    for (id, start, end) in Self::ropes(scene, selection) {
                        for (which, tip) in [(Which::Start, start), (Which::End, end)] {
                            let Some((sx, sy)) =
                                camera.project(viewport, three_d::vec3(tip.x, tip.y, tip.z))
                            else {
                                continue;
                            };
                            let d = (egui::vec2(sx - position.x, sy - position.y)).length();
                            if d < pick && best.is_none_or(|(bd, _, _)| d < bd) {
                                best = Some((d, id, which));
                            }
                        }
                    }
                    if let Some((_, id, which)) = best {
                        let object = scene.object(id);
                        let (transform, length, start, end) = match object {
                            Some(o) => {
                                let length = match o.primitive {
                                    Primitive::Rope { length, .. } => length,
                                    _ => 1.0,
                                };
                                (o.transform, length, o.rope_start, o.rope_end)
                            }
                            None => continue,
                        };
                        // Detach the dragged end for free placement; re-attach
                        // on release if dropped on an object.
                        if let Some(object) = scene.object_mut(id) {
                            match which {
                                Which::Start => object.rope_start = RopeEnd::default(),
                                Which::End => object.rope_end = RopeEnd::default(),
                            }
                        }
                        self.drag = Some(Drag {
                            id,
                            which,
                            orig_transform: transform,
                            orig_length: length,
                            orig_start: start,
                            orig_end: end,
                            hover: None,
                        });
                        *handled = true;
                    }
                }
                Event::MouseMotion { position, .. } if self.drag.is_some() => {
                    let id = self.drag.as_ref().unwrap().id;
                    let is_start = self.drag.as_ref().unwrap().which == Which::Start;
                    let hover = ray_attach_target(
                        scene,
                        physics,
                        camera,
                        viewport,
                        position.x,
                        position.y,
                        id,
                        is_start,
                    );
                    let world = if let Some(h) = hover {
                        h.world_point
                    } else {
                        let pivot = scene.rope_end_world(id, is_start);
                        match plane_hit(camera, viewport, position.x, position.y, pivot) {
                            Some(p) => p,
                            None => continue,
                        }
                    };
                    // free pose while dragging (attachment only commits on release)
                    place_free_end(scene, id, is_start, world);
                    if let Some(drag) = self.drag.as_mut() {
                        drag.hover = hover;
                    }
                }
                Event::MouseRelease {
                    button: MouseButton::Left,
                    ..
                } if self.drag.is_some() => {
                    let drag = self.drag.take().unwrap();
                    let is_start = drag.which == Which::Start;
                    if let Some(hover) = drag.hover {
                        // pin to the object under the cursor at the hit point
                        let local = scene
                            .world_transform(hover.object)
                            .inverse_transform_point(hover.world_point);
                        if let Some(object) = scene.object_mut(drag.id) {
                            let end = RopeEnd {
                                object: Some(hover.object),
                                local_point: local,
                            };
                            if is_start {
                                object.rope_start = end;
                            } else {
                                object.rope_end = end;
                            }
                        }
                        snap_rope_rest_pose(scene, drag.id);
                    } else {
                        // empty space: leave free at the current free pose
                        if let Some(object) = scene.object_mut(drag.id) {
                            if is_start {
                                object.rope_start = RopeEnd::default();
                            } else {
                                object.rope_end = RopeEnd::default();
                            }
                        }
                    }
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.drag.is_some() => {
                    let drag = self.drag.take().unwrap();
                    if let Some(object) = scene.object_mut(drag.id) {
                        object.transform = drag.orig_transform;
                        if let Primitive::Rope { length, .. } = &mut object.primitive {
                            *length = drag.orig_length;
                        }
                        object.rope_start = drag.orig_start;
                        object.rope_end = drag.orig_end;
                        object.mesh_revision = object.mesh_revision.wrapping_add(1);
                    }
                    *handled = true;
                }
                _ => {}
            }
        }
    }

    /// Draw endpoint discs (and a thin rest-span line) for selected ropes.
    pub fn draw(
        &self,
        ctx: &egui::Context,
        scene: &Scene,
        selection: &Selection,
        camera: &BlenderCamera,
        viewport: Viewport,
        device_pixel_ratio: f32,
        clip: egui::Rect,
    ) {
        let ropes = Self::ropes(scene, selection);
        if ropes.is_empty() && self.drag.is_none() {
            return;
        }
        let painter = ctx
            .layer_painter(egui::LayerId::background())
            .with_clip_rect(clip);
        let pointer = ctx.pointer_hover_pos();
        let project = |p: Vec3| -> Option<egui::Pos2> {
            let (x, y) = camera.project(viewport, three_d::vec3(p.x, p.y, p.z))?;
            Some(egui::Pos2::new(
                x / device_pixel_ratio,
                (viewport.height as f32 - y) / device_pixel_ratio,
            ))
        };

        for (id, start, end) in ropes {
            let (Some(a), Some(b)) = (project(start), project(end)) else {
                continue;
            };
            painter.line_segment([a, b], egui::Stroke::new(1.5, LINE_COLOR));

            for (which, screen, color) in [
                (Which::Start, a, START_COLOR),
                (Which::End, b, END_COLOR),
            ] {
                let hover = pointer.is_some_and(|p| (p - screen).length() < PICK_RADIUS);
                let active = self
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.id == id && d.which == which);
                let snapping = active
                    && self
                        .drag
                        .as_ref()
                        .is_some_and(|d| d.hover.is_some());
                let stroke_color = if snapping { SNAP_COLOR } else { color };
                let fill = if hover || active {
                    stroke_color
                } else {
                    egui::Color32::from_black_alpha(160)
                };
                painter.circle_filled(screen, HANDLE_RADIUS, fill);
                painter.circle_stroke(
                    screen,
                    HANDLE_RADIUS,
                    egui::Stroke::new(if snapping { 2.5 } else { 1.5 }, stroke_color),
                );
                if snapping {
                    painter.circle_stroke(
                        screen,
                        HANDLE_RADIUS + 4.0,
                        egui::Stroke::new(1.5, SNAP_COLOR),
                    );
                }

                let label = match which {
                    Which::Start => "S",
                    Which::End => "E",
                };
                let font = egui::FontId::proportional(10.0);
                painter.text(
                    screen,
                    egui::Align2::CENTER_CENTER,
                    label,
                    font,
                    if hover || active {
                        egui::Color32::BLACK
                    } else {
                        color
                    },
                );
            }

            // tooltip: target name while snapping
            if let Some(drag) = self.drag.as_ref() {
                if drag.id == id {
                    if let Some(hover) = drag.hover {
                        if let Some(name) = scene.object(hover.object).map(|o| o.name.as_str()) {
                            let tip = project(hover.world_point).unwrap_or(a);
                            let text = format!("Attach → {name}");
                            let font = egui::FontId::proportional(12.0);
                            let pos = tip + egui::vec2(12.0, -14.0);
                            let rect = painter.text(
                                pos,
                                egui::Align2::LEFT_BOTTOM,
                                &text,
                                font.clone(),
                                SNAP_COLOR,
                            );
                            painter.rect_filled(
                                rect.expand(3.0),
                                3.0,
                                egui::Color32::from_black_alpha(160),
                            );
                            painter.text(
                                pos,
                                egui::Align2::LEFT_BOTTOM,
                                &text,
                                font,
                                SNAP_COLOR,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Find a valid attach target under the pointer.
///
/// 1. Raycast, **skipping the rope itself** (its capsule used to steal hits
///    and made attaching to a cube almost impossible).
/// 2. If the ray misses, magnetically snap to the nearest object surface
///    within [`MAGNET_DIST`] of the free-plane probe under the cursor.
fn ray_attach_target(
    scene: &Scene,
    physics: &PhysicsMirror,
    camera: &BlenderCamera,
    viewport: Viewport,
    x_px: f32,
    y_px: f32,
    rope_id: ObjectId,
    is_start: bool,
) -> Option<HoverTarget> {
    let (origin, dir) = camera.pick_ray(viewport, x_px, y_px);
    let origin = Vec3::new(origin.x, origin.y, origin.z);
    let dir = Vec3::new(dir.x, dir.y, dir.z);

    // Exclude this rope and any other ropes so we always hit the intended solid.
    let mut exclude: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| o.primitive.is_rope())
        .map(|o| o.id)
        .collect();
    if !exclude.contains(&rope_id) {
        exclude.push(rope_id);
    }

    if let Some((target, world_point)) = physics.pick_surface(origin, dir, &exclude) {
        if valid_attach_target(scene, rope_id, target) {
            return Some(HoverTarget {
                object: target,
                world_point,
            });
        }
    }

    // Magnetic assist: free-plane point under the cursor → nearest surface.
    let pivot = scene.rope_end_world(rope_id, is_start);
    let probe = plane_hit(camera, viewport, x_px, y_px, pivot)?;
    if let Some((target, world_point)) =
        physics.closest_surface_point(probe, &exclude, MAGNET_DIST)
    {
        if valid_attach_target(scene, rope_id, target) {
            return Some(HoverTarget {
                object: target,
                world_point,
            });
        }
    }
    None
}

fn valid_attach_target(scene: &Scene, rope_id: ObjectId, target: ObjectId) -> bool {
    if target == rope_id {
        return false;
    }
    let Some(object) = scene.object(target) else {
        return false;
    };
    if !object.visible {
        return false;
    }
    // lights never simulate; ropes would create pin cycles
    if object.primitive.is_light() || object.primitive.is_rope() {
        return false;
    }
    true
}

/// Move a free endpoint to `world` without changing attachment state.
fn place_free_end(scene: &mut Scene, id: ObjectId, is_start: bool, world: Vec3) {
    let other = scene.rope_end_world(id, !is_start);
    let (start_w, end_w) = if is_start {
        (world, other)
    } else {
        (other, world)
    };
    set_rope_span(scene, id, start_w, end_w);
}

/// Place the rope rest pose along the world span from `start` to `end`
/// (origin at start, local +X toward end). When `update_length` is true the
/// design length becomes `|end - start|` (user is placing free ends). When
/// false the existing length is kept (attach follow / post-sim restore).
pub fn set_rope_span(scene: &mut Scene, id: ObjectId, start: Vec3, end: Vec3) {
    set_rope_span_ex(scene, id, start, end, true);
}

fn set_rope_span_ex(
    scene: &mut Scene,
    id: ObjectId,
    start: Vec3,
    end: Vec3,
    update_length: bool,
) {
    let dir = end - start;
    let span = dir.length().max(0.05);
    let rotation = if dir.length_squared() > 1e-8 {
        Quat::from_rotation_arc(Vec3::X, dir.normalize())
    } else {
        scene.world_transform(id).rotation
    };
    scene.set_world_transform(
        id,
        Transform {
            location: start,
            rotation,
            scale: Vec3::ONE,
        },
    );
    if let Some(object) = scene.object_mut(id) {
        if let Primitive::Rope {
            length: l,
            radius: _,
            segments: _,
        } = &mut object.primitive
        {
            if update_length {
                *l = span;
            }
        }
        object.mesh_revision = object.mesh_revision.wrapping_add(1);
    }
}

/// Align the rest-pose mesh with the current attachment / free endpoints.
/// Call after changing `rope_start` / `rope_end` so the cord jumps to the
/// pins in design mode (not only when simulation plays).
///
/// **Preserves design length** so attaching a 2 m rope between closer pins
/// keeps 2 m (it will sag under gravity) instead of shrinking to the span.
pub fn snap_rope_rest_pose(scene: &mut Scene, id: ObjectId) {
    let start = scene.rope_end_world(id, true);
    let end = scene.rope_end_world(id, false);
    set_rope_span_ex(scene, id, start, end, false);
}

/// Keep attached rope ends on their targets while editing. Free ends stay
/// put; only ropes with at least one anchor are considered. **Does not
/// change design length** — a rope longer than its pin span stays long
/// (sags under gravity). No-op when the rest pose already matches.
pub fn sync_attached_ropes(scene: &mut Scene) {
    let ids: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| {
            o.visible
                && o.primitive.is_rope()
                && (o.rope_start.object.is_some() || o.rope_end.object.is_some())
        })
        .map(|o| o.id)
        .collect();
    for id in ids {
        let Some(object) = scene.object(id) else {
            continue;
        };
        let length = match object.primitive {
            Primitive::Rope { length, .. } => length.max(0.05),
            _ => continue,
        };
        let world = scene.world_transform(id);
        let rest_start = world.location;
        // only re-home the start pin; free tip is length along +X
        let want_start = scene.rope_end_world(id, true);
        let want_end = scene.rope_end_world(id, false);
        // Direction for orientation: prefer attach span; if ends coincide, keep.
        let need_move = (rest_start - want_start).length_squared() > 1e-6;
        let need_aim = {
            let rest_dir = world.rotation * Vec3::X;
            let want_dir = (want_end - want_start).normalize_or_zero();
            want_dir.length_squared() > 1e-8
                && rest_dir.dot(want_dir) < 0.999
        };
        if need_move || need_aim {
            let _ = length;
            set_rope_span_ex(scene, id, want_start, want_end, false);
        }
    }
}

/// Intersect the camera pick ray with the plane through `point` that faces
/// the camera. Returns the world hit (glam).
fn plane_hit(
    camera: &BlenderCamera,
    viewport: Viewport,
    x_px: f32,
    y_px: f32,
    point: Vec3,
) -> Option<Vec3> {
    let (origin, dir) = camera.pick_ray(viewport, x_px, y_px);
    let origin = Vec3::new(origin.x, origin.y, origin.z);
    let dir = Vec3::new(dir.x, dir.y, dir.z);
    let (_, _, forward) = camera.screen_basis();
    let n = Vec3::new(forward.x, forward.y, forward.z);
    let denom = dir.dot(n);
    if denom.abs() < 1e-8 {
        return None;
    }
    let t = (point - origin).dot(n) / denom;
    if t < 0.0 {
        return None;
    }
    Some(origin + dir * t)
}
