//! Blender-style navigation gizmo: the axis balls in the viewport corner.
//! Clicking a ball snaps the view to look along that axis.

use crate::camera::BlenderCamera;
use three_d::egui;
use three_d::InnerSpace;

const RADIUS: f32 = 40.0; // widget radius in ui points
const BALL_RADIUS: f32 = 8.0;

struct AxisBall {
    label: &'static str,
    world: three_d::Vec3,
    color: egui::Color32,
    /// (yaw°, pitch°) that makes the camera look along -world (from the ball side)
    view: (f32, f32),
    positive: bool,
}

fn balls() -> [AxisBall; 6] {
    use three_d::vec3;
    let red = egui::Color32::from_rgb(230, 100, 90);
    let green = egui::Color32::from_rgb(130, 190, 80);
    let blue = egui::Color32::from_rgb(90, 140, 230);
    [
        AxisBall { label: "X", world: vec3(1.0, 0.0, 0.0), color: red, view: (90.0, 0.0), positive: true },
        AxisBall { label: "-X", world: vec3(-1.0, 0.0, 0.0), color: red, view: (-90.0, 0.0), positive: false },
        AxisBall { label: "Y", world: vec3(0.0, 1.0, 0.0), color: green, view: (180.0, 0.0), positive: true },
        AxisBall { label: "-Y", world: vec3(0.0, -1.0, 0.0), color: green, view: (0.0, 0.0), positive: false },
        AxisBall { label: "Z", world: vec3(0.0, 0.0, 1.0), color: blue, view: (0.0, 90.0), positive: true },
        AxisBall { label: "-Z", world: vec3(0.0, 0.0, -1.0), color: blue, view: (0.0, -90.0), positive: false },
    ]
}

pub fn axis_widget(
    ctx: &egui::Context,
    camera: &mut BlenderCamera,
    right_offset: f32,
    top_offset: f32,
) {
    let screen = ctx.content_rect();
    let center = egui::pos2(
        screen.right() - right_offset - RADIUS - 16.0,
        screen.top() + top_offset + RADIUS + 16.0,
    );
    let widget_rect = egui::Rect::from_center_size(center, egui::vec2(2.0 * RADIUS, 2.0 * RADIUS));

    egui::Area::new(egui::Id::new("axis-widget"))
        .fixed_pos(widget_rect.min)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(2.0 * RADIUS, 2.0 * RADIUS), egui::Sense::click());
            let painter = ui.painter();
            let center = rect.center();

            if response.hovered() {
                painter.circle_filled(center, RADIUS, egui::Color32::from_black_alpha(60));
            }

            let (right, up, forward) = camera.screen_basis();

            // project each axis into screen space
            let mut projected: Vec<(egui::Pos2, f32, &AxisBall)> = Vec::new();
            let ball_list = balls();
            for ball in &ball_list {
                let sx = ball.world.dot(right);
                let sy = ball.world.dot(up);
                let depth = ball.world.dot(forward); // > 0 = pointing away from viewer
                let pos = center + egui::vec2(sx, -sy) * (RADIUS - BALL_RADIUS - 2.0);
                projected.push((pos, depth, ball));
            }
            // draw farthest first
            projected.sort_by(|a, b| b.1.total_cmp(&a.1));

            let clicked_at = response.interact_pointer_pos().filter(|_| response.clicked());
            let mut clicked_view: Option<(f32, f32)> = None;

            for (pos, depth, ball) in &projected {
                let toward_viewer = *depth < 0.0;
                let alpha = if toward_viewer { 255 } else { 140 };
                let color = egui::Color32::from_rgba_unmultiplied(
                    ball.color.r(),
                    ball.color.g(),
                    ball.color.b(),
                    alpha,
                );
                if ball.positive {
                    painter.line_segment([center, *pos], egui::Stroke::new(2.0, color));
                    painter.circle_filled(*pos, BALL_RADIUS, color);
                    painter.text(
                        *pos,
                        egui::Align2::CENTER_CENTER,
                        ball.label,
                        egui::FontId::proportional(10.0),
                        egui::Color32::from_black_alpha(220),
                    );
                } else {
                    painter.circle_filled(*pos, BALL_RADIUS, egui::Color32::from_black_alpha(60));
                    painter.circle_stroke(*pos, BALL_RADIUS, egui::Stroke::new(1.5, color));
                }
                if let Some(click) = clicked_at {
                    if click.distance(*pos) <= BALL_RADIUS + 2.0 {
                        clicked_view = Some(ball.view);
                    }
                }
            }

            if let Some((yaw, pitch)) = clicked_view {
                camera.set_view(yaw, pitch);
            }
        });
}

/// Blender-style view name overlay in the top-left viewport corner.
pub fn view_label(ctx: &egui::Context, camera: &BlenderCamera, left_offset: f32, top_offset: f32) {
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("view-label"))
        .fixed_pos(egui::pos2(
            screen.left() + left_offset + 12.0,
            screen.top() + top_offset + 8.0,
        ))
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(camera.view_name())
                    .size(13.0)
                    .color(egui::Color32::from_rgba_unmultiplied(220, 220, 225, 180)),
            );
        });
}
