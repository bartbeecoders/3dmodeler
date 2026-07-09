//! Blender's Shift+A "Add" menu as a pie / wheel menu (see pie.rs).
//!
//! Opens centered on the cursor — via Shift+A or a right-click on empty
//! canvas (main.rs calls `open_at` when the pick hits nothing): eight chips
//! (the seven primitives + Wall) around a hub. LMB commits the hovered
//! slot, RMB / Esc / clicking other UI cancels.
//!
//! Events are consumed in `handle_events` (which runs after the egui pass,
//! see main.rs) so a commit click never falls through to viewport picking;
//! the actual commit happens on the next `ui` call via `pending_click`.

use crate::object_ops::event_pos_to_egui;
use crate::pie::{self, PieIcon, PieSlot};
use modeler_core::{Primitive, Scene, Transform};
use three_d::egui;
use three_d::{Event, Key, MouseButton, Viewport};

#[derive(Clone, Copy)]
enum PieItem {
    Primitive(Primitive),
    Wall,
    Floor,
}

/// Slot order around the wheel, starting north and going clockwise.
/// Cube sits on top — it is used the most.
fn pie_items() -> [(PieItem, &'static str); 11] {
    // catalog: [Plane, Cube, UvSphere, IcoSphere, Cylinder, Cone, Torus, Empty]
    let c = Primitive::catalog();
    // point light; other kinds via the properties panel or the Add menu
    let light = Primitive::light_catalog()[0];
    [
        (PieItem::Primitive(c[1]), "Cube"),
        (PieItem::Primitive(c[2]), "UV Sphere"),
        (PieItem::Primitive(c[3]), "Ico Sphere"),
        (PieItem::Primitive(c[5]), "Cone"),
        (PieItem::Primitive(c[4]), "Cylinder"),
        (PieItem::Primitive(c[6]), "Torus"),
        (PieItem::Primitive(c[0]), "Plane"),
        (PieItem::Primitive(light), "Light"),
        (PieItem::Primitive(c[7]), "Empty"),
        (PieItem::Floor, "Floor"),
        (PieItem::Wall, "Wall"),
    ]
}

fn slot_icon(item: PieItem) -> PieIcon {
    match item {
        PieItem::Wall => PieIcon::Wall,
        PieItem::Floor => PieIcon::Floor,
        PieItem::Primitive(primitive) => pie::primitive_icon(&primitive),
    }
}

pub struct AddMenu {
    open: bool,
    position: egui::Pos2,
    last_mouse: egui::Pos2,
    /// LMB arrived in `handle_events`; commit on the next `ui` pass.
    pending_click: bool,
    /// Guards event handling on the frame the menu opened (an opening RMB
    /// press is already in this frame's event list, marked handled).
    just_opened: bool,
    /// 0 → 1 scale-in animation (owned here, rendered by pie::draw).
    anim: f32,
}

impl AddMenu {
    pub fn new() -> Self {
        Self {
            open: false,
            position: egui::Pos2::new(200.0, 200.0),
            last_mouse: egui::Pos2::new(200.0, 200.0),
            pending_click: false,
            just_opened: false,
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
                Event::MousePress { button, handled, .. }
                    if self.open && !self.just_opened =>
                {
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

    /// Open the wheel at `pos`. Also called from main.rs when a right-click
    /// lands on empty canvas (the opening press is marked handled there).
    pub fn open_at(&mut self, pos: egui::Pos2) {
        self.open = true;
        self.position = pos;
        self.pending_click = false;
        self.just_opened = true;
        self.anim = 0.0;
    }

    /// Draw the wheel; returns a status-bar message when an action ran.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        scene: &mut Scene,
        selection: &mut crate::selection::Selection,
        wall_tool: &mut crate::wall_tool::WallTool,
        settings: &crate::settings::Settings,
    ) -> Option<String> {
        if !self.open {
            return None;
        }
        let items = pie_items();
        let slots: Vec<PieSlot> = items
            .iter()
            .map(|&(item, label)| PieSlot::new(label, slot_icon(item)))
            .collect();
        let hovered = pie::draw(ctx, "add-pie", self.position, "Add", &slots, &mut self.anim);

        // commit / cancel (the click was consumed in handle_events);
        // clicking the hub or dead center closes without adding
        let mut status = None;
        if self.pending_click {
            self.pending_click = false;
            if let Some(slot) = hovered {
                match items[slot].0 {
                    PieItem::Primitive(primitive) => {
                        let id = scene.add_object(primitive, Transform::default());
                        selection.set(vec![id], Some(id));
                    }
                    PieItem::Wall => wall_tool.start(settings),
                    PieItem::Floor => {
                        status = Some(crate::object_ops::add_floor(scene, selection));
                    }
                }
            }
            self.open = false;
        }
        self.just_opened = false;
        status
    }
}

/// The primitive list as menu buttons with pictograms; returns the clicked
/// primitive. Used by the menu-bar Add dropdown (Shift+A opens the pie).
pub fn mesh_menu_buttons(ui: &mut egui::Ui) -> Option<Primitive> {
    let mut clicked = None;
    for primitive in Primitive::catalog() {
        let label = match primitive {
            Primitive::UvSphere { .. } => "UV Sphere".to_string(),
            Primitive::IcoSphere { .. } => "Ico Sphere".to_string(),
            other => other.base_name().to_string(),
        };
        if pie::icon_menu_button(ui, &pie::primitive_icon(&primitive), &label).clicked() {
            clicked = Some(primitive);
        }
    }
    clicked
}
