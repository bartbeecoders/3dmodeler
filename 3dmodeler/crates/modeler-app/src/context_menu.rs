//! Right-click context menu in the viewport, as a pie / wheel menu
//! (see pie.rs).
//!
//! Object mode: right-clicking an object selects it (unless it is already
//! part of the selection) and opens the wheel at the cursor — pivot/anchor
//! at the exact clicked surface point plus the common object operations.
//! On walls the west/northwest slots become "add door / window here" (their
//! dimensions and materials are edited in the sidebar). Edit mode:
//! right-clicking a vertex/edge/face offers "set as pivot / anchor" (same
//! as the P/A keys).
//!
//! The object wheel is multi-level:
//! - **Break** → Bricks / Balls (shatter into dynamic particles)
//! - **Set material** → Color / Roughness / Metallic (opens a small editor)
//! Esc / RMB on a sub-menu goes back to the root; on the root they dismiss.
//!
//! main.rs decides what was hit (physics ray in object mode, element pick in
//! edit mode) and calls `open`; the wheel is drawn from UiState::draw. Like
//! the Shift+A wheel, clicks are consumed in `handle_events` (runs after the
//! egui pass) and committed on the next `ui` call via `pending_click`.

use crate::library::LibraryPanel;
use crate::modal::{self, ModalTransform};
use crate::object_ops::{self, BreakKind};
use crate::pie::{self, PieIcon, PieSlot};
use crate::selection::Selection;
use modeler_core::glam::Vec3;
use modeler_core::{Material, ObjectId, Primitive, Scene, WallCutout};
use three_d::egui;
use three_d::{Event, Key, MouseButton};

#[derive(Clone, Copy)]
pub enum Target {
    /// An object, with the clicked surface point in its LOCAL space.
    Object { id: ObjectId, hit_local: Vec3 },
    /// An edit-mode element (vertex/edge/face) and its local point.
    Element { id: ObjectId, point: Vec3, label: &'static str },
}

/// Which ring of the object pie is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuLevel {
    /// Main object operations.
    Root,
    /// Break → Bricks / Balls.
    Break,
    /// Set material → Color / Roughness / Metallic.
    Material,
}

/// Material property being edited in the post-pie popup.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MaterialField {
    Color,
    Roughness,
    Metallic,
}

/// Floating editor opened from Set material → …
struct MaterialEditor {
    id: ObjectId,
    field: MaterialField,
    /// Working values (all three kept so the object stays consistent).
    material: Material,
    /// Screen position for the popup (where the pie was).
    pos: egui::Pos2,
}

/// What the westward slots offer: wall openings, brick→wall rebuild or
/// group/attach actions. Optional trailing slots are recorded by index so
/// execute does not depend on how many extras were pushed.
enum WheelKind {
    Object {
        wall: Option<(f32, f32)>,
        rebuild: Option<u64>,
        apply_slot: Option<usize>,
        break_slot: Option<usize>,
        material_slot: Option<usize>,
    },
    BreakSub,
    MaterialSub,
    Element,
}

pub struct ContextMenu {
    state: Option<(egui::Pos2, Target)>,
    level: MenuLevel,
    /// Popup after choosing Color / Roughness / Metallic.
    material_edit: Option<MaterialEditor>,
    /// Guards event handling on the frame the menu opened (the opening RMB
    /// press is already in this frame's event list, marked handled).
    just_opened: bool,
    /// LMB arrived in `handle_events`; commit on the next `ui` pass.
    pending_click: bool,
    /// 0 → 1 scale-in animation (owned here, rendered by pie::draw).
    anim: f32,
}

impl ContextMenu {
    pub fn new() -> Self {
        Self {
            state: None,
            level: MenuLevel::Root,
            material_edit: None,
            just_opened: false,
            pending_click: false,
            anim: 0.0,
        }
    }

    pub fn open(&mut self, pos: egui::Pos2, target: Target) {
        self.state = Some((pos, target));
        self.level = MenuLevel::Root;
        self.material_edit = None;
        self.just_opened = true;
        self.pending_click = false;
        self.anim = 0.0;
    }

    pub fn close(&mut self) {
        self.state = None;
        self.level = MenuLevel::Root;
        self.material_edit = None;
        self.pending_click = false;
    }

    /// True while the pie or a material property popup is up (blocks Tab for
    /// edit mode, etc.).
    pub fn is_open(&self) -> bool {
        self.state.is_some() || self.material_edit.is_some()
    }

    fn go_back_or_close(&mut self) {
        match self.level {
            MenuLevel::Root => self.close(),
            MenuLevel::Break | MenuLevel::Material => {
                self.level = MenuLevel::Root;
                self.anim = 0.0;
                self.pending_click = false;
            }
        }
    }

    /// Consume clicks/Esc while the wheel is open so a commit click never
    /// falls through to viewport picking. Runs after the egui pass and
    /// after the RMB opener (see main.rs), hence the `just_opened` guard.
    pub fn handle_events(&mut self, events: &mut [Event]) {
        // Material popup is normal egui UI — only Esc dismisses it here.
        if self.material_edit.is_some() {
            for event in events.iter_mut() {
                if let Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } = event
                {
                    if !*handled {
                        self.material_edit = None;
                        *handled = true;
                    }
                }
            }
            return;
        }

        if self.state.is_none() || self.just_opened {
            return;
        }
        for event in events.iter_mut() {
            match event {
                Event::KeyPress { kind: Key::Escape, handled, .. } if !*handled => {
                    self.go_back_or_close();
                    *handled = true;
                }
                Event::MousePress { button, handled, .. } => {
                    if *handled {
                        // egui took it (menu bar, sidebar…): just dismiss
                        self.close();
                    } else {
                        *handled = true;
                        if *button == MouseButton::Left {
                            self.pending_click = true;
                        } else {
                            // RMB / MMB: back one level, or dismiss at root
                            self.go_back_or_close();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Draw the wheel or material popup; returns a status-bar message when
    /// an action ran.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        modal: &mut ModalTransform,
        library_panel: &mut LibraryPanel,
        break_dialog: &mut Option<(BreakKind, i32)>,
    ) -> Option<String> {
        // Material property popup (pie already dismissed).
        if self.material_edit.is_some() {
            return self.draw_material_editor(ctx, scene);
        }

        let Some((pos, target)) = self.state else { return None };
        let mut status = None;

        // build the slot ring for the current target + menu level
        let (kind, slots, hub) = match (self.level, target) {
            (MenuLevel::Break, Target::Object { .. }) => {
                let slots = vec![
                    PieSlot::new("Bricks", PieIcon::Bricks),
                    PieSlot::new("Balls", PieIcon::Balls),
                ];
                (WheelKind::BreakSub, slots, "Break".to_string())
            }
            (MenuLevel::Material, Target::Object { .. }) => {
                let slots = vec![
                    PieSlot::new("Color", PieIcon::MaterialColor),
                    PieSlot::new("Roughness", PieIcon::MaterialRoughness),
                    PieSlot::new("Metallic", PieIcon::MaterialMetallic),
                ];
                (WheelKind::MaterialSub, slots, "Material".to_string())
            }
            (_, Target::Object { id, .. }) => {
                let Some(object) = scene.object(id) else {
                    self.close();
                    return None;
                };
                let is_group = object.group;
                let wall = match object.primitive {
                    Primitive::Wall { length, height, .. }
                        if object.edited_mesh.is_none() =>
                    {
                        Some((length, height))
                    }
                    _ => None,
                };
                let rebuild = crate::object_ops::rebuildable_folder(scene, id);
                let multi =
                    selection.selected().len() >= 2 && selection.active().is_some();
                let has_modifiers = !object.modifiers.is_empty();
                let mut slots = vec![
                    PieSlot::new("Duplicate", PieIcon::Duplicate), // N
                    PieSlot::new("Pivot here", PieIcon::Glyph("⌖")), // NE
                    PieSlot::new("Anchor here", PieIcon::Anchor),  // E
                    PieSlot::new("Reset P/A", PieIcon::Glyph("↩")), // SE
                    PieSlot::new("Delete", PieIcon::Glyph("✖")),   // S
                    PieSlot::new("To Library…", PieIcon::Glyph("💾")), // SW
                ];
                if wall.is_some() {
                    slots.push(PieSlot::new("Add door", PieIcon::Door)); // W
                    slots.push(PieSlot::new("Add window", PieIcon::Window)); // NW
                } else if rebuild.is_some() {
                    slots.push(PieSlot::new("Rebuild", PieIcon::Wall)); // W
                    slots.push(PieSlot::new("Attach", PieIcon::Attach).enabled(multi)); // NW
                } else if is_group {
                    slots.push(PieSlot::new("Ungroup", PieIcon::Ungroup)); // W
                    slots.push(PieSlot::new("Attach", PieIcon::Attach).enabled(multi)); // NW
                } else {
                    slots.push(PieSlot::new("Group", PieIcon::Glyph("❐")).enabled(multi)); // W
                    slots.push(PieSlot::new("Attach", PieIcon::Attach).enabled(multi)); // NW
                }
                // Bake the non-destructive stack (same as the sidebar Apply).
                let apply_slot = if has_modifiers {
                    slots.push(PieSlot::new("Apply modifier", PieIcon::Glyph("✓")));
                    Some(slots.len() - 1)
                } else {
                    None
                };
                // Material for solids (lights/empties have no surface material).
                let has_material = !object.primitive.is_light()
                    && !matches!(object.primitive, Primitive::Empty { .. });
                let material_slot = if has_material {
                    slots.push(PieSlot::new("Set material", PieIcon::MaterialColor));
                    Some(slots.len() - 1)
                } else {
                    None
                };
                // Any solid object can shatter into dynamic particles (not ropes).
                let breakable = has_material && !object.primitive.is_rope();
                let break_slot = if breakable {
                    slots.push(PieSlot::new("Break", PieIcon::Bricks));
                    Some(slots.len() - 1)
                } else {
                    None
                };
                (
                    WheelKind::Object {
                        wall,
                        rebuild,
                        apply_slot,
                        break_slot,
                        material_slot,
                    },
                    slots,
                    hub_label(&object.name),
                )
            }
            (_, Target::Element { label, .. }) => {
                let slots = vec![
                    PieSlot::new("Pivot  (P)", PieIcon::Glyph("⌖")), // N
                    PieSlot::new("Anchor  (A)", PieIcon::Anchor),    // S
                ];
                (WheelKind::Element, slots, label.to_string())
            }
        };

        let pie_id = match self.level {
            MenuLevel::Root => "context-pie",
            MenuLevel::Break => "context-pie-break",
            MenuLevel::Material => "context-pie-material",
        };
        let hovered = pie::draw(ctx, pie_id, pos, &hub, &slots, &mut self.anim);

        if self.pending_click {
            self.pending_click = false;
            if let Some(slot) = hovered {
                // Parent slots: open a sub-menu instead of closing.
                if let WheelKind::Object {
                    break_slot,
                    material_slot,
                    ..
                } = kind
                {
                    if break_slot == Some(slot) {
                        self.level = MenuLevel::Break;
                        self.anim = 0.0;
                        self.just_opened = false;
                        return None;
                    }
                    if material_slot == Some(slot) {
                        self.level = MenuLevel::Material;
                        self.anim = 0.0;
                        self.just_opened = false;
                        return None;
                    }
                }
                // Material sub: open the property editor popup.
                if matches!(kind, WheelKind::MaterialSub) {
                    if let Target::Object { id, .. } = target {
                        if let Some(material) = scene.object_material(id) {
                            let field = match slot {
                                0 => MaterialField::Color,
                                1 => MaterialField::Roughness,
                                _ => MaterialField::Metallic,
                            };
                            self.material_edit = Some(MaterialEditor {
                                id,
                                field,
                                material,
                                pos,
                            });
                            self.state = None;
                            self.level = MenuLevel::Root;
                            self.just_opened = false;
                            return None;
                        }
                    }
                }
                status = self.execute(
                    slot,
                    kind,
                    target,
                    scene,
                    selection,
                    modal,
                    library_panel,
                    break_dialog,
                );
                self.close();
            } else {
                // click on empty: dismiss
                self.close();
            }
        }
        self.just_opened = false;
        status
    }

    /// Color / roughness / metallic popup; live-updates the object.
    fn draw_material_editor(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
    ) -> Option<String> {
        // Object may have been deleted while the popup was open.
        let still_there = self
            .material_edit
            .as_ref()
            .is_some_and(|e| scene.object(e.id).is_some());
        if self.material_edit.is_some() && !still_there {
            self.material_edit = None;
            return None;
        }
        let Some(edit) = self.material_edit.as_mut() else {
            return None;
        };

        let title = match edit.field {
            MaterialField::Color => "Base color",
            MaterialField::Roughness => "Roughness",
            MaterialField::Metallic => "Metallic",
        };
        let field = edit.field;
        let id = edit.id;
        let mut open = true;
        let mut changed = false;
        let mut done = false;

        egui::Window::new(title)
            .id(egui::Id::new("context-material-edit"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_pos(edit.pos + egui::vec2(12.0, 12.0))
            .show(ctx, |ui| {
                match field {
                    MaterialField::Color => {
                        ui.horizontal(|ui| {
                            ui.label("Color");
                            if ui
                                .color_edit_button_rgb(&mut edit.material.base_color)
                                .changed()
                            {
                                changed = true;
                            }
                        });
                        // quick swatches for common materials
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("Presets").weak().small());
                        ui.horizontal(|ui| {
                            for (label, rgb) in [
                                ("Gray", [0.8, 0.8, 0.8]),
                                ("White", [0.95, 0.95, 0.95]),
                                ("Red", [0.75, 0.15, 0.12]),
                                ("Brick", [0.55, 0.25, 0.18]),
                                ("Wood", [0.45, 0.32, 0.18]),
                                ("Metal", [0.7, 0.72, 0.75]),
                            ] {
                                let color = egui::Color32::from_rgb(
                                    (rgb[0] * 255.0) as u8,
                                    (rgb[1] * 255.0) as u8,
                                    (rgb[2] * 255.0) as u8,
                                );
                                if ui
                                    .add(
                                        egui::Button::new("")
                                            .fill(color)
                                            .min_size(egui::vec2(22.0, 18.0)),
                                    )
                                    .on_hover_text(label)
                                    .clicked()
                                {
                                    edit.material.base_color = rgb;
                                    changed = true;
                                }
                            }
                        });
                    }
                    MaterialField::Roughness => {
                        if ui
                            .add(
                                egui::Slider::new(&mut edit.material.roughness, 0.0..=1.0)
                                    .text("Roughness"),
                            )
                            .changed()
                        {
                            changed = true;
                        }
                        ui.horizontal(|ui| {
                            for (label, v) in [
                                ("Matte", 1.0),
                                ("0.7", 0.7),
                                ("Satin", 0.4),
                                ("Gloss", 0.15),
                                ("Mirror", 0.0),
                            ] {
                                if ui.small_button(label).clicked() {
                                    edit.material.roughness = v;
                                    changed = true;
                                }
                            }
                        });
                    }
                    MaterialField::Metallic => {
                        if ui
                            .add(
                                egui::Slider::new(&mut edit.material.metallic, 0.0..=1.0)
                                    .text("Metallic"),
                            )
                            .changed()
                        {
                            changed = true;
                        }
                        ui.horizontal(|ui| {
                            for (label, v) in [
                                ("Dielectric", 0.0),
                                ("0.5", 0.5),
                                ("Metal", 1.0),
                            ] {
                                if ui.small_button(label).clicked() {
                                    edit.material.metallic = v;
                                    changed = true;
                                }
                            }
                        });
                    }
                }
                ui.add_space(6.0);
                if ui.button("Done").clicked() {
                    done = true;
                }
            });

        if changed {
            let mut next = scene.object_material(id).unwrap_or(edit.material);
            match field {
                MaterialField::Color => next.base_color = edit.material.base_color,
                MaterialField::Roughness => next.roughness = edit.material.roughness,
                MaterialField::Metallic => next.metallic = edit.material.metallic,
            }
            scene.set_object_material(id, next);
        }

        if done || !open {
            self.material_edit = None;
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn execute(
        &mut self,
        slot: usize,
        kind: WheelKind,
        target: Target,
        scene: &mut Scene,
        selection: &mut Selection,
        modal: &mut ModalTransform,
        library_panel: &mut LibraryPanel,
        break_dialog: &mut Option<(BreakKind, i32)>,
    ) -> Option<String> {
        match (kind, target) {
            (WheelKind::BreakSub, Target::Object { .. }) => {
                let kind = if slot == 0 {
                    BreakKind::Bricks
                } else {
                    BreakKind::Balls
                };
                *break_dialog =
                    Some((kind, crate::object_ops::DEFAULT_PARTICLES as i32));
                None
            }
            (WheelKind::MaterialSub, _) => None, // handled before execute
            (
                WheelKind::Object {
                    wall,
                    rebuild,
                    apply_slot,
                    break_slot: _,
                    material_slot: _,
                },
                Target::Object { id, hit_local },
            ) => {
                let name = scene.object(id).map(|o| o.name.clone()).unwrap_or_default();
                if apply_slot == Some(slot) {
                    return Some(
                        crate::modifiers::apply(scene, id, usize::MAX)
                            .unwrap_or_else(|e| e),
                    );
                }
                match slot {
                    0 => {
                        if modal::duplicate_selection(scene, selection) {
                            modal.begin_grab(scene, selection);
                        }
                        None
                    }
                    1 => {
                        if let Some(object) = scene.object_mut(id) {
                            object.pivot = hit_local;
                        }
                        Some(format!("pivot of '{name}' set"))
                    }
                    2 => {
                        if let Some(object) = scene.object_mut(id) {
                            object.anchor = hit_local;
                        }
                        Some(format!("anchor of '{name}' set"))
                    }
                    3 => {
                        if let Some(object) = scene.object_mut(id) {
                            object.pivot = Vec3::ZERO;
                            object.anchor = Vec3::ZERO;
                        }
                        Some(format!("pivot and anchor of '{name}' reset"))
                    }
                    4 => {
                        object_ops::delete_selected(scene, selection);
                        None
                    }
                    5 => {
                        library_panel.open_create_dialog(scene, selection);
                        None
                    }
                    6 | 7 => match wall {
                        Some((length, height)) => {
                            let (cutout, what) = if slot == 6 {
                                (WallCutout::door(hit_local.x, length, height), "door")
                            } else {
                                (
                                    WallCutout::window(
                                        hit_local.x,
                                        hit_local.z,
                                        length,
                                        height,
                                    ),
                                    "window",
                                )
                            };
                            if let Some(object) = scene.object_mut(id) {
                                object.cutouts.push(cutout);
                                object.mesh_revision += 1;
                            }
                            Some(format!("{what} added to '{name}'"))
                        }
                        None if slot == 6 => {
                            if let Some(folder) = rebuild {
                                return object_ops::rebuild_wall_from_folder(
                                    scene, folder,
                                )
                                .map(|wall| {
                                    selection.set(vec![wall], Some(wall));
                                    let wall_name = scene
                                        .object(wall)
                                        .map(|o| o.name.clone())
                                        .unwrap_or_default();
                                    format!("bricks rebuilt into '{wall_name}'")
                                });
                            }
                            let is_group =
                                scene.object(id).is_some_and(|o| o.group);
                            if is_group {
                                if let Some(object) = scene.object_mut(id) {
                                    object.group = false;
                                }
                                Some(format!(
                                    "ungrouped '{name}' — parts are now selectable"
                                ))
                            } else {
                                crate::ui::group_selection(scene, selection);
                                Some("grouped the selection".to_string())
                            }
                        }
                        None => {
                            if let Some(active) = selection.active() {
                                for id in selection.selected().to_vec() {
                                    if id != active {
                                        scene.attach(id, active, None);
                                    }
                                }
                            }
                            None
                        }
                    },
                    _ => None,
                }
            }
            (WheelKind::Element, Target::Element { id, point, label }) => {
                let what = label.to_lowercase();
                if slot == 0 {
                    if let Some(object) = scene.object_mut(id) {
                        object.pivot = point;
                    }
                    Some(format!("pivot set to the selected {what}"))
                } else {
                    if let Some(object) = scene.object_mut(id) {
                        object.anchor = point;
                    }
                    Some(format!("anchor set to the selected {what}"))
                }
            }
            _ => None,
        }
    }
}

/// Object name shortened to fit the wheel hub.
fn hub_label(name: &str) -> String {
    if name.chars().count() > 9 {
        format!("{}…", name.chars().take(8).collect::<String>())
    } else {
        name.to_string()
    }
}
