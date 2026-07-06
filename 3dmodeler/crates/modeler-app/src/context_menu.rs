//! Right-click context menu in the viewport.
//!
//! Object mode: right-clicking an object selects it (unless it is already
//! part of the selection) and opens a menu at the cursor — set the pivot or
//! anchor to the exact clicked surface point, reset them, and the common
//! object operations. Edit mode: right-clicking a vertex/edge/face selects
//! that element and offers "set as pivot / anchor" (same as the P/A keys).
//!
//! main.rs decides what was hit (physics ray in object mode, element pick in
//! edit mode) and calls `open`; the menu itself is drawn from UiState::draw.

use crate::library::LibraryPanel;
use crate::modal::{self, ModalTransform};
use crate::object_ops;
use crate::selection::Selection;
use crate::settings::Unit;
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Primitive, Scene, WallCutout};
use three_d::egui;

#[derive(Clone, Copy)]
pub enum Target {
    /// An object, with the clicked surface point in its LOCAL space.
    Object { id: ObjectId, hit_local: Vec3 },
    /// An edit-mode element (vertex/edge/face) and its local point.
    Element { id: ObjectId, point: Vec3, label: &'static str },
}

pub struct ContextMenu {
    state: Option<(egui::Pos2, Target)>,
    /// Guards `clicked_elsewhere` on the frame the menu opened.
    just_opened: bool,
}

impl ContextMenu {
    pub fn new() -> Self {
        Self { state: None, just_opened: false }
    }

    pub fn open(&mut self, pos: egui::Pos2, target: Target) {
        self.state = Some((pos, target));
        self.just_opened = true;
    }

    pub fn close(&mut self) {
        self.state = None;
    }

    /// Draw the menu; returns a status-bar message when an action ran.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        modal: &mut ModalTransform,
        library_panel: &mut LibraryPanel,
        unit: Unit,
    ) -> Option<String> {
        let Some((pos, target)) = self.state else { return None };
        let mut status = None;
        let mut close = false;

        let area = egui::Area::new(egui::Id::new("viewport-context-menu"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(190.0);
                    close = match target {
                        Target::Object { id, hit_local } => object_menu(
                            ui, scene, selection, modal, library_panel, id, hit_local, unit,
                            &mut status,
                        ),
                        Target::Element { id, point, label } => {
                            element_menu(ui, scene, id, point, label, &mut status)
                        }
                    };
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        if !self.just_opened && area.response.clicked_elsewhere() {
            close = true;
        }
        self.just_opened = false;
        if close {
            self.state = None;
        }
        status
    }
}

#[allow(clippy::too_many_arguments)]
fn object_menu(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    selection: &mut Selection,
    modal: &mut ModalTransform,
    library_panel: &mut LibraryPanel,
    id: ObjectId,
    hit_local: Vec3,
    unit: Unit,
    status: &mut Option<String>,
) -> bool {
    let Some(object) = scene.object(id) else { return true };
    let name = object.name.clone();
    let is_group = object.group;
    let wall = match object.primitive {
        Primitive::Wall { .. } if object.edited_mesh.is_none() => Some(object.primitive),
        _ => None,
    };
    ui.label(
        egui::RichText::new(if is_group { format!("❐ {name} (group)") } else { name.clone() })
            .weak()
            .size(11.0),
    );
    let mut close = false;

    if is_group {
        if ui
            .button("Ungroup")
            .on_hover_text(
                "Break the group into its parts: clicks select parts \
                 individually again (the parent hierarchy is kept)",
            )
            .clicked()
        {
            if let Some(object) = scene.object_mut(id) {
                object.group = false;
            }
            *status = Some(format!("ungrouped '{name}' — parts are now selectable"));
            close = true;
        }
        ui.separator();
    }

    // wall section: dimensions, material and openings at the clicked point
    if let Some(Primitive::Wall { length, height, thickness }) = wall {
        ui.label(egui::RichText::new("Wall").weak().size(11.0));

        for (label, value, min, max, speed) in [
            ("Height", height, 0.1f32, 20.0f32, 0.02f64),
            ("Thickness", thickness, 0.01, 2.0, 0.005),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                let mut shown = unit.from_meters(value);
                if ui
                    .add(
                        egui::DragValue::new(&mut shown)
                            .speed(speed * unit.per_meter() as f64)
                            .range(unit.from_meters(min)..=unit.from_meters(max))
                            .suffix(format!(" {}", unit.suffix())),
                    )
                    .changed()
                {
                    let value = unit.to_meters(shown).clamp(min, max);
                    if let Some(object) = scene.object_mut(id) {
                        object.primitive = match label {
                            "Height" => Primitive::Wall { length, height: value, thickness },
                            _ => Primitive::Wall { length, height, thickness: value },
                        };
                    }
                }
            });
        }

        ui.horizontal(|ui| {
            ui.label("Material");
            let mut color = scene.object(id).map(|o| o.material.base_color).unwrap_or_default();
            if ui.color_edit_button_rgb(&mut color).changed() {
                if let Some(object) = scene.object_mut(id) {
                    object.material.base_color = color;
                }
            }
        });

        if ui
            .button("Add door here")
            .on_hover_text("Cut a 0.9 × 2.1 m door opening centered on the clicked point")
            .clicked()
        {
            if let Some(object) = scene.object_mut(id) {
                object.cutouts.push(WallCutout::door(hit_local.x, length, height));
                object.mesh_revision += 1;
            }
            *status = Some(format!("door added to '{name}'"));
            close = true;
        }
        if ui
            .button("Add window here")
            .on_hover_text("Cut a 1.2 × 1.2 m window opening centered on the clicked point")
            .clicked()
        {
            if let Some(object) = scene.object_mut(id) {
                object
                    .cutouts
                    .push(WallCutout::window(hit_local.x, hit_local.z, length, height));
                object.mesh_revision += 1;
            }
            *status = Some(format!("window added to '{name}'"));
            close = true;
        }
        let cutout_count = scene.object(id).map(|o| o.cutouts.len()).unwrap_or(0);
        if cutout_count > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{cutout_count} opening{} — edit in the sidebar (N)",
                    if cutout_count == 1 { "" } else { "s" }
                ))
                .weak()
                .size(11.0),
            );
        }
        ui.separator();
    }

    if ui
        .button("Set pivot to this point")
        .on_hover_text("The clicked surface point becomes the rotation pivot (R)")
        .clicked()
    {
        if let Some(object) = scene.object_mut(id) {
            object.pivot = hit_local;
        }
        *status = Some(format!("pivot of '{name}' set"));
        close = true;
    }
    if ui
        .button("Set anchor to this point")
        .on_hover_text("The clicked surface point becomes the attachment anchor")
        .clicked()
    {
        if let Some(object) = scene.object_mut(id) {
            object.anchor = hit_local;
        }
        *status = Some(format!("anchor of '{name}' set"));
        close = true;
    }
    if ui.button("Reset pivot / anchor to origin").clicked() {
        if let Some(object) = scene.object_mut(id) {
            object.pivot = Vec3::ZERO;
            object.anchor = Vec3::ZERO;
        }
        *status = Some(format!("pivot and anchor of '{name}' reset"));
        close = true;
    }
    ui.separator();

    if ui.button("Duplicate  (Shift+D)").clicked() {
        if modal::duplicate_selection(scene, selection) {
            modal.begin_grab(scene, selection);
        }
        close = true;
    }
    if ui.button("Delete  (X)").clicked() {
        object_ops::delete_selected(scene, selection);
        close = true;
    }
    ui.separator();

    let can_attach = selection.selected().len() >= 2 && selection.active().is_some();
    if ui
        .add_enabled(can_attach, egui::Button::new("Attach to Active"))
        .on_hover_text(
            "Move each selected object so its anchor lands on the active \
             object's anchor, then parent it there",
        )
        .clicked()
    {
        if let Some(active) = selection.active() {
            for id in selection.selected().to_vec() {
                if id != active {
                    scene.attach(id, active, None);
                }
            }
        }
        close = true;
    }
    if !is_group {
        let can_group = selection.selected().len() >= 2 && selection.active().is_some();
        if ui
            .add_enabled(can_group, egui::Button::new("Group Selection"))
            .on_hover_text(
                "Parent the selected objects to the active one and make it a \
                 group: clicks then select the assembly as one unit",
            )
            .clicked()
        {
            crate::ui::group_selection(scene, selection);
            *status = Some("grouped the selection".to_string());
            close = true;
        }
    }
    if ui.button("Save Selection to Library…").clicked() {
        library_panel.open_create_dialog(scene, selection);
        close = true;
    }
    close
}

fn element_menu(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    id: ObjectId,
    point: Vec3,
    label: &str,
    status: &mut Option<String>,
) -> bool {
    ui.label(egui::RichText::new(label).weak().size(11.0));
    let mut close = false;

    if ui
        .button("Set as pivot point  (P)")
        .on_hover_text("Rotations (R) spin the object around this point")
        .clicked()
    {
        if let Some(object) = scene.object_mut(id) {
            object.pivot = point;
        }
        *status = Some(format!("pivot set to the selected {}", label.to_lowercase()));
        close = true;
    }
    if ui
        .button("Set as anchor point  (A)")
        .on_hover_text("The object attaches to other objects by this point")
        .clicked()
    {
        if let Some(object) = scene.object_mut(id) {
            object.anchor = point;
        }
        *status = Some(format!("anchor set to the selected {}", label.to_lowercase()));
        close = true;
    }
    close
}
