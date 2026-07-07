//! Blender's Shift+A "Add" menu as a pie / wheel menu.
//!
//! Opens centered on the cursor: eight chips (the seven primitives + Wall)
//! arranged around a hub, each with a small line-art icon. The hovered slot
//! is picked by the mouse DIRECTION from the hub (Blender pie behavior), so
//! a quick flick-and-click adds a mesh without precise aiming. LMB commits
//! the hovered slot, RMB / Esc / clicking other UI cancels.
//!
//! Events are consumed in `handle_events` (which runs after the egui pass,
//! see main.rs) so a commit click never falls through to viewport picking;
//! the actual commit happens on the next `ui` call via `pending_click`.

use crate::object_ops::event_pos_to_egui;
use modeler_core::{Primitive, Scene, Transform};
use three_d::egui;
use three_d::{Event, Key, MouseButton, Viewport};

const SLOTS: usize = 8;
const PIE_RADIUS: f32 = 96.0;
const HUB_RADIUS: f32 = 22.0;

#[derive(Clone, Copy)]
enum PieItem {
    Primitive(Primitive),
    Wall,
}

/// Slot order around the wheel, starting north and going clockwise:
/// N, NE, E, SE, S, SW, W, NW. Cube sits on top — it is used the most.
fn pie_items() -> [(PieItem, &'static str); SLOTS] {
    let c = Primitive::catalog(); // [Plane, Cube, UvSphere, IcoSphere, Cylinder, Cone, Torus]
    [
        (PieItem::Primitive(c[1]), "Cube"),       // N
        (PieItem::Primitive(c[2]), "UV Sphere"),  // NE
        (PieItem::Primitive(c[3]), "Ico Sphere"), // E
        (PieItem::Primitive(c[5]), "Cone"),       // SE
        (PieItem::Primitive(c[4]), "Cylinder"),   // S
        (PieItem::Primitive(c[6]), "Torus"),      // SW
        (PieItem::Primitive(c[0]), "Plane"),      // W
        (PieItem::Wall, "Wall"),                  // NW
    ]
}

/// Unit direction of a slot (screen space, y down); slot 0 = north.
fn slot_dir(slot: usize) -> egui::Vec2 {
    let angle = (slot as f32) * std::f32::consts::TAU / SLOTS as f32
        - std::f32::consts::FRAC_PI_2;
    egui::vec2(angle.cos(), angle.sin())
}

pub struct AddMenu {
    open: bool,
    position: egui::Pos2,
    last_mouse: egui::Pos2,
    /// LMB arrived in `handle_events`; commit on the next `ui` pass.
    pending_click: bool,
    /// 0 → 1 scale-in animation.
    anim: f32,
}

impl AddMenu {
    pub fn new() -> Self {
        Self {
            open: false,
            position: egui::Pos2::new(200.0, 200.0),
            last_mouse: egui::Pos2::new(200.0, 200.0),
            pending_click: false,
            anim: 0.0,
        }
    }

    /// Track the mouse and open/close on Shift+A / Escape / clicks.
    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        viewport: Viewport,
        device_pixel_ratio: f32,
    ) {
        // If egui consumed the key press (e.g. a focused text field), the
        // accompanying Text event must not trigger the menu either.
        let key_a_consumed = events.iter().any(|e| {
            matches!(
                e,
                Event::KeyPress { kind: Key::A, handled: true, .. }
            )
        });

        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, .. } => {
                    self.last_mouse =
                        event_pos_to_egui(position.x, position.y, viewport, device_pixel_ratio);
                }
                // Layout-aware path: an uppercase "A" was typed (Shift+A on
                // any keyboard layout — Key::* codes are PHYSICAL positions
                // on the web backend, which breaks e.g. AZERTY).
                Event::Text(text) if text == "A" && !key_a_consumed => {
                    self.open_at(self.last_mouse);
                }
                // Physical-key fallback (layout-correct on most native
                // backends; harmless double-fire alongside the Text path).
                Event::KeyPress {
                    kind: Key::A,
                    modifiers,
                    handled,
                    ..
                } if !*handled && modifiers.shift => {
                    self.open_at(self.last_mouse);
                    *handled = true;
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.open => {
                    self.open = false;
                    self.pending_click = false;
                    *handled = true;
                }
                Event::MousePress { button, handled, .. } if self.open => {
                    if *handled {
                        // egui took it (menu bar, sidebar…): just dismiss
                        self.open = false;
                        self.pending_click = false;
                    } else {
                        *handled = true; // never falls through to picking
                        if *button == MouseButton::Left {
                            self.pending_click = true;
                        } else {
                            self.open = false; // RMB / MMB cancels
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn open_at(&mut self, pos: egui::Pos2) {
        self.open = true;
        self.position = pos;
        self.pending_click = false;
        self.anim = 0.0;
    }

    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        wall_tool: &mut crate::wall_tool::WallTool,
        settings: &crate::settings::Settings,
    ) {
        if !self.open {
            return;
        }
        // scale-in: quick cubic ease-out
        let dt = ctx.input(|i| i.stable_dt).min(0.05);
        if self.anim < 1.0 {
            self.anim = (self.anim + dt * 9.0).min(1.0);
            ctx.request_repaint();
        }
        let ease = 1.0 - (1.0 - self.anim).powi(3);

        // keep the whole wheel on screen (chips extend past the ring)
        let screen = ctx.content_rect();
        let margin_x = PIE_RADIUS + 110.0;
        let margin_y = PIE_RADIUS + 50.0;
        let mut center = self.position;
        center.x = center.x.clamp(screen.left() + margin_x, screen.right() - margin_x);
        center.y = center.y.clamp(screen.top() + margin_y, screen.bottom() - margin_y);
        let radius = PIE_RADIUS * ease;

        // hovered slot from the mouse direction relative to the hub
        let pointer = ctx.pointer_hover_pos().unwrap_or(self.last_mouse);
        let delta = pointer - center;
        let hovered = if delta.length() > HUB_RADIUS + 4.0 {
            let deg = delta.y.atan2(delta.x).to_degrees(); // 0° = east, y down
            Some(((deg + 90.0 + 22.5).rem_euclid(360.0) / 45.0) as usize % SLOTS)
        } else {
            None
        };

        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("add-pie"),
        ));
        let visuals = ctx.global_style().visuals.clone();
        let accent = visuals.hyperlink_color;
        let text_color = visuals.text_color();
        let font = egui::FontId::proportional(13.0);

        // soft backdrop so the wheel reads as one layer over the viewport
        painter.circle_filled(
            center,
            radius + 70.0,
            egui::Color32::from_black_alpha((46.0 * ease) as u8),
        );

        // direction ray towards the hovered slot
        if let Some(slot) = hovered {
            let dir = slot_dir(slot);
            painter.line_segment(
                [center + dir * HUB_RADIUS, center + dir * (radius - 14.0)],
                egui::Stroke::new(2.0, accent),
            );
        }

        // hub
        painter.circle_filled(center, HUB_RADIUS, visuals.window_fill);
        painter.circle_stroke(center, HUB_RADIUS, egui::Stroke::new(1.5, accent));
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            "Add",
            egui::FontId::proportional(12.0),
            text_color,
        );

        let items = pie_items();
        for (i, (_, label)) in items.iter().enumerate() {
            let selected = hovered == Some(i);
            let dir = slot_dir(i);
            let slot_pos = center + dir * radius;

            let label_color = if selected {
                visuals.selection.stroke.color
            } else {
                text_color
            };
            let galley = painter.layout_no_wrap(label.to_string(), font.clone(), label_color);

            // chip geometry: icon + label; side chips grow away from the hub
            let icon_w = 20.0;
            let pad = egui::vec2(9.0, 6.0);
            let chip_size = egui::vec2(
                icon_w + 4.0 + galley.size().x + pad.x * 2.0,
                galley.size().y.max(16.0) + pad.y * 2.0,
            );
            let shift = if dir.x > 0.35 {
                chip_size.x * 0.5
            } else if dir.x < -0.35 {
                -chip_size.x * 0.5
            } else {
                0.0
            };
            let rect = egui::Rect::from_center_size(
                slot_pos + egui::vec2(shift, 0.0),
                chip_size,
            );

            // fake drop shadow + chip body
            painter.rect_filled(
                rect.translate(egui::vec2(1.5, 2.5)),
                6.0,
                egui::Color32::from_black_alpha(70),
            );
            let fill = if selected { visuals.selection.bg_fill } else { visuals.window_fill };
            let stroke = if selected {
                egui::Stroke::new(1.5, accent)
            } else {
                visuals.window_stroke
            };
            painter.rect_filled(rect, 6.0, fill);
            painter.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);

            let icon_center =
                egui::pos2(rect.left() + pad.x + icon_w * 0.5, rect.center().y);
            draw_icon(&painter, i, icon_center, 7.0, egui::Stroke::new(1.4, label_color));
            painter.galley(
                egui::pos2(rect.left() + pad.x + icon_w + 4.0, rect.center().y - galley.size().y * 0.5),
                galley,
                label_color,
            );
        }

        // commit / cancel (the click was consumed in handle_events);
        // clicking the hub or dead center closes without adding
        if self.pending_click {
            self.pending_click = false;
            if let Some(slot) = hovered {
                match items[slot].0 {
                    PieItem::Primitive(primitive) => {
                        scene.add_object(primitive, Transform::default());
                    }
                    PieItem::Wall => wall_tool.start(settings),
                }
            }
            self.open = false;
        }
    }
}

/// Tiny line-art icon per pie slot (same index order as `pie_items`).
fn draw_icon(
    painter: &egui::Painter,
    slot: usize,
    c: egui::Pos2,
    s: f32,
    stroke: egui::Stroke,
) {
    let p = |x: f32, y: f32| c + egui::vec2(x * s, y * s);
    let ellipse = |center: egui::Pos2, radius: egui::Vec2| {
        egui::Shape::Ellipse(egui::epaint::EllipseShape {
            center,
            radius,
            angle: 0.0,
            fill: egui::Color32::TRANSPARENT,
            stroke,
        })
    };
    match slot {
        // Cube: front square + offset top/side faces
        0 => {
            let d = egui::vec2(0.55 * s, -0.55 * s);
            let (a, b) = (p(-0.9, -0.35), p(0.35, 0.9));
            let rect = egui::Rect::from_two_pos(a, b);
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
            for corner in [rect.left_top(), rect.right_top(), rect.right_bottom()] {
                painter.line_segment([corner, corner + d], stroke);
            }
            painter.line_segment([rect.left_top() + d, rect.right_top() + d], stroke);
            painter.line_segment([rect.right_top() + d, rect.right_bottom() + d], stroke);
        }
        // UV Sphere: circle with latitude chords
        1 => {
            painter.circle_stroke(c, s, stroke);
            painter.line_segment([p(-1.0, 0.0), p(1.0, 0.0)], stroke);
            let w = (1.0f32 - 0.55 * 0.55).sqrt();
            painter.line_segment([p(-w, -0.55), p(w, -0.55)], stroke);
            painter.line_segment([p(-w, 0.55), p(w, 0.55)], stroke);
        }
        // Ico Sphere: circle with an inscribed triangle
        2 => {
            painter.circle_stroke(c, s, stroke);
            let tri: Vec<egui::Pos2> = [-90.0f32, 30.0, 150.0]
                .iter()
                .map(|deg| {
                    let r = deg.to_radians();
                    p(r.cos(), r.sin())
                })
                .collect();
            painter.add(egui::Shape::closed_line(tri, stroke));
        }
        // Cone: triangle over a base ellipse
        3 => {
            painter.add(ellipse(p(0.0, 0.55), egui::vec2(0.8 * s, 0.3 * s)));
            painter.line_segment([p(-0.8, 0.55), p(0.0, -0.9)], stroke);
            painter.line_segment([p(0.8, 0.55), p(0.0, -0.9)], stroke);
        }
        // Cylinder: two ellipses joined by sides
        4 => {
            painter.add(ellipse(p(0.0, -0.6), egui::vec2(0.75 * s, 0.3 * s)));
            painter.add(ellipse(p(0.0, 0.6), egui::vec2(0.75 * s, 0.3 * s)));
            painter.line_segment([p(-0.75, -0.6), p(-0.75, 0.6)], stroke);
            painter.line_segment([p(0.75, -0.6), p(0.75, 0.6)], stroke);
        }
        // Torus: concentric circles
        5 => {
            painter.circle_stroke(c, s, stroke);
            painter.circle_stroke(c, 0.42 * s, stroke);
        }
        // Plane: flat parallelogram
        6 => {
            let quad = vec![p(-1.0, 0.55), p(-0.35, -0.55), p(1.0, -0.55), p(0.35, 0.55)];
            painter.add(egui::Shape::closed_line(quad, stroke));
        }
        // Wall: brick courses
        _ => {
            let rect = egui::Rect::from_two_pos(p(-1.0, -0.65), p(1.0, 0.65));
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
            painter.line_segment([p(-1.0, -0.22), p(1.0, -0.22)], stroke);
            painter.line_segment([p(-1.0, 0.22), p(1.0, 0.22)], stroke);
            painter.line_segment([p(0.0, -0.65), p(0.0, -0.22)], stroke);
            painter.line_segment([p(-0.5, -0.22), p(-0.5, 0.22)], stroke);
            painter.line_segment([p(0.5, -0.22), p(0.5, 0.22)], stroke);
            painter.line_segment([p(0.0, 0.22), p(0.0, 0.65)], stroke);
        }
    }
}

/// The primitive list as menu buttons; returns the clicked primitive.
/// Used by the menu-bar Add dropdown (the Shift+A popup is the pie above).
pub fn mesh_menu_buttons(ui: &mut egui::Ui) -> Option<Primitive> {
    let mut clicked = None;
    for primitive in Primitive::catalog() {
        let label = match primitive {
            Primitive::UvSphere { .. } => "UV Sphere".to_string(),
            Primitive::IcoSphere { .. } => "Ico Sphere".to_string(),
            other => other.base_name().to_string(),
        };
        if ui.button(label).clicked() {
            clicked = Some(primitive);
        }
    }
    clicked
}
