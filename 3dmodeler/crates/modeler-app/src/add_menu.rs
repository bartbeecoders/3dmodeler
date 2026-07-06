//! Blender's Shift+A "Add" menu: opens at the mouse cursor, adds a primitive
//! at the origin (the 3D cursor, once we have one).

use crate::object_ops::event_pos_to_egui;
use modeler_core::{Primitive, Scene, Transform};
use three_d::egui;
use three_d::{Event, Key, Viewport};

pub struct AddMenu {
    open: bool,
    position: egui::Pos2,
    last_mouse: egui::Pos2,
}

impl AddMenu {
    pub fn new() -> Self {
        Self {
            open: false,
            position: egui::Pos2::new(200.0, 200.0),
            last_mouse: egui::Pos2::new(200.0, 200.0),
        }
    }

    /// Track the mouse and open/close on Shift+A / Escape / click-away.
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
                    self.open = true;
                    self.position = self.last_mouse;
                }
                // Physical-key fallback (layout-correct on most native
                // backends; harmless double-fire alongside the Text path).
                Event::KeyPress {
                    kind: Key::A,
                    modifiers,
                    handled,
                    ..
                } if !*handled && modifiers.shift => {
                    self.open = true;
                    self.position = self.last_mouse;
                    *handled = true;
                }
                Event::KeyPress {
                    kind: Key::Escape,
                    handled,
                    ..
                } if !*handled && self.open => {
                    self.open = false;
                    *handled = true;
                }
                // any click that egui didn't take (i.e. into the viewport)
                Event::MousePress { handled, .. } if !*handled && self.open => {
                    self.open = false;
                }
                _ => {}
            }
        }
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
        egui::Area::new(egui::Id::new("add-menu"))
            .fixed_pos(self.position)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(140.0);
                    ui.label(egui::RichText::new("Add Mesh").strong().size(12.0));
                    ui.separator();
                    if let Some(primitive) = mesh_menu_buttons(ui) {
                        scene.add_object(primitive, Transform::default());
                        self.open = false;
                    }
                    ui.separator();
                    if ui
                        .button("Wall")
                        .on_hover_text(
                            "Draw wall segments on the floor: click start and corners, \
                             Esc/RMB ends",
                        )
                        .clicked()
                    {
                        wall_tool.start(settings);
                        self.open = false;
                    }
                });
            });
    }
}

/// The primitive list as menu buttons; returns the clicked primitive.
/// Shared by the Shift+A popup and the side panel.
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
