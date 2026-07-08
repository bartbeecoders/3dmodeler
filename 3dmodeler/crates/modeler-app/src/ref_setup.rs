//! Smart reference setup: load a whole drawing set at once (elevations +
//! floor plans), drag each picture onto the view slot it shows, and place
//! them all around the origin in one go — correct plane, correct offset,
//! mirrored where needed, and consistently scaled.
//!
//! Scaling assumes the drawings share one print scale (true for normal plan
//! sets): the building width typed in the dialog fixes meters-per-pixel via
//! the front elevation, and every other image inherits that scale from its
//! own pixel size. Individual images can still be re-calibrated afterwards.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use modeler_core::glam::Vec3;
use modeler_core::{ImagePlane, ReferenceImage, Scene};
use three_d::egui;

use crate::ref_image;
use crate::settings::Settings;

// ------------------------------------------------------------------ slots --

/// A view slot in the setup dialog. Elevations stand on the ground plane
/// around the model; `Floor(i)` is a horizontal plan at storey `i`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Front,
    Back,
    Left,
    Right,
    Top,
    Floor(u32),
}

impl Slot {
    fn label(self) -> String {
        match self {
            Slot::Front => "Front".into(),
            Slot::Back => "Back".into(),
            Slot::Left => "Left".into(),
            Slot::Right => "Right".into(),
            Slot::Top => "Top".into(),
            Slot::Floor(0) => "Ground floor".into(),
            Slot::Floor(1) => "1st floor".into(),
            Slot::Floor(2) => "2nd floor".into(),
            Slot::Floor(3) => "3rd floor".into(),
            Slot::Floor(n) => format!("{n}th floor"),
        }
    }

    fn hover(self) -> &'static str {
        match self {
            Slot::Front => "Elevation seen from the front (-Y), placed behind the model",
            Slot::Back => "Elevation seen from the back (+Y), mirrored automatically",
            Slot::Left => "Elevation seen from the left (-X), mirrored automatically",
            Slot::Right => "Elevation seen from the right (+X)",
            Slot::Top => "Roof / top view, placed at roof height",
            Slot::Floor(_) => "Floor plan, placed flat at this storey's height",
        }
    }

    /// Scale-anchor priority: lower ranks are trusted first when deriving
    /// meters-per-pixel from the typed building width.
    fn anchor_rank(self) -> u32 {
        match self {
            Slot::Front => 0,
            Slot::Back => 1,
            Slot::Right => 2,
            Slot::Left => 3,
            Slot::Top => 4,
            Slot::Floor(n) => 5 + n,
        }
    }

    fn is_elevation(self) -> bool {
        matches!(self, Slot::Front | Slot::Back | Slot::Left | Slot::Right)
    }
}

// -------------------------------------------------------------- placement --

/// An image loaded into the dialog's tray.
pub struct SetupImage {
    pub name: String,
    pub bytes: Vec<u8>,
    /// Source pixel size (width, height).
    pub px: (u32, u32),
}

impl SetupImage {
    fn aspect(&self) -> f32 {
        self.px.1.max(1) as f32 / self.px.0.max(1) as f32
    }
}

pub struct PlacementParams {
    /// Real-world width of the building on the front elevation, meters.
    pub building_width_m: f32,
    /// Storey-to-storey height, meters (floor plan Z offsets).
    pub floor_height_m: f32,
    pub opacity: f32,
}

/// Lift plans slightly off the geometry they sit on (grid at z = 0, slab
/// tops at storey heights) to avoid z-fighting.
const PLAN_LIFT_M: f32 = 0.01;

/// Turn slot assignments into ready-to-add reference images: one shared
/// meters-per-pixel scale, elevations upright behind the model on their
/// axis plane (mirrored where the view direction demands it), plans flat
/// at their storey height.
pub fn build_placements(
    assigned: &[(Slot, &SetupImage)],
    params: &PlacementParams,
) -> Vec<ReferenceImage> {
    if assigned.is_empty() {
        return Vec::new();
    }
    let anchor = assigned
        .iter()
        .min_by_key(|(slot, _)| slot.anchor_rank())
        .expect("non-empty");
    // meters per source pixel, shared by the whole drawing set
    let mpp = params.building_width_m.max(0.01) / anchor.1.px.0.max(1) as f32;
    let width_of = |img: &SetupImage| mpp * img.px.0.max(1) as f32;
    let height_of = |img: &SetupImage| width_of(img) * img.aspect();

    let find = |wanted: &[Slot]| -> Option<&SetupImage> {
        wanted.iter().find_map(|w| {
            assigned
                .iter()
                .find(|(slot, _)| slot == w)
                .map(|(_, img)| *img)
        })
    };
    let plan = find(&[Slot::Floor(0), Slot::Top]).or_else(|| {
        assigned
            .iter()
            .find(|(s, _)| matches!(s, Slot::Floor(_)))
            .map(|(_, img)| *img)
    });

    // building extents: X span from front/back, Y span from left/right,
    // falling back to the plan's spans, then to the typed width
    let x_extent = find(&[Slot::Front, Slot::Back])
        .or(plan)
        .map(&width_of)
        .unwrap_or(params.building_width_m.max(0.01));
    let y_extent = find(&[Slot::Left, Slot::Right])
        .map(&width_of)
        .or_else(|| plan.map(&height_of))
        .unwrap_or(x_extent);
    let roof_z = assigned
        .iter()
        .filter(|(slot, _)| slot.is_elevation())
        .map(|(_, img)| height_of(img))
        .fold(f32::NEG_INFINITY, f32::max)
        .max(
            params.floor_height_m
                * assigned
                    .iter()
                    .filter_map(|(s, _)| match s {
                        Slot::Floor(n) => Some(n + 1),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(1) as f32,
        );

    assigned
        .iter()
        .map(|(slot, img)| {
            let (w, h) = (width_of(img), height_of(img));
            // elevations sit behind the model as seen from their view
            // direction, bottom edge on the ground
            let (plane, location, flip_h) = match slot {
                Slot::Front => (ImagePlane::Y, Vec3::new(0.0, 0.5 * y_extent, 0.5 * h), false),
                Slot::Back => (ImagePlane::Y, Vec3::new(0.0, -0.5 * y_extent, 0.5 * h), true),
                Slot::Right => (ImagePlane::X, Vec3::new(-0.5 * x_extent, 0.0, 0.5 * h), false),
                Slot::Left => (ImagePlane::X, Vec3::new(0.5 * x_extent, 0.0, 0.5 * h), true),
                Slot::Top => (ImagePlane::Z, Vec3::new(0.0, 0.0, roof_z + PLAN_LIFT_M), false),
                Slot::Floor(n) => (
                    ImagePlane::Z,
                    Vec3::new(0.0, 0.0, *n as f32 * params.floor_height_m + PLAN_LIFT_M),
                    false,
                ),
            };
            ReferenceImage {
                id: 0, // assigned by the scene
                name: slot.label(),
                plane,
                location,
                rotation_deg: 0.0,
                width_m: w,
                aspect: img.aspect(),
                opacity: params.opacity.clamp(0.0, 1.0),
                visible: true,
                flip_h,
                data_base64: BASE64.encode(&img.bytes),
            }
        })
        .collect()
}

// ------------------------------------------------------------------ dialog --

/// Drag-and-drop payload: index into the dialog's image tray. A dedicated
/// type so the drop zones ignore payloads from other panels.
struct DragImage(usize);

pub struct RefSetupDialog {
    pub open: bool,
    images: Vec<SetupImage>,
    /// Thumbnail textures, lazily decoded, parallel to `images`.
    textures: Vec<Option<egui::TextureHandle>>,
    /// slot -> tray index; a Vec to keep placement order deterministic.
    assigned: Vec<(Slot, usize)>,
    /// Tray image armed by clicking (click a slot to assign it) — the
    /// keyboard/touchpad fallback for drag-and-drop.
    selected: Option<usize>,
    /// Floor-plan slots shown above the ground floor.
    extra_floors: u32,
    building_width_m: f32,
    floor_height_m: f32,
    opacity: f32,
}

const THUMB: egui::Vec2 = egui::Vec2::new(96.0, 72.0);
const SLOT_SIZE: egui::Vec2 = egui::Vec2::new(112.0, 118.0);

impl RefSetupDialog {
    pub fn new() -> Self {
        Self {
            open: false,
            images: Vec::new(),
            textures: Vec::new(),
            assigned: Vec::new(),
            selected: None,
            extra_floors: 1,
            building_width_m: 10.0,
            floor_height_m: 3.0,
            opacity: 0.5,
        }
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    fn reset(&mut self) {
        self.images.clear();
        self.textures.clear();
        self.assigned.clear();
        self.selected = None;
    }

    fn add_image(&mut self, name: String, bytes: Vec<u8>) {
        let Some(px) = image::ImageReader::new(std::io::Cursor::new(&bytes[..]))
            .with_guessed_format()
            .ok()
            .and_then(|r| r.into_dimensions().ok())
        else {
            return; // not a decodable image — ignore the file
        };
        self.images.push(SetupImage { name, bytes, px });
        self.textures.push(None);
    }

    fn assign(&mut self, slot: Slot, index: usize) {
        self.assigned.retain(|(s, i)| *s != slot && *i != index);
        self.assigned.push((slot, index));
        self.selected = None;
    }

    fn assignment(&self, slot: Slot) -> Option<usize> {
        self.assigned
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, i)| *i)
    }

    fn slot_of(&self, index: usize) -> Option<Slot> {
        self.assigned
            .iter()
            .find(|(_, i)| *i == index)
            .map(|(s, _)| *s)
    }

    /// Decode (downscaled) thumbnail texture for tray image `index`.
    fn texture_for(&mut self, ctx: &egui::Context, index: usize) -> Option<egui::TextureHandle> {
        if let Some(texture) = &self.textures[index] {
            return Some(texture.clone());
        }
        let decoded = image::load_from_memory(&self.images[index].bytes).ok()?;
        let thumb = decoded.thumbnail(256, 256).to_rgba8();
        let (w, h) = thumb.dimensions();
        let color_image =
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], thumb.as_raw());
        let texture = ctx.load_texture(
            format!("ref-setup-{index}"),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        self.textures[index] = Some(texture.clone());
        Some(texture)
    }

    /// Draw the dialog; returns a status message when images were placed.
    pub fn window(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        settings: &Settings,
    ) -> Option<String> {
        // poll before the open-check: files dropped from the OS onto the
        // window arrive here too, and should open the dialog by themselves
        let arrived = ref_image::poll_setup_images();
        if !arrived.is_empty() {
            self.open = true;
        }
        for (name, bytes) in arrived {
            self.add_image(name, bytes);
        }
        if !self.open {
            return None;
        }

        let unit = settings.unit;
        let mut open = self.open;
        let mut accepted = false;
        let mut cancelled = false;

        egui::Window::new("Reference Setup")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Add Images…").clicked() {
                        ref_image::request_setup_images();
                    }
                    ui.label(
                        egui::RichText::new(
                            "Load your drawing set — or drop image files straight from your file \
                             manager — then drag each picture onto the view it shows (or click a \
                             picture, then a slot).",
                        )
                        .weak(),
                    );
                });
                ui.add_space(4.0);
                self.tray(ui, ctx);
                ui.separator();

                let elevations = [Slot::Front, Slot::Back, Slot::Left, Slot::Right, Slot::Top];
                ui.horizontal(|ui| {
                    for slot in elevations {
                        self.slot_zone(ui, ctx, slot);
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for n in 0..=self.extra_floors {
                        self.slot_zone(ui, ctx, Slot::Floor(n));
                    }
                    ui.vertical(|ui| {
                        if ui
                            .small_button("+ floor")
                            .on_hover_text("Add a floor-plan slot for the next storey")
                            .clicked()
                        {
                            self.extra_floors += 1;
                        }
                        let top = Slot::Floor(self.extra_floors);
                        if self.extra_floors > 0
                            && self.assignment(top).is_none()
                            && ui.small_button("− floor").clicked()
                        {
                            self.extra_floors -= 1;
                        }
                    });
                });
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Building width");
                    let mut width = unit.from_meters(self.building_width_m);
                    if ui
                        .add(
                            egui::DragValue::new(&mut width)
                                .speed(0.1 * unit.per_meter() as f64)
                                .range(unit.from_meters(0.1)..=unit.from_meters(1000.0))
                                .suffix(format!(" {}", unit.suffix())),
                        )
                        .on_hover_text(
                            "Real width of the building on the front elevation — every \
                             drawing is scaled from this, assuming the set shares one \
                             print scale (recalibrate individual images later if needed)",
                        )
                        .changed()
                    {
                        self.building_width_m = unit.to_meters(width).max(0.1);
                    }
                    ui.add_space(12.0);
                    ui.label("Floor height");
                    let mut floor_h = unit.from_meters(self.floor_height_m);
                    if ui
                        .add(
                            egui::DragValue::new(&mut floor_h)
                                .speed(0.02 * unit.per_meter() as f64)
                                .range(unit.from_meters(0.5)..=unit.from_meters(10.0))
                                .suffix(format!(" {}", unit.suffix())),
                        )
                        .on_hover_text("Storey-to-storey height: sets each floor plan's Z level")
                        .changed()
                    {
                        self.floor_height_m = unit.to_meters(floor_h).max(0.5);
                    }
                    ui.add_space(12.0);
                    ui.label("Opacity");
                    ui.add(egui::Slider::new(&mut self.opacity, 0.05..=1.0));
                });
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    let n = self.assigned.len();
                    if ui
                        .add_enabled(
                            n > 0,
                            egui::Button::new(format!("Place {n} image{}", if n == 1 { "" } else { "s" })),
                        )
                        .clicked()
                    {
                        accepted = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            });

        let mut status = None;
        if accepted {
            let assigned: Vec<(Slot, &SetupImage)> = self
                .assigned
                .iter()
                .map(|(slot, i)| (*slot, &self.images[*i]))
                .collect();
            let params = PlacementParams {
                building_width_m: self.building_width_m,
                floor_height_m: self.floor_height_m,
                opacity: self.opacity,
            };
            let placements = build_placements(&assigned, &params);
            let n = placements.len();
            for image in placements {
                scene.add_reference_image(image);
            }
            status = Some(format!("placed {n} reference image{}", if n == 1 { "" } else { "s" }));
        }
        if accepted || cancelled || !open {
            self.reset();
            self.open = false;
        }
        status
    }

    /// Unassigned images: drag sources (and click-to-arm targets).
    fn tray(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let unassigned: Vec<usize> = (0..self.images.len())
            .filter(|i| self.slot_of(*i).is_none())
            .collect();
        if unassigned.is_empty() {
            let text = if self.images.is_empty() {
                "No images loaded yet."
            } else {
                "All images assigned."
            };
            ui.label(egui::RichText::new(text).weak());
            return;
        }
        ui.horizontal_wrapped(|ui| {
            for index in unassigned {
                let texture = self.texture_for(ctx, index);
                let name = self.images[index].name.clone();
                let armed = self.selected == Some(index);
                let id = egui::Id::new(("ref-setup-drag", index));
                let response = ui
                    .dnd_drag_source(id, DragImage(index), |ui| {
                        let stroke = if armed {
                            egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
                        } else {
                            ui.visuals().widgets.inactive.bg_stroke
                        };
                        egui::Frame::group(ui.style()).stroke(stroke).show(ui, |ui| {
                            ui.set_width(THUMB.x);
                            ui.vertical_centered(|ui| {
                                thumbnail(ui, texture.as_ref(), THUMB);
                                ui.label(egui::RichText::new(name).size(10.0).weak());
                            });
                        });
                    })
                    .response;
                // drag sources only sense drags; clicks (to arm the image for
                // click-assign) need their own interact region on top
                let click = ui.interact(response.rect, id.with("click"), egui::Sense::click());
                if click.clicked() {
                    self.selected = if armed { None } else { Some(index) };
                }
            }
        });
    }

    /// One drop zone: shows the slot label and the assigned thumbnail.
    fn slot_zone(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, slot: Slot) {
        let assigned = self.assignment(slot);
        let texture = assigned.and_then(|i| self.texture_for(ctx, i));
        let frame = egui::Frame::group(ui.style()).inner_margin(4.0);
        let mut clear = false;
        let (zone, payload) = ui.dnd_drop_zone::<DragImage, ()>(frame, |ui| {
            ui.set_min_size(SLOT_SIZE);
            ui.set_max_width(SLOT_SIZE.x);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(slot.label()).size(11.0).strong());
                match (assigned, texture) {
                    (Some(index), texture) => {
                        // assigned thumbnails can be dragged on to another slot
                        let id = egui::Id::new(("ref-setup-slot-drag", index));
                        ui.dnd_drag_source(id, DragImage(index), |ui| {
                            thumbnail(ui, texture.as_ref(), THUMB);
                        });
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&self.images[index].name).size(10.0).weak(),
                            );
                            if ui.small_button("✖").on_hover_text("Clear this slot").clicked() {
                                clear = true;
                            }
                        });
                    }
                    (None, _) => {
                        ui.add_space(0.5 * THUMB.y - 8.0);
                        ui.label(egui::RichText::new("drop here").weak().size(10.0));
                        ui.add_space(0.5 * THUMB.y - 8.0);
                    }
                }
            });
        });
        let zone = zone.response.on_hover_text(slot.hover());
        if let Some(dropped) = payload {
            self.assign(slot, dropped.0);
        } else if zone.clicked() {
            if let Some(index) = self.selected {
                self.assign(slot, index);
            }
        }
        if clear {
            self.assigned.retain(|(s, _)| *s != slot);
        }
    }
}

/// Draw a texture fitted inside `bounds`, keeping the image aspect.
fn thumbnail(ui: &mut egui::Ui, texture: Option<&egui::TextureHandle>, bounds: egui::Vec2) {
    let Some(texture) = texture else {
        ui.allocate_space(bounds);
        return;
    };
    let size = texture.size_vec2();
    let scale = (bounds.x / size.x).min(bounds.y / size.y).min(1.0);
    ui.add(
        egui::Image::new(egui::load::SizedTexture::new(texture.id(), size * scale))
            .fit_to_exact_size(size * scale),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(name: &str, w: u32, h: u32) -> SetupImage {
        SetupImage {
            name: name.into(),
            bytes: vec![0; 4],
            px: (w, h),
        }
    }

    fn params() -> PlacementParams {
        PlacementParams {
            building_width_m: 10.0,
            floor_height_m: 3.0,
            opacity: 0.5,
        }
    }

    #[test]
    fn shared_scale_and_planes() {
        // front 1000 px wide = 10 m -> 0.01 m/px; side 800 px -> 8 m deep
        let front = img("front", 1000, 600);
        let side = img("right", 800, 600);
        let plan = img("plan", 1000, 800);
        let assigned = vec![
            (Slot::Front, &front),
            (Slot::Right, &side),
            (Slot::Floor(0), &plan),
        ];
        let placed = build_placements(&assigned, &params());
        assert_eq!(placed.len(), 3);

        let front = &placed[0];
        assert_eq!(front.plane, ImagePlane::Y);
        assert!((front.width_m - 10.0).abs() < 1e-5);
        // behind the model: +Y at half the depth (side image is 8 m wide),
        // bottom edge on the ground (center at half its 6 m height)
        assert!((front.location.y - 4.0).abs() < 1e-5);
        assert!((front.location.z - 3.0).abs() < 1e-5);
        assert!(!front.flip_h);

        let right = &placed[1];
        assert_eq!(right.plane, ImagePlane::X);
        assert!((right.width_m - 8.0).abs() < 1e-5);
        assert!((right.location.x + 5.0).abs() < 1e-5, "behind as seen from +X");
        assert!(!right.flip_h);

        let plan = &placed[2];
        assert_eq!(plan.plane, ImagePlane::Z);
        assert!((plan.location.z - PLAN_LIFT_M).abs() < 1e-6);
        assert!((plan.width_m - 10.0).abs() < 1e-5);
    }

    #[test]
    fn back_and_left_are_mirrored() {
        let front = img("front", 1000, 600);
        let back = img("back", 1000, 600);
        let left = img("left", 800, 600);
        let assigned = vec![(Slot::Front, &front), (Slot::Back, &back), (Slot::Left, &left)];
        let placed = build_placements(&assigned, &params());
        assert!(!placed[0].flip_h);
        assert!(placed[1].flip_h);
        assert!(placed[2].flip_h);
        // back sits opposite the front; left opposite the right side
        assert!((placed[1].location.y + placed[0].location.y).abs() < 1e-5);
        assert!(placed[2].location.x > 0.0);
    }

    #[test]
    fn floors_stack_and_top_sits_on_the_roof() {
        let front = img("front", 1000, 600); // 10 m x 6 m elevation
        let g = img("ground", 1000, 800);
        let first = img("first", 1000, 800);
        let top = img("roof", 1000, 800);
        let assigned = vec![
            (Slot::Front, &front),
            (Slot::Floor(0), &g),
            (Slot::Floor(1), &first),
            (Slot::Top, &top),
        ];
        let placed = build_placements(&assigned, &params());
        let z_of = |name: &str| {
            placed
                .iter()
                .find(|p| p.name.to_lowercase().contains(name))
                .unwrap()
                .location
                .z
        };
        assert!((z_of("ground") - PLAN_LIFT_M).abs() < 1e-6);
        assert!((z_of("1st") - (3.0 + PLAN_LIFT_M)).abs() < 1e-5);
        // roof at the elevation height (6 m), not the floor-count height
        assert!((z_of("top") - (6.0 + PLAN_LIFT_M)).abs() < 1e-5);
    }

    #[test]
    fn plan_only_falls_back_to_plan_extents() {
        let plan = img("plan", 500, 1000); // 10 m wide, 20 m deep
        let assigned = vec![(Slot::Floor(0), &plan)];
        let placed = build_placements(&assigned, &params());
        assert_eq!(placed.len(), 1);
        assert!((placed[0].width_m - 10.0).abs() < 1e-5);
        assert!((placed[0].aspect - 2.0).abs() < 1e-5);
    }

    #[test]
    fn empty_assignment_places_nothing() {
        assert!(build_placements(&[], &params()).is_empty());
    }
}
