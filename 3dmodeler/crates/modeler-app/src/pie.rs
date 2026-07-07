//! Generic pie / wheel menu: chips arranged around a hub at the cursor, the
//! hovered slot picked by mouse DIRECTION from the hub (Blender pie
//! behavior), so a quick flick-and-click triggers an action without precise
//! aiming. Slot 0 sits north, following slots go clockwise.
//!
//! This module only draws and hit-tests. Opening, click consumption and
//! committing are the caller's job — see add_menu.rs (Shift+A add wheel)
//! and context_menu.rs (right-click object wheel) for the event pattern.

use three_d::egui;

pub const RADIUS: f32 = 96.0;
pub const HUB_RADIUS: f32 = 22.0;

pub enum PieIcon {
    /// A font glyph (only use glyphs already proven to render in this app).
    Glyph(&'static str),
    // primitives (line-art)
    Cube,
    UvSphere,
    IcoSphere,
    Cone,
    Cylinder,
    Torus,
    Plane,
    Wall,
    // actions (line-art)
    Duplicate,
    Anchor,
    Ungroup,
    Attach,
    Door,
    Window,
}

pub struct PieSlot {
    pub label: String,
    pub icon: PieIcon,
    pub enabled: bool,
}

impl PieSlot {
    pub fn new(label: impl Into<String>, icon: PieIcon) -> Self {
        Self { label: label.into(), icon, enabled: true }
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
}

/// Unit direction of a slot (screen space, y down); slot 0 = north.
pub fn slot_dir(slot: usize, count: usize) -> egui::Vec2 {
    let angle = slot as f32 * std::f32::consts::TAU / count as f32
        - std::f32::consts::FRAC_PI_2;
    egui::vec2(angle.cos(), angle.sin())
}

/// Draw the wheel and return the hovered ENABLED slot. `anim` is the
/// caller-owned 0→1 scale-in state, reset it to 0 when (re)opening.
pub fn draw(
    ctx: &egui::Context,
    id: &str,
    desired_center: egui::Pos2,
    hub_label: &str,
    slots: &[PieSlot],
    anim: &mut f32,
) -> Option<usize> {
    let count = slots.len().max(1);

    // scale-in: quick cubic ease-out
    let dt = ctx.input(|i| i.stable_dt).min(0.05);
    if *anim < 1.0 {
        *anim = (*anim + dt * 9.0).min(1.0);
        ctx.request_repaint();
    }
    let ease = 1.0 - (1.0 - *anim).powi(3);

    // keep the whole wheel on screen (chips extend past the ring)
    let screen = ctx.content_rect();
    let margin_x = RADIUS + 110.0;
    let margin_y = RADIUS + 50.0;
    let mut center = desired_center;
    center.x = center.x.clamp(screen.left() + margin_x, screen.right() - margin_x);
    center.y = center.y.clamp(screen.top() + margin_y, screen.bottom() - margin_y);
    let radius = RADIUS * ease;

    // hovered slot from the mouse direction relative to the hub
    let sector = 360.0 / count as f32;
    let hovered_sector = ctx.pointer_hover_pos().and_then(|pointer| {
        let delta = pointer - center;
        (delta.length() > HUB_RADIUS + 4.0).then(|| {
            let deg = delta.y.atan2(delta.x).to_degrees(); // 0° = east, y down
            ((deg + 90.0 + sector * 0.5).rem_euclid(360.0) / sector) as usize % count
        })
    });
    let hovered = hovered_sector.filter(|&i| slots[i].enabled);

    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new(id),
    ));
    let visuals = ctx.global_style().visuals.clone();
    let accent = visuals.hyperlink_color;
    let text_color = visuals.text_color();
    let disabled_color = text_color.gamma_multiply(0.4);
    let font = egui::FontId::proportional(13.0);

    // soft backdrop so the wheel reads as one layer over the viewport
    painter.circle_filled(
        center,
        radius + 70.0,
        egui::Color32::from_black_alpha((46.0 * ease) as u8),
    );

    // direction ray towards the hovered slot
    if let Some(slot) = hovered {
        let dir = slot_dir(slot, count);
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
        hub_label,
        egui::FontId::proportional(11.0),
        text_color,
    );

    for (i, slot) in slots.iter().enumerate() {
        let selected = hovered == Some(i);
        let dir = slot_dir(i, count);
        let slot_pos = center + dir * radius;

        let label_color = if !slot.enabled {
            disabled_color
        } else if selected {
            visuals.selection.stroke.color
        } else {
            text_color
        };
        let galley = painter.layout_no_wrap(slot.label.clone(), font.clone(), label_color);

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
        let rect =
            egui::Rect::from_center_size(slot_pos + egui::vec2(shift, 0.0), chip_size);

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

        let icon_center = egui::pos2(rect.left() + pad.x + icon_w * 0.5, rect.center().y);
        draw_icon(
            &painter,
            &slot.icon,
            icon_center,
            7.0,
            egui::Stroke::new(1.4, label_color),
        );
        painter.galley(
            egui::pos2(
                rect.left() + pad.x + icon_w + 4.0,
                rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            label_color,
        );
    }

    hovered
}

/// Tiny line-art icon (or glyph) inside a chip.
fn draw_icon(
    painter: &egui::Painter,
    icon: &PieIcon,
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
    match icon {
        PieIcon::Glyph(glyph) => {
            painter.text(
                c,
                egui::Align2::CENTER_CENTER,
                *glyph,
                egui::FontId::proportional(14.0),
                stroke.color,
            );
        }
        // Cube: front square + offset top/side faces
        PieIcon::Cube => {
            let d = egui::vec2(0.55 * s, -0.55 * s);
            let rect = egui::Rect::from_two_pos(p(-0.9, -0.35), p(0.35, 0.9));
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
            for corner in [rect.left_top(), rect.right_top(), rect.right_bottom()] {
                painter.line_segment([corner, corner + d], stroke);
            }
            painter.line_segment([rect.left_top() + d, rect.right_top() + d], stroke);
            painter.line_segment([rect.right_top() + d, rect.right_bottom() + d], stroke);
        }
        // UV Sphere: circle with latitude chords
        PieIcon::UvSphere => {
            painter.circle_stroke(c, s, stroke);
            painter.line_segment([p(-1.0, 0.0), p(1.0, 0.0)], stroke);
            let w = (1.0f32 - 0.55 * 0.55).sqrt();
            painter.line_segment([p(-w, -0.55), p(w, -0.55)], stroke);
            painter.line_segment([p(-w, 0.55), p(w, 0.55)], stroke);
        }
        // Ico Sphere: circle with an inscribed triangle
        PieIcon::IcoSphere => {
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
        PieIcon::Cone => {
            painter.add(ellipse(p(0.0, 0.55), egui::vec2(0.8 * s, 0.3 * s)));
            painter.line_segment([p(-0.8, 0.55), p(0.0, -0.9)], stroke);
            painter.line_segment([p(0.8, 0.55), p(0.0, -0.9)], stroke);
        }
        // Cylinder: two ellipses joined by sides
        PieIcon::Cylinder => {
            painter.add(ellipse(p(0.0, -0.6), egui::vec2(0.75 * s, 0.3 * s)));
            painter.add(ellipse(p(0.0, 0.6), egui::vec2(0.75 * s, 0.3 * s)));
            painter.line_segment([p(-0.75, -0.6), p(-0.75, 0.6)], stroke);
            painter.line_segment([p(0.75, -0.6), p(0.75, 0.6)], stroke);
        }
        // Torus: concentric circles
        PieIcon::Torus => {
            painter.circle_stroke(c, s, stroke);
            painter.circle_stroke(c, 0.42 * s, stroke);
        }
        // Plane: flat parallelogram
        PieIcon::Plane => {
            let quad = vec![p(-1.0, 0.55), p(-0.35, -0.55), p(1.0, -0.55), p(0.35, 0.55)];
            painter.add(egui::Shape::closed_line(quad, stroke));
        }
        // Wall: brick courses
        PieIcon::Wall => {
            let rect = egui::Rect::from_two_pos(p(-1.0, -0.65), p(1.0, 0.65));
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
            painter.line_segment([p(-1.0, -0.22), p(1.0, -0.22)], stroke);
            painter.line_segment([p(-1.0, 0.22), p(1.0, 0.22)], stroke);
            painter.line_segment([p(0.0, -0.65), p(0.0, -0.22)], stroke);
            painter.line_segment([p(-0.5, -0.22), p(-0.5, 0.22)], stroke);
            painter.line_segment([p(0.5, -0.22), p(0.5, 0.22)], stroke);
            painter.line_segment([p(0.0, 0.22), p(0.0, 0.65)], stroke);
        }
        // Duplicate: two overlapping squares
        PieIcon::Duplicate => {
            let back = egui::Rect::from_two_pos(p(-0.9, -0.9), p(0.45, 0.45));
            let front = egui::Rect::from_two_pos(p(-0.45, -0.45), p(0.9, 0.9));
            painter.rect_stroke(back, 0.0, stroke, egui::StrokeKind::Middle);
            painter.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Middle);
        }
        // Anchor: ring on a shank with flukes
        PieIcon::Anchor => {
            painter.circle_stroke(p(0.0, -0.7), 0.25 * s, stroke);
            painter.line_segment([p(0.0, -0.45), p(0.0, 0.85)], stroke);
            painter.line_segment([p(-0.5, -0.05), p(0.5, -0.05)], stroke);
            painter.line_segment([p(0.0, 0.85), p(-0.65, 0.35)], stroke);
            painter.line_segment([p(0.0, 0.85), p(0.65, 0.35)], stroke);
        }
        // Ungroup: two squares moving apart
        PieIcon::Ungroup => {
            let a = egui::Rect::from_two_pos(p(-1.0, -0.9), p(-0.1, 0.0));
            let b = egui::Rect::from_two_pos(p(0.1, 0.0), p(1.0, 0.9));
            painter.rect_stroke(a, 0.0, stroke, egui::StrokeKind::Middle);
            painter.rect_stroke(b, 0.0, stroke, egui::StrokeKind::Middle);
        }
        // Attach: two interlocked rings
        PieIcon::Attach => {
            painter.circle_stroke(p(-0.4, 0.0), 0.55 * s, stroke);
            painter.circle_stroke(p(0.4, 0.0), 0.55 * s, stroke);
        }
        // Door: leaf with a handle
        PieIcon::Door => {
            let rect = egui::Rect::from_two_pos(p(-0.6, -0.9), p(0.6, 0.9));
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
            painter.circle_filled(p(0.3, 0.1), 0.12 * s, stroke.color);
        }
        // Window: frame with cross panes
        PieIcon::Window => {
            let rect = egui::Rect::from_two_pos(p(-0.8, -0.8), p(0.8, 0.8));
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
            painter.line_segment([p(-0.8, 0.0), p(0.8, 0.0)], stroke);
            painter.line_segment([p(0.0, -0.8), p(0.0, 0.8)], stroke);
        }
    }
}
