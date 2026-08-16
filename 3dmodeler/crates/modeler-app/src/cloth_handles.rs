//! Draggable multi-anchor handles for selected cloths.
//!
//! When a cloth is selected (edit mode, simulation stopped), each entry in
//! `Object::cloth_anchors` shows a disc you can drag:
//! - Drop on another object to **attach** that pin (lands on the hit point).
//! - Drop on empty space to leave the pin free at the rest-pose vertex.
//! - **Alt+click** the cloth to add a new free anchor at the nearest grid vertex.
//! - Esc cancels an in-progress drag.

use crate::camera::BlenderCamera;
use crate::gfx::egui;
use crate::gfx::{Event, Key, MouseButton, Viewport};
use crate::physics::PhysicsMirror;
use crate::selection::Selection;
use modeler_core::glam::Vec3;
use modeler_core::{ClothAnchor, ObjectId, Primitive, Scene, Transform};

const HANDLE_RADIUS: f32 = 7.0;
const PICK_RADIUS: f32 = 14.0;
const MAGNET_DIST: f32 = 0.45;
const FREE_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);
const PINNED_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 160, 255);
const LINE_COLOR: egui::Color32 = egui::Color32::from_rgb(160, 180, 200);
const SNAP_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 220, 80);

#[derive(Clone, Copy)]
struct HoverTarget {
    object: ObjectId,
    world_point: Vec3,
}

struct Drag {
    cloth_id: ObjectId,
    anchor_index: usize,
    orig_anchor: ClothAnchor,
    hover: Option<HoverTarget>,
}

pub struct ClothHandles {
    drag: Option<Drag>,
}

impl ClothHandles {
    pub fn new() -> Self {
        Self { drag: None }
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// Selected visible cloths with (id, anchor_index, world handle pos, rest vertex).
    fn anchors(scene: &Scene, selection: &Selection) -> Vec<(ObjectId, usize, Vec3, Vec3, bool)> {
        let mut out = Vec::new();
        for object in scene.objects() {
            if !object.visible || !object.primitive.is_cloth() {
                continue;
            }
            if !selection.is_selected(object.id) {
                continue;
            }
            for (i, a) in object.cloth_anchors.iter().enumerate() {
                let handle = scene.cloth_anchor_world(object.id, i);
                let rest = scene.cloth_vertex_world(object.id, a.u, a.v);
                let pinned = a.object.is_some();
                out.push((object.id, i, handle, rest, pinned));
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
                    modifiers,
                    ..
                } if !*handled && !pointer_over_ui && self.drag.is_none() => {
                    // Alt+click: add anchor at nearest grid vertex of a selected cloth
                    if modifiers.alt {
                        if let Some(id) = selection.active().filter(|&id| {
                            scene.object(id).is_some_and(|o| o.primitive.is_cloth())
                        }) {
                            if let Some((u, v)) =
                                nearest_grid_uv(scene, physics, camera, viewport, position.x, position.y, id)
                            {
                                if add_anchor_if_missing(scene, id, u, v) {
                                    *handled = true;
                                    continue;
                                }
                            }
                        }
                    }

                    let pick = PICK_RADIUS * device_pixel_ratio;
                    let mut best: Option<(f32, ObjectId, usize)> = None;
                    for (id, idx, handle, _, _) in Self::anchors(scene, selection) {
                        let Some((sx, sy)) =
                            camera.project(viewport, handle)
                        else {
                            continue;
                        };
                        let d = (egui::vec2(sx - position.x, sy - position.y)).length();
                        if d < pick && best.is_none_or(|(bd, _, _)| d < bd) {
                            best = Some((d, id, idx));
                        }
                    }
                    if let Some((_, id, idx)) = best {
                        let orig = scene
                            .object(id)
                            .and_then(|o| o.cloth_anchors.get(idx).copied());
                        let Some(orig) = orig else {
                            continue;
                        };
                        // detach while dragging; re-attach on release if snapped
                        if let Some(object) = scene.object_mut(id) {
                            if let Some(a) = object.cloth_anchors.get_mut(idx) {
                                a.object = None;
                                a.local_point = Vec3::ZERO;
                            }
                        }
                        self.drag = Some(Drag {
                            cloth_id: id,
                            anchor_index: idx,
                            orig_anchor: orig,
                            hover: None,
                        });
                        *handled = true;
                    }
                }
                Event::MouseMotion { position, .. } if self.drag.is_some() => {
                    let cloth_id = self.drag.as_ref().unwrap().cloth_id;
                    let hover = ray_attach_target(
                        scene,
                        physics,
                        camera,
                        viewport,
                        position.x,
                        position.y,
                        cloth_id,
                    );
                    if let Some(drag) = self.drag.as_mut() {
                        drag.hover = hover;
                    }
                }
                Event::MouseRelease {
                    button: MouseButton::Left,
                    ..
                } if self.drag.is_some() => {
                    let drag = self.drag.take().unwrap();
                    if let Some(hover) = drag.hover {
                        let local = scene
                            .world_transform(hover.object)
                            .inverse_transform_point(hover.world_point);
                        if let Some(object) = scene.object_mut(drag.cloth_id) {
                            if let Some(a) = object.cloth_anchors.get_mut(drag.anchor_index) {
                                a.object = Some(hover.object);
                                a.local_point = local;
                            }
                        }
                    } else {
                        // free at rest vertex
                        if let Some(object) = scene.object_mut(drag.cloth_id) {
                            if let Some(a) = object.cloth_anchors.get_mut(drag.anchor_index) {
                                a.object = None;
                                a.local_point = Vec3::ZERO;
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
                    if let Some(object) = scene.object_mut(drag.cloth_id) {
                        if let Some(a) = object.cloth_anchors.get_mut(drag.anchor_index) {
                            *a = drag.orig_anchor;
                        }
                    }
                    *handled = true;
                }
                Event::KeyPress {
                    kind: Key::Delete | Key::Backspace,
                    handled,
                    ..
                } if !*handled && self.drag.is_none() && !pointer_over_ui => {
                    // Delete removes the closest hovered anchor on a selected cloth
                    // (only when a single handle is under the pointer — otherwise
                    // the normal delete tool owns the key).
                    // Skip: let object delete work; anchors are removed via UI.
                    let _ = handled;
                }
                _ => {}
            }
        }
    }

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
        let anchors = Self::anchors(scene, selection);
        if anchors.is_empty() && self.drag.is_none() {
            return;
        }
        let painter = ctx
            .layer_painter(egui::LayerId::background())
            .with_clip_rect(clip);
        let pointer = ctx.pointer_hover_pos();
        let project = |p: Vec3| -> Option<egui::Pos2> {
            let (x, y) = camera.project(viewport, p)?;
            Some(egui::Pos2::new(
                x / device_pixel_ratio,
                (viewport.height as f32 - y) / device_pixel_ratio,
            ))
        };

        for (id, idx, handle, rest, pinned) in anchors {
            let (Some(h), Some(r)) = (project(handle), project(rest)) else {
                continue;
            };
            // line from rest vertex to pin when attached (or while dragging)
            let active = self
                .drag
                .as_ref()
                .is_some_and(|d| d.cloth_id == id && d.anchor_index == idx);
            let show_line = pinned || active;
            if show_line && (h - r).length() > 2.0 {
                painter.line_segment([r, h], egui::Stroke::new(1.2, LINE_COLOR));
            }

            let color = if pinned { PINNED_COLOR } else { FREE_COLOR };
            let hover = pointer.is_some_and(|p| (p - h).length() < PICK_RADIUS);
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
            painter.circle_filled(h, HANDLE_RADIUS, fill);
            painter.circle_stroke(
                h,
                HANDLE_RADIUS,
                egui::Stroke::new(if snapping { 2.5 } else { 1.5 }, stroke_color),
            );
            if snapping {
                painter.circle_stroke(
                    h,
                    HANDLE_RADIUS + 4.0,
                    egui::Stroke::new(1.5, SNAP_COLOR),
                );
            }
            let label = {
                let a = scene
                    .object(id)
                    .and_then(|o| o.cloth_anchors.get(idx).copied());
                let (su, sv) = scene.object(id).and_then(|o| match o.primitive {
                    Primitive::Cloth {
                        segments_u,
                        segments_v,
                        ..
                    } => Some((segments_u.clamp(1, 24), segments_v.clamp(1, 24))),
                    _ => None,
                }).unwrap_or((8, 8));
                if let Some(a) = a {
                    match (a.u == 0, a.u == su, a.v == 0, a.v == sv) {
                        (true, _, true, _) => "BL".into(),
                        (_, true, true, _) => "BR".into(),
                        (true, _, _, true) => "TL".into(),
                        (_, true, _, true) => "TR".into(),
                        _ => format!("{}", idx + 1),
                    }
                } else {
                    format!("{}", idx + 1)
                }
            };
            let font = egui::FontId::proportional(9.0);
            painter.text(
                h,
                egui::Align2::CENTER_CENTER,
                &label,
                font,
                if hover || active {
                    egui::Color32::BLACK
                } else {
                    color
                },
            );

            if active {
                if let Some(drag) = self.drag.as_ref() {
                    if let Some(hover_t) = drag.hover {
                        if let Some(name) = scene.object(hover_t.object).map(|o| o.name.as_str()) {
                            let tip = project(hover_t.world_point).unwrap_or(h);
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

fn add_anchor_if_missing(scene: &mut Scene, cloth_id: ObjectId, u: u32, v: u32) -> bool {
    let Some(object) = scene.object_mut(cloth_id) else {
        return false;
    };
    if !object.primitive.is_cloth() {
        return false;
    }
    if object
        .cloth_anchors
        .iter()
        .any(|a| a.u == u && a.v == v)
    {
        return false;
    }
    object.cloth_anchors.push(ClothAnchor::free(u, v));
    true
}

/// Nearest grid (u,v) under the pointer on a cloth (raycast + rest-plane).
fn nearest_grid_uv(
    scene: &Scene,
    physics: &PhysicsMirror,
    camera: &BlenderCamera,
    viewport: Viewport,
    x_px: f32,
    y_px: f32,
    cloth_id: ObjectId,
) -> Option<(u32, u32)> {
    let object = scene.object(cloth_id)?;
    let Primitive::Cloth {
        width,
        height,
        segments_u,
        segments_v,
        ..
    } = object.primitive
    else {
        return None;
    };
    let su = segments_u.clamp(1, 24);
    let sv = segments_v.clamp(1, 24);
    let width = width.max(0.05);
    let height = height.max(0.05);

    let (origin, dir) = camera.pick_ray(viewport, x_px, y_px);
    let origin = Vec3::new(origin.x, origin.y, origin.z);
    let dir = Vec3::new(dir.x, dir.y, dir.z);

    // Prefer a hit on this cloth's surface
    let world_hit = physics
        .pick_surface(origin, dir, &[])
        .filter(|(id, _)| *id == cloth_id)
        .map(|(_, p)| p)
        .or_else(|| {
            // intersect rest plane (local XY → world)
            let world = scene.world_transform(cloth_id);
            let plane_n = (world.rotation * Vec3::Z).normalize_or_zero();
            let plane_p = world.location;
            let denom = plane_n.dot(dir);
            if denom.abs() < 1e-6 {
                return None;
            }
            let t = plane_n.dot(plane_p - origin) / denom;
            if t < 0.0 {
                return None;
            }
            Some(origin + dir * t)
        })?;

    let local = scene
        .world_transform(cloth_id)
        .inverse_transform_point(world_hit);
    // map local XY to continuous uv
    let fu = ((local.x / width) + 0.5).clamp(0.0, 1.0) * su as f32;
    let fv = ((local.y / height) + 0.5).clamp(0.0, 1.0) * sv as f32;
    let u = fu.round() as u32;
    let v = fv.round() as u32;
    Some((u.min(su), v.min(sv)))
}

fn ray_attach_target(
    scene: &Scene,
    physics: &PhysicsMirror,
    camera: &BlenderCamera,
    viewport: Viewport,
    x_px: f32,
    y_px: f32,
    cloth_id: ObjectId,
) -> Option<HoverTarget> {
    let (origin, dir) = camera.pick_ray(viewport, x_px, y_px);
    let origin = Vec3::new(origin.x, origin.y, origin.z);
    let dir = Vec3::new(dir.x, dir.y, dir.z);

    let mut exclude: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| o.primitive.is_soft_sim())
        .map(|o| o.id)
        .collect();
    if !exclude.contains(&cloth_id) {
        exclude.push(cloth_id);
    }

    if let Some((target, world_point)) = physics.pick_surface(origin, dir, &exclude) {
        if valid_attach_target(scene, cloth_id, target) {
            return Some(HoverTarget {
                object: target,
                world_point,
            });
        }
    }

    // magnetic assist
    let pivot = scene.world_transform(cloth_id).location;
    let probe = plane_hit(camera, viewport, x_px, y_px, pivot)?;
    if let Some((target, world_point)) =
        physics.closest_surface_point(probe, &exclude, MAGNET_DIST)
    {
        if valid_attach_target(scene, cloth_id, target) {
            return Some(HoverTarget {
                object: target,
                world_point,
            });
        }
    }
    None
}

fn valid_attach_target(scene: &Scene, cloth_id: ObjectId, target: ObjectId) -> bool {
    if target == cloth_id {
        return false;
    }
    let Some(object) = scene.object(target) else {
        return false;
    };
    if !object.visible {
        return false;
    }
    if object.primitive.is_gizmo() || object.primitive.is_soft_sim() {
        return false;
    }
    true
}

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
    let cam_forward = dir;
    let denom = cam_forward.dot(dir);
    if denom.abs() < 1e-8 {
        return None;
    }
    let t = cam_forward.dot(point - origin) / denom;
    if !t.is_finite() {
        return None;
    }
    Some(origin + dir * t)
}

/// Translate the cloth so the centroid of pinned rest vertices matches the
/// centroid of pin targets — design-mode preview without full soft sim.
pub fn align_cloth_to_pins(scene: &mut Scene, cloth_id: ObjectId) {
    let Some(object) = scene.object(cloth_id) else {
        return;
    };
    if !object.primitive.is_cloth() {
        return;
    }
    let pairs: Vec<(Vec3, Vec3)> = object
        .cloth_anchors
        .iter()
        .enumerate()
        .filter(|(_, a)| a.object.is_some())
        .map(|(i, a)| {
            let rest = scene.cloth_vertex_world(cloth_id, a.u, a.v);
            let pin = scene.cloth_anchor_world(cloth_id, i);
            (rest, pin)
        })
        .collect();
    if pairs.is_empty() {
        return;
    }
    let n = pairs.len() as f32;
    let rest_c = pairs.iter().map(|(r, _)| *r).sum::<Vec3>() / n;
    let pin_c = pairs.iter().map(|(_, p)| *p).sum::<Vec3>() / n;
    let delta = pin_c - rest_c;
    if delta.length_squared() < 1e-10 {
        return;
    }
    let world = scene.world_transform(cloth_id);
    scene.set_world_transform(
        cloth_id,
        Transform {
            location: world.location + delta,
            rotation: world.rotation,
            scale: world.scale,
        },
    );
}

/// Keep anchors on corners / proportional UV when grid resolution changes.
pub fn remap_cloth_anchors(
    anchors: &[ClothAnchor],
    old_su: u32,
    old_sv: u32,
    new_su: u32,
    new_sv: u32,
) -> Vec<ClothAnchor> {
    let old_su = old_su.max(1);
    let old_sv = old_sv.max(1);
    let new_su = new_su.max(1);
    let new_sv = new_sv.max(1);
    let map_u = |u: u32| -> u32 {
        if u == 0 {
            0
        } else if u >= old_su {
            new_su
        } else {
            ((u as f32 / old_su as f32) * new_su as f32).round() as u32
        }
        .min(new_su)
    };
    let map_v = |v: u32| -> u32 {
        if v == 0 {
            0
        } else if v >= old_sv {
            new_sv
        } else {
            ((v as f32 / old_sv as f32) * new_sv as f32).round() as u32
        }
        .min(new_sv)
    };
    let mut out: Vec<ClothAnchor> = Vec::with_capacity(anchors.len());
    for a in anchors {
        let mut b = *a;
        b.u = map_u(a.u);
        b.v = map_v(a.v);
        if let Some(existing) = out.iter_mut().find(|x| x.u == b.u && x.v == b.v) {
            if existing.object.is_none() && b.object.is_some() {
                *existing = b;
            }
        } else {
            out.push(b);
        }
    }
    out
}

/// Align every cloth that has at least one pin (design-mode keep-alive).
pub fn sync_pinned_cloths(scene: &mut Scene) {
    let ids: Vec<ObjectId> = scene
        .objects()
        .iter()
        .filter(|o| {
            o.visible
                && o.primitive.is_cloth()
                && o.cloth_anchors.iter().any(|a| a.object.is_some())
        })
        .map(|o| o.id)
        .collect();
    for id in ids {
        align_cloth_to_pins(scene, id);
    }
}
