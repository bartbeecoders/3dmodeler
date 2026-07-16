//! Initial-force arrows for dynamic objects.
//!
//! Each dynamic object with a non-zero `initial_force` draws a world-space
//! arrow from its origin in the force direction. Selected dynamic objects
//! also get a tip handle: **Shift+drag** the tip to set the vector (zero
//! force shows a small rest handle at the origin so you can pull a force
//! out of nothing). Plain clicks leave the handle alone so selection and
//! other tools work. Esc cancels an in-progress drag and restores the
//! original force.
//!
//! The stored value is a **world-space linear impulse (N·s)** applied once
//! when simulation starts. Visual length is `impulse * VISUAL_SCALE` meters.

use crate::camera::BlenderCamera;
use crate::selection::Selection;
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Scene};
use three_d::egui;
use three_d::{Event, Key, MouseButton, Viewport};

/// World meters of arrow per unit of impulse. A force of (0, 0, 10) draws a
/// 1 m arrow; scale is also used when converting a dragged tip back to force.
pub const VISUAL_SCALE: f32 = 0.1;
/// Handle disc / pick radii, logical points.
const HANDLE_RADIUS: f32 = 7.0;
const PICK_RADIUS: f32 = 14.0;
const FORCE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 140, 40);
const FORCE_COLOR_SEL: egui::Color32 = egui::Color32::from_rgb(255, 190, 80);
const ZERO_HANDLE: egui::Color32 = egui::Color32::from_rgb(180, 160, 140);

struct Drag {
    id: ObjectId,
    /// Original force, restored on Esc.
    orig: Vec3,
}

pub struct ForceHandles {
    drag: Option<Drag>,
}

impl ForceHandles {
    pub fn new() -> Self {
        Self { drag: None }
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// World-space tip of the force arrow (origin + force × scale). For a
    /// zero force the tip sits at the origin itself.
    pub fn tip_world(origin: Vec3, force: Vec3) -> Vec3 {
        if force.length_squared() < 1e-12 {
            origin
        } else {
            origin + force * VISUAL_SCALE
        }
    }

    fn force_from_tip(origin: Vec3, tip: Vec3) -> Vec3 {
        (tip - origin) / VISUAL_SCALE
    }

    /// Visible dynamic solids: (id, world origin, force).
    fn arrows(scene: &Scene) -> Vec<(ObjectId, Vec3, Vec3)> {
        let mut out = Vec::new();
        let worlds = scene.world_transforms();
        for object in scene.objects() {
            if !object.visible || !object.dynamic {
                continue;
            }
            if object.primitive.is_light()
                || matches!(object.primitive, modeler_core::Primitive::Empty { .. })
            {
                continue;
            }
            let origin = worlds
                .get(&object.id)
                .map(|t| t.location)
                .unwrap_or(object.transform.location);
            out.push((object.id, origin, object.initial_force));
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
                    modifiers,
                    handled,
                    ..
                } if !*handled
                    && !pointer_over_ui
                    && self.drag.is_none()
                    && modifiers.shift =>
                {
                    // Shift+drag only — plain LMB is free for selection / grab.
                    let pick = PICK_RADIUS * device_pixel_ratio;
                    let mut best: Option<(f32, ObjectId)> = None;
                    for (id, origin, force) in Self::arrows(scene) {
                        if !selection.is_selected(id) {
                            continue;
                        }
                        let tip = Self::tip_world(origin, force);
                        let Some((sx, sy)) =
                            camera.project(viewport, three_d::vec3(tip.x, tip.y, tip.z))
                        else {
                            continue;
                        };
                        let d = (egui::vec2(sx - position.x, sy - position.y)).length();
                        if d < pick && best.is_none_or(|(bd, _)| d < bd) {
                            best = Some((d, id));
                        }
                    }
                    if let Some((_, id)) = best {
                        let force = scene
                            .object(id)
                            .map(|o| o.initial_force)
                            .unwrap_or(Vec3::ZERO);
                        self.drag = Some(Drag { id, orig: force });
                        *handled = true;
                    }
                }
                Event::MouseMotion { position, .. } if self.drag.is_some() => {
                    let id = self.drag.as_ref().unwrap().id;
                    let origin = scene.world_transform(id).location;
                    // Intersect the pick ray with the plane through the object
                    // that faces the camera — free 3D-ish drag of the tip.
                    if let Some(tip) =
                        plane_hit(camera, viewport, position.x, position.y, origin)
                    {
                        let force = Self::force_from_tip(origin, tip);
                        if let Some(object) = scene.object_mut(id) {
                            object.initial_force = force;
                        }
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
                        object.initial_force = drag.orig;
                    }
                    *handled = true;
                }
                _ => {}
            }
        }
    }

    /// Draw force arrows for every visible dynamic object that has a force,
    /// plus tip handles on the selection (even when the force is zero).
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
        let arrows = Self::arrows(scene);
        if arrows.is_empty() {
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

        for (id, origin, force) in arrows {
            let selected = selection.is_selected(id);
            let has_force = force.length_squared() > 1e-12;
            if !has_force && !selected {
                continue;
            }
            let tip = Self::tip_world(origin, force);
            let color = if selected { FORCE_COLOR_SEL } else { FORCE_COLOR };
            let (Some(a), Some(b)) = (project(origin), project(tip)) else {
                continue;
            };

            if has_force {
                draw_arrow(&painter, a, b, color, 2.0);
                let mag = force.length();
                let label = if mag >= 100.0 {
                    format!("{mag:.0}")
                } else if mag >= 10.0 {
                    format!("{mag:.1}")
                } else {
                    format!("{mag:.2}")
                };
                let font = egui::FontId::proportional(11.0);
                let text_pos = b + egui::vec2(8.0, -4.0);
                let rect = painter.text(
                    text_pos,
                    egui::Align2::LEFT_BOTTOM,
                    &label,
                    font.clone(),
                    color,
                );
                painter.rect_filled(
                    rect.expand(2.5),
                    2.0,
                    egui::Color32::from_black_alpha(140),
                );
                painter.text(text_pos, egui::Align2::LEFT_BOTTOM, &label, font, color);
            } else if selected {
                // rest handle at origin so the user can pull a force out
                painter.circle_stroke(a, HANDLE_RADIUS, egui::Stroke::new(1.5, ZERO_HANDLE));
            }

            if selected {
                let tip_color = if has_force { color } else { ZERO_HANDLE };
                let hover = pointer.is_some_and(|p| (p - b).length() < PICK_RADIUS);
                let active = self.drag.as_ref().is_some_and(|d| d.id == id);
                let fill = if hover || active {
                    tip_color
                } else {
                    egui::Color32::from_black_alpha(160)
                };
                painter.circle_filled(b, HANDLE_RADIUS, fill);
                painter.circle_stroke(b, HANDLE_RADIUS, egui::Stroke::new(1.5, tip_color));
            }
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

fn draw_arrow(
    painter: &egui::Painter,
    from: egui::Pos2,
    to: egui::Pos2,
    color: egui::Color32,
    width: f32,
) {
    let dir = to - from;
    let len = dir.length();
    if len < 1.0 {
        painter.circle_filled(from, 3.0, color);
        return;
    }
    let dir_n = dir / len;
    let head = 10.0_f32.min(len * 0.35);
    let wing = egui::vec2(-dir_n.y, dir_n.x) * (head * 0.45);
    let base = to - dir_n * head;
    painter.line_segment([from, base], egui::Stroke::new(width, color));
    painter.add(egui::Shape::convex_polygon(
        vec![to, base + wing, base - wing],
        color,
        egui::Stroke::NONE,
    ));
}
