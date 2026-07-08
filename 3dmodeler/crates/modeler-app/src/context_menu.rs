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
//! main.rs decides what was hit (physics ray in object mode, element pick in
//! edit mode) and calls `open`; the wheel is drawn from UiState::draw. Like
//! the Shift+A wheel, clicks are consumed in `handle_events` (runs after the
//! egui pass) and committed on the next `ui` call via `pending_click`.

use crate::library::LibraryPanel;
use crate::modal::{self, ModalTransform};
use crate::object_ops;
use crate::pie::{self, PieIcon, PieSlot};
use crate::selection::Selection;
use modeler_core::glam::Vec3;
use modeler_core::{ObjectId, Primitive, Scene, WallCutout};
use three_d::egui;
use three_d::{Event, Key, MouseButton};

#[derive(Clone, Copy)]
pub enum Target {
    /// An object, with the clicked surface point in its LOCAL space.
    Object { id: ObjectId, hit_local: Vec3 },
    /// An edit-mode element (vertex/edge/face) and its local point.
    Element { id: ObjectId, point: Vec3, label: &'static str },
}

/// What the westward slots offer: wall openings, brick→wall rebuild or
/// group/attach actions.
enum WheelKind {
    Object {
        /// (length, height) when the object is a pristine wall.
        wall: Option<(f32, f32)>,
        /// The bricks folder when the object came from a broken wall.
        rebuild: Option<u64>,
    },
    Element,
}

pub struct ContextMenu {
    state: Option<(egui::Pos2, Target)>,
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
            just_opened: false,
            pending_click: false,
            anim: 0.0,
        }
    }

    pub fn open(&mut self, pos: egui::Pos2, target: Target) {
        self.state = Some((pos, target));
        self.just_opened = true;
        self.pending_click = false;
        self.anim = 0.0;
    }

    pub fn close(&mut self) {
        self.state = None;
        self.pending_click = false;
    }

    /// Consume clicks/Esc while the wheel is open so a commit click never
    /// falls through to viewport picking. Runs after the egui pass and
    /// after the RMB opener (see main.rs), hence the `just_opened` guard.
    pub fn handle_events(&mut self, events: &mut [Event]) {
        if self.state.is_none() || self.just_opened {
            return;
        }
        for event in events.iter_mut() {
            match event {
                Event::KeyPress { kind: Key::Escape, handled, .. } if !*handled => {
                    self.close();
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
                            self.close(); // RMB / MMB cancels
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Draw the wheel; returns a status-bar message when an action ran.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut Selection,
        modal: &mut ModalTransform,
        library_panel: &mut LibraryPanel,
    ) -> Option<String> {
        let Some((pos, target)) = self.state else { return None };
        let mut status = None;

        // build the slot ring for the current target
        let (kind, slots, hub) = match target {
            Target::Object { id, .. } => {
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
                // slot 8: any solid object can shatter into dynamic bricks
                let breakable = !object.primitive.is_light()
                    && !matches!(object.primitive, Primitive::Empty { .. });
                if breakable {
                    slots.push(PieSlot::new("Bricks", PieIcon::Bricks));
                }
                (WheelKind::Object { wall, rebuild }, slots, hub_label(&object.name))
            }
            Target::Element { label, .. } => {
                let slots = vec![
                    PieSlot::new("Pivot  (P)", PieIcon::Glyph("⌖")), // N
                    PieSlot::new("Anchor  (A)", PieIcon::Anchor),    // S
                ];
                (WheelKind::Element, slots, label.to_string())
            }
        };

        let hovered = pie::draw(ctx, "context-pie", pos, &hub, &slots, &mut self.anim);

        if self.pending_click {
            self.pending_click = false;
            if let Some(slot) = hovered {
                status = self.execute(
                    slot, kind, target, scene, selection, modal, library_panel,
                );
            }
            self.state = None;
        }
        self.just_opened = false;
        status
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
    ) -> Option<String> {
        match (kind, target) {
            (WheelKind::Object { wall, rebuild }, Target::Object { id, hit_local }) => {
                let name = scene.object(id).map(|o| o.name.clone()).unwrap_or_default();
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
                        // wall wheel: cut an opening at the clicked point
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
                        // object wheel W: rebuild wall (bricks), else
                        // group/ungroup
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
                    8 => crate::object_ops::break_into_bricks(scene, id).map(|bricks| {
                        let count = bricks.len();
                        let active = bricks.first().copied();
                        selection.set(bricks, active);
                        format!("broken into {count} bricks — Space simulates")
                    }),
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
