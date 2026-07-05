//! Object-level operations: delete via X (with Blender's confirm popup) or
//! the Delete key (immediate).

use crate::selection::Selection;
use modeler_core::Scene;
use three_d::egui;
use three_d::{Event, Key, Viewport};

pub struct DeleteTool {
    confirm_open: bool,
    position: egui::Pos2,
    last_mouse: egui::Pos2,
}

/// Convert a three-d event position (physical px, bottom-left origin) to
/// egui logical coordinates (top-left origin).
pub fn event_pos_to_egui(
    x: f32,
    y: f32,
    viewport: Viewport,
    device_pixel_ratio: f32,
) -> egui::Pos2 {
    egui::Pos2::new(
        x / device_pixel_ratio,
        (viewport.height as f32 - y) / device_pixel_ratio,
    )
}

impl DeleteTool {
    pub fn new() -> Self {
        Self {
            confirm_open: false,
            position: egui::Pos2::new(200.0, 200.0),
            last_mouse: egui::Pos2::new(200.0, 200.0),
        }
    }

    pub fn handle_events(
        &mut self,
        events: &mut [Event],
        viewport: Viewport,
        device_pixel_ratio: f32,
        egui_owns_keyboard: bool,
        scene: &mut Scene,
        selection: &mut Selection,
    ) {
        for event in events.iter_mut() {
            match event {
                Event::MouseMotion { position, .. } => {
                    self.last_mouse =
                        event_pos_to_egui(position.x, position.y, viewport, device_pixel_ratio);
                }
                // Blender: X opens a small "Delete" confirmation at the cursor
                Event::Text(text) if text == "x" && !egui_owns_keyboard => {
                    if !selection.is_empty() {
                        self.confirm_open = true;
                        self.position = self.last_mouse;
                        text.clear();
                    }
                }
                // Delete key removes immediately
                Event::KeyPress {
                    kind: Key::Delete,
                    handled,
                    ..
                } if !*handled => {
                    delete_selected(scene, selection);
                    *handled = true;
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.confirm_open => {
                    self.confirm_open = false;
                    *handled = true;
                }
                Event::MousePress { handled, .. } if self.confirm_open => {
                    // click anywhere else closes the popup; the popup's own
                    // button click is consumed by egui before we see it
                    if !*handled {
                        self.confirm_open = false;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, scene: &mut Scene, selection: &mut Selection) {
        if !self.confirm_open {
            return;
        }
        // the selection can empty out under the popup (e.g. Delete key)
        if selection.is_empty() {
            self.confirm_open = false;
            return;
        }
        egui::Area::new(egui::Id::new("delete-confirm"))
            .fixed_pos(self.position)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(100.0);
                    let count = selection.selected().len();
                    let label = if count == 1 {
                        "Delete?".to_string()
                    } else {
                        format!("Delete {count} objects?")
                    };
                    ui.label(egui::RichText::new(label).strong().size(12.0));
                    ui.separator();
                    if ui.button("Delete").clicked() {
                        delete_selected(scene, selection);
                        self.confirm_open = false;
                    }
                });
            });
    }
}

pub fn delete_selected(scene: &mut Scene, selection: &mut Selection) {
    for id in selection.selected().to_vec() {
        scene.remove_object(id);
    }
    selection.retain_existing(|id| scene.object(id).is_some());
}
