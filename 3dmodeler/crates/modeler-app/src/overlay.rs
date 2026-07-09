//! Viewport overlays drawn with the egui painter (background layer, under
//! the UI panels): object labels, dimension readouts, and measurements.
//! Also hosts the ruler tool state (Add ▸ Measure).

use crate::camera::BlenderCamera;
use crate::modal::{GuideKind, Guides};
use crate::ref_image::CalibrateTool;
use crate::selection::Selection;
use crate::settings::Unit;
use modeler_core::glam::Vec3;
use modeler_core::Scene;
use three_d::egui;
use three_d::Viewport;

const MEASURE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 210, 90);
const LABEL_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 230, 235);
const DIM_COLOR: egui::Color32 = egui::Color32::from_rgb(150, 200, 255);
// matches the axis widget / grid axis colors
const AXIS_COLORS: [egui::Color32; 3] = [
    egui::Color32::from_rgb(230, 100, 90),
    egui::Color32::from_rgb(130, 190, 80),
    egui::Color32::from_rgb(90, 140, 230),
];
const GUIDE_COLOR: egui::Color32 = egui::Color32::from_rgb(235, 235, 235);

/// Ruler tool: two clicks on surfaces create a persistent measurement.
pub struct MeasureTool {
    pub active: bool,
    pub first: Option<Vec3>,
}

impl MeasureTool {
    pub fn new() -> Self {
        Self {
            active: false,
            first: None,
        }
    }

    pub fn start(&mut self) {
        self.active = true;
        self.first = None;
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.first = None;
    }

    /// Feed a picked world point; completes the measurement on the second.
    pub fn add_point(&mut self, point: Vec3, scene: &mut Scene) {
        match self.first.take() {
            None => self.first = Some(point),
            Some(first) => {
                scene.add_measurement(first, point);
                self.active = false;
            }
        }
    }
}

fn to_egui(
    camera: &BlenderCamera,
    viewport: Viewport,
    device_pixel_ratio: f32,
    p: Vec3,
) -> Option<egui::Pos2> {
    let (x, y) = camera.project(viewport, three_d::vec3(p.x, p.y, p.z))?;
    Some(egui::Pos2::new(
        x / device_pixel_ratio,
        (viewport.height as f32 - y) / device_pixel_ratio,
    ))
}

fn text_with_bg(
    painter: &egui::Painter,
    pos: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    size: f32,
    color: egui::Color32,
) {
    let font = egui::FontId::proportional(size);
    let rect = painter.text(pos, anchor, text, font.clone(), color);
    painter.rect_filled(rect.expand(3.0), 3.0, egui::Color32::from_black_alpha(140));
    painter.text(pos, anchor, text, font, color);
}

/// Guide visuals for the active modal transform (Blender-style): axis lines
/// through the pivot while constrained, a dashed pivot-to-mouse line, and a
/// swept arc while rotating.
pub fn draw_modal_guides(
    ctx: &egui::Context,
    camera: &BlenderCamera,
    viewport: Viewport,
    device_pixel_ratio: f32,
    clip: egui::Rect,
    guides: &Guides,
) {
    let painter = ctx.layer_painter(egui::LayerId::background()).with_clip_rect(clip);
    let project = |p: Vec3| to_egui(camera, viewport, device_pixel_ratio, p);
    // three-d mouse position (physical px, bottom-left) -> egui coords
    let mouse = |(x, y): (f32, f32)| {
        egui::Pos2::new(
            x / device_pixel_ratio,
            (viewport.height as f32 - y) / device_pixel_ratio,
        )
    };
    let Some(pivot) = project(guides.pivot) else {
        return;
    };
    let extent = egui::Vec2::new(viewport.width as f32, viewport.height as f32).length()
        / device_pixel_ratio;

    // --- axis guide lines (G/R/S + X/Y/Z, Shift+axis for plane locks) ------
    // A 3D line projects to a 2D line, so sample a nearby point along the
    // axis for the screen direction and extend across the whole viewport.
    let step = camera.world_per_pixel_at(
        viewport,
        three_d::vec3(guides.pivot.x, guides.pivot.y, guides.pivot.z),
    ) * 100.0;
    for &i in &guides.axes {
        let axis = [Vec3::X, Vec3::Y, Vec3::Z][i];
        let Some(sample) = project(guides.pivot + axis * step)
            .or_else(|| project(guides.pivot - axis * step))
        else {
            continue;
        };
        let dir = sample - pivot;
        if dir.length() < 0.25 {
            continue; // looking straight down this axis
        }
        let dir = dir.normalized() * extent;
        painter.line_segment(
            [pivot - dir, pivot + dir],
            egui::Stroke::new(1.0, AXIS_COLORS[i]),
        );
    }

    // --- dashed pivot-to-mouse line (all operators, like Blender) ----------
    let cur = mouse(guides.cur_mouse);
    painter.extend(egui::Shape::dashed_line(
        &[pivot, cur],
        egui::Stroke::new(1.0, GUIDE_COLOR),
        5.0,
        5.0,
    ));

    // --- vertex-snap target (Blender-style snap circle) --------------------
    if let Some(target) = guides.snap_target {
        if let Some(pos) = project(target) {
            painter.circle_stroke(pos, 8.0, egui::Stroke::new(2.0, GUIDE_COLOR));
            painter.circle_filled(pos, 2.0, GUIDE_COLOR);
        }
    }

    // --- rotation arc -------------------------------------------------------
    if guides.kind == GuideKind::Rotate {
        let start = mouse(guides.start_mouse);
        // faint reference line marking where the drag began
        painter.extend(egui::Shape::dashed_line(
            &[pivot, start],
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(70)),
            5.0,
            5.0,
        ));

        let radius = (cur - pivot).length().max(12.0);
        let a0 = (start.y - pivot.y).atan2(start.x - pivot.x);
        // screen_sweep is CCW-positive with a bottom-left origin; egui's y
        // points down, which mirrors the angle
        let sweep = -guides.screen_sweep;
        let segments = ((sweep.abs() * radius / 4.0).ceil() as usize).clamp(2, 512);
        let points: Vec<egui::Pos2> = (0..=segments)
            .map(|s| {
                let a = a0 + sweep * (s as f32 / segments as f32);
                pivot + radius * egui::Vec2::new(a.cos(), a.sin())
            })
            .collect();

        // translucent pie fill as a triangle fan around the pivot
        let fill = egui::Color32::from_white_alpha(18);
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(pivot, fill);
        for &p in &points {
            mesh.colored_vertex(p, fill);
        }
        for s in 1..points.len() as u32 {
            mesh.add_triangle(0, s, s + 1);
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.5, GUIDE_COLOR),
        ));
    }
}

/// Edit-mode visuals: the object's wireframe (sharp edges), its vertices in
/// vertex mode, and the selected element highlighted in orange.
pub fn draw_edit_mode(
    ctx: &egui::Context,
    camera: &BlenderCamera,
    viewport: Viewport,
    device_pixel_ratio: f32,
    clip: egui::Rect,
    overlay: &crate::edit_mode::EditOverlay,
) {
    use crate::edit_mode::SelectedShape;
    const WIRE: egui::Color32 = egui::Color32::from_rgba_premultiplied(150, 160, 175, 200);
    const VERT: egui::Color32 = egui::Color32::from_rgb(210, 215, 225);
    const SELECTED: egui::Color32 = egui::Color32::from_rgb(255, 170, 64);
    /// Pending loop-cut / bevel geometry (Blender-style preview yellow).
    const PREVIEW: egui::Color32 = egui::Color32::from_rgb(255, 230, 80);

    let painter = ctx.layer_painter(egui::LayerId::background()).with_clip_rect(clip);
    let project = |p: Vec3| to_egui(camera, viewport, device_pixel_ratio, p);

    for &(a, b) in &overlay.edges {
        if let (Some(a), Some(b)) = (project(a), project(b)) {
            painter.line_segment([a, b], egui::Stroke::new(1.0, WIRE));
        }
    }
    // pending loop-cut / bevel preview edges
    for &(a, b) in &overlay.highlight {
        if let (Some(a), Some(b)) = (project(a), project(b)) {
            painter.line_segment([a, b], egui::Stroke::new(2.5, PREVIEW));
        }
    }
    for &v in &overlay.verts {
        if let Some(p) = project(v) {
            painter.circle_filled(p, 2.5, VERT);
        }
    }
    match &overlay.selected {
        Some(SelectedShape::Point(v)) => {
            if let Some(p) = project(*v) {
                painter.circle_filled(p, 5.0, SELECTED);
            }
        }
        Some(SelectedShape::Line(a, b)) => {
            if let (Some(a), Some(b)) = (project(*a), project(*b)) {
                painter.line_segment([a, b], egui::Stroke::new(3.0, SELECTED));
                painter.circle_filled(a, 3.5, SELECTED);
                painter.circle_filled(b, 3.5, SELECTED);
            }
        }
        Some(SelectedShape::Polygon { tris, outline }) => {
            let fill = egui::Color32::from_rgba_premultiplied(120, 80, 30, 90);
            let mut mesh = egui::Mesh::default();
            for tri in tris {
                let (Some(a), Some(b), Some(c)) =
                    (project(tri[0]), project(tri[1]), project(tri[2]))
                else {
                    continue;
                };
                let base = mesh.vertices.len() as u32;
                mesh.colored_vertex(a, fill);
                mesh.colored_vertex(b, fill);
                mesh.colored_vertex(c, fill);
                mesh.add_triangle(base, base + 1, base + 2);
            }
            painter.add(egui::Shape::mesh(mesh));
            for &(a, b) in outline {
                if let (Some(a), Some(b)) = (project(a), project(b)) {
                    painter.line_segment([a, b], egui::Stroke::new(2.0, SELECTED));
                }
            }
        }
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
/// Clip rectangle for viewport overlays: the window minus the UI chrome,
/// so overlay drawings never spill over the menu bar, sidebar or status bar.
pub fn viewport_clip(ctx: &egui::Context, layout: &crate::ui::UiLayout) -> egui::Rect {
    let screen = ctx.content_rect();
    egui::Rect::from_min_max(
        egui::pos2(screen.left(), screen.top() + layout.top_offset),
        egui::pos2(
            screen.right() - layout.right_offset,
            screen.bottom() - layout.bottom_offset,
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    ctx: &egui::Context,
    camera: &BlenderCamera,
    viewport: Viewport,
    device_pixel_ratio: f32,
    clip: egui::Rect,
    scene: &Scene,
    selection: &Selection,
    measure: &MeasureTool,
    calibrate: &CalibrateTool,
    unit: Unit,
) {
    let painter = ctx.layer_painter(egui::LayerId::background()).with_clip_rect(clip);
    let project = |p: Vec3| to_egui(camera, viewport, device_pixel_ratio, p);

    // --- calibration picks (scale-from-2-points) ---------------------------
    for (i, point) in calibrate.points.iter().enumerate() {
        if let Some(pos) = project(*point) {
            painter.circle_stroke(pos, 6.0, egui::Stroke::new(2.0, DIM_COLOR));
            painter.circle_filled(pos, 2.0, DIM_COLOR);
            text_with_bg(
                &painter,
                pos + egui::vec2(0.0, -10.0),
                egui::Align2::CENTER_BOTTOM,
                &format!("{}", i + 1),
                11.0,
                DIM_COLOR,
            );
        }
    }
    if calibrate.points.len() == 2 {
        if let (Some(a), Some(b)) = (project(calibrate.points[0]), project(calibrate.points[1])) {
            painter.line_segment([a, b], egui::Stroke::new(1.5, DIM_COLOR));
        }
    }

    // --- measurements ------------------------------------------------------
    for m in scene.measurements() {
        let (Some(a), Some(b)) = (project(m.a), project(m.b)) else {
            continue;
        };
        painter.line_segment([a, b], egui::Stroke::new(1.5, MEASURE_COLOR));
        painter.circle_filled(a, 3.0, MEASURE_COLOR);
        painter.circle_filled(b, 3.0, MEASURE_COLOR);
        let mid = egui::Pos2::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
        text_with_bg(
            &painter,
            mid + egui::vec2(0.0, -6.0),
            egui::Align2::CENTER_BOTTOM,
            &unit.format(m.length()),
            12.0,
            MEASURE_COLOR,
        );
    }

    // ruler in progress: mark the first point
    if let Some(first) = measure.first {
        if let Some(a) = project(first) {
            painter.circle_stroke(a, 5.0, egui::Stroke::new(1.5, MEASURE_COLOR));
        }
    }

    // --- selected reference image (viewport click) ---------------------------
    if let Some(image_id) = selection.image() {
        if let Some(image) = scene.reference_images().iter().find(|r| r.id == image_id) {
            const IMAGE_SEL: egui::Color32 = egui::Color32::from_rgb(255, 170, 64);
            let corners = image.corners();
            let screen: Vec<_> = corners.iter().filter_map(|&c| project(c)).collect();
            if screen.len() == 4 {
                painter.add(egui::Shape::closed_line(
                    screen.clone(),
                    egui::Stroke::new(2.0, IMAGE_SEL),
                ));
                let top = screen
                    .iter()
                    .copied()
                    .reduce(|a, b| if a.y <= b.y { a } else { b })
                    .unwrap();
                text_with_bg(
                    &painter,
                    top + egui::vec2(0.0, -8.0),
                    egui::Align2::CENTER_BOTTOM,
                    &format!("{}  ·  G move · End to ground", image.name),
                    11.0,
                    IMAGE_SEL,
                );
            }
        }
    }

    // --- pivot & anchor markers (active object, when set) -------------------
    const PIVOT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 170, 64);
    const ANCHOR_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 205, 225);
    if let Some(active) = selection.active() {
        if let Some(object) = scene.object(active) {
            if object.pivot != Vec3::ZERO {
                if let Some(pos) = project(scene.world_pivot(active)) {
                    // crosshair circle, Blender-3D-cursor-style
                    painter.circle_stroke(pos, 6.0, egui::Stroke::new(1.5, PIVOT_COLOR));
                    for d in [egui::vec2(9.0, 0.0), egui::vec2(0.0, 9.0)] {
                        painter.line_segment(
                            [pos - d, pos + d],
                            egui::Stroke::new(1.0, PIVOT_COLOR),
                        );
                    }
                    text_with_bg(
                        &painter,
                        pos + egui::vec2(10.0, -4.0),
                        egui::Align2::LEFT_BOTTOM,
                        "pivot",
                        10.0,
                        PIVOT_COLOR,
                    );
                }
            }
            if object.anchor != Vec3::ZERO {
                if let Some(pos) = project(scene.world_anchor(active)) {
                    // diamond
                    let r = 6.0;
                    let points = vec![
                        pos + egui::vec2(0.0, -r),
                        pos + egui::vec2(r, 0.0),
                        pos + egui::vec2(0.0, r),
                        pos + egui::vec2(-r, 0.0),
                    ];
                    painter.add(egui::Shape::closed_line(
                        points,
                        egui::Stroke::new(1.5, ANCHOR_COLOR),
                    ));
                    painter.circle_filled(pos, 1.5, ANCHOR_COLOR);
                    text_with_bg(
                        &painter,
                        pos + egui::vec2(10.0, 8.0),
                        egui::Align2::LEFT_TOP,
                        "anchor",
                        10.0,
                        ANCHOR_COLOR,
                    );
                }
            }
        }
    }

    // --- object adornments --------------------------------------------------
    let worlds = scene.world_transforms(); // one O(N) pass for the frame
    for object in scene.objects() {
        if !object.visible || (!object.show_label && !object.show_dimensions) {
            continue;
        }
        let world = worlds.get(&object.id).copied().unwrap_or(object.transform);
        let top = world.location
            + Vec3::Z * (object.bounding_radius() * world.scale.z.abs() + 0.15);
        let Some(anchor) = project(top) else { continue };
        // off-screen labels: skip the text layout and tessellation entirely
        // (the margin generously covers a label's own extent)
        if !clip.expand(120.0).contains(anchor) {
            continue;
        }

        let mut y = anchor.y;
        if object.show_label {
            text_with_bg(
                &painter,
                egui::Pos2::new(anchor.x, y),
                egui::Align2::CENTER_BOTTOM,
                &object.name,
                13.0,
                LABEL_COLOR,
            );
            y -= 16.0;
        }
        if object.show_dimensions {
            let d = object.primitive.dimensions() * world.scale.abs() * unit.per_meter();
            text_with_bg(
                &painter,
                egui::Pos2::new(anchor.x, y),
                egui::Align2::CENTER_BOTTOM,
                &format!(
                    "{:.p$} × {:.p$} × {:.p$} {}",
                    d.x,
                    d.y,
                    d.z,
                    unit.suffix(),
                    p = unit.decimals().min(2),
                ),
                11.0,
                DIM_COLOR,
            );
        }
    }
}
