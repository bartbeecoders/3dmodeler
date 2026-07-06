//! Object-library storage and UI.
//!
//! The library persists app-wide (native: JSON in the user's config dir next
//! to settings; web: localStorage) — it is NOT part of the scene file, so
//! assets are available across scenes. The sidebar panel lists every asset
//! with its preview; dragging one into the viewport places it at the drop
//! point (main.rs resolves the drop ray), and Object ▸ Save Selection to
//! Library… captures the current selection as a new asset.

use crate::preview;
use crate::selection::Selection;
use modeler_core::glam::Vec3;
use modeler_core::{library, Library, Scene};
use std::collections::HashMap;
use three_d::egui;

// --- persistence -------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
fn library_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("box3d-modeler").join("library.json"))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_store() -> Option<String> {
    std::fs::read_to_string(library_path()?).ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn write_store(json: &str) {
    let Some(path) = library_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "modeler_library";

#[cfg(target_arch = "wasm32")]
fn read_store() -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()
        .flatten()?
        .get_item(STORAGE_KEY)
        .ok()
        .flatten()
}

#[cfg(target_arch = "wasm32")]
fn write_store(json: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, json);
    }
}

pub fn load() -> Library {
    read_store()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn save(library: &Library) {
    if let Ok(json) = serde_json::to_string(library) {
        write_store(&json);
    }
}

// --- UI ------------------------------------------------------------------

/// Drag payload for library assets (distinct type so it can't be confused
/// with the outliner's ObjectId drags).
#[derive(Clone, Copy)]
struct AssetDrag(u64);

/// A finished drag out of the library: place this asset at the pointer.
pub struct DropRequest {
    pub asset_id: u64,
    /// egui logical points, top-left origin.
    pub pos: egui::Pos2,
}

/// The "Save Selection to Library…" / edit-asset dialog.
enum Dialog {
    Closed,
    Create {
        name: String,
        description: String,
    },
    Edit {
        asset_id: u64,
        name: String,
        description: String,
        pivot: Vec3,
        anchor: Vec3,
    },
}

pub struct LibraryPanel {
    dialog: Dialog,
    /// Preview textures, keyed by asset id; the hash detects preview changes
    /// (e.g. an MCP update_library_object while the panel is visible).
    textures: HashMap<u64, (u64, egui::TextureHandle)>,
    drop: Option<DropRequest>,
}

impl LibraryPanel {
    pub fn new() -> Self {
        Self {
            dialog: Dialog::Closed,
            textures: HashMap::new(),
            drop: None,
        }
    }

    /// Open the create dialog for the current selection (Object menu).
    pub fn open_create_dialog(&mut self, scene: &Scene, selection: &Selection) {
        let name = selection
            .active()
            .and_then(|id| scene.object(id))
            .map(|o| o.name.clone())
            .unwrap_or_else(|| "Asset".to_string());
        self.dialog = Dialog::Create { name, description: String::new() };
    }

    /// The asset drop from this frame's drag, if any (consumed by main.rs).
    pub fn take_drop(&mut self) -> Option<DropRequest> {
        self.drop.take()
    }

    /// Sidebar section: one draggable row per asset.
    pub fn section(
        &mut self,
        ui: &mut egui::Ui,
        library: &mut Library,
    ) -> Option<String> {
        let mut status = None;
        ui.strong("Library");
        if library.assets().is_empty() {
            ui.weak("Empty — select objects and use\nObject ▸ Save Selection to Library…");
            return None;
        }
        ui.weak("Drag an item into the viewport to place it.");

        let ids: Vec<u64> = library.assets().iter().map(|a| a.id).collect();
        let mut delete: Option<u64> = None;
        for id in ids {
            let Some(asset) = library.asset(id) else { continue };
            let texture = self.texture_for(ui.ctx(), id, &asset.preview_png_base64);
            let (name, description) = (asset.name.clone(), asset.description.clone());
            let objects = asset.objects.len();

            ui.horizontal(|ui| {
                let response = ui
                    .dnd_drag_source(egui::Id::new(("library-asset", id)), AssetDrag(id), |ui| {
                        if let Some(texture) = &texture {
                            ui.add(
                                egui::Image::new(texture)
                                    .fit_to_exact_size(egui::vec2(36.0, 36.0)),
                            );
                        } else {
                            ui.label(egui::RichText::new("▦").size(28.0).weak());
                        }
                        ui.vertical(|ui| {
                            ui.label(&name);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{objects} object{}",
                                    if objects == 1 { "" } else { "s" }
                                ))
                                .weak()
                                .size(10.0),
                            );
                        });
                    })
                    .response;
                if !description.is_empty() {
                    response.on_hover_text(&description);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✖").on_hover_text("Delete from library").clicked() {
                        delete = Some(id);
                    }
                    if ui
                        .small_button("✏")
                        .on_hover_text("Edit name / description / pivot / anchor")
                        .clicked()
                    {
                        let asset = library.asset(id);
                        self.dialog = Dialog::Edit {
                            asset_id: id,
                            name: name.clone(),
                            description: description.clone(),
                            pivot: asset.map(|a| a.pivot).unwrap_or(Vec3::ZERO),
                            anchor: asset.map(|a| a.anchor).unwrap_or(Vec3::ZERO),
                        };
                    }
                });
            });
        }
        if let Some(id) = delete {
            let name = library.asset(id).map(|a| a.name.clone()).unwrap_or_default();
            library.remove_asset(id);
            self.textures.remove(&id);
            status = Some(format!("deleted '{name}' from the library"));
        }
        status
    }

    /// Detect a drag released over the 3D viewport (outside all panels).
    /// Call after the layout offsets are known.
    pub fn detect_viewport_drop(
        &mut self,
        ctx: &egui::Context,
        top_offset: f32,
        right_offset: f32,
        bottom_offset: f32,
    ) {
        if !ctx.input(|i| i.pointer.any_released()) {
            return;
        }
        if !egui::DragAndDrop::has_payload_of_type::<AssetDrag>(ctx) {
            return;
        }
        let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) else { return };
        let screen = ctx.content_rect();
        let in_viewport = pos.x >= screen.left()
            && pos.x <= screen.right() - right_offset
            && pos.y >= screen.top() + top_offset
            && pos.y <= screen.bottom() - bottom_offset;
        if !in_viewport {
            return;
        }
        if let Some(payload) = egui::DragAndDrop::take_payload::<AssetDrag>(ctx) {
            self.drop = Some(DropRequest { asset_id: payload.0, pos });
        }
    }

    /// The create/edit dialog window. Returns a status message on completion.
    pub fn dialog_window(
        &mut self,
        ctx: &egui::Context,
        scene: &Scene,
        selection: &Selection,
        library: &mut Library,
    ) -> Option<String> {
        let mut status = None;
        let (title, mut name, mut description, mut points, editing) = match &self.dialog {
            Dialog::Closed => return None,
            Dialog::Create { name, description } => (
                "Save Selection to Library",
                name.clone(),
                description.clone(),
                (Vec3::ZERO, Vec3::ZERO),
                None,
            ),
            Dialog::Edit { asset_id, name, description, pivot, anchor } => (
                "Edit Library Item",
                name.clone(),
                description.clone(),
                (*pivot, *anchor),
                Some(*asset_id),
            ),
        };

        let mut open = true;
        let mut confirmed = false;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if editing.is_none() {
                    let count = selection.selected().len();
                    ui.label(format!(
                        "{count} selected object{} (children included)",
                        if count == 1 { "" } else { "s" }
                    ));
                    ui.add_space(4.0);
                }
                ui.label("Name:");
                let response = ui.text_edit_singleline(&mut name);
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirmed = true;
                }
                ui.label("Description:");
                ui.add(
                    egui::TextEdit::multiline(&mut description)
                        .desired_rows(3)
                        .desired_width(280.0),
                );
                if editing.is_some() {
                    ui.add_space(4.0);
                    for (label, value, hint) in [
                        (
                            "Pivot:",
                            &mut points.0,
                            "Placement/rotation reference — lands on the drop point \
                             when placed on empty ground. (0,0,0) = footprint center \
                             at the lowest point.",
                        ),
                        (
                            "Anchor:",
                            &mut points.1,
                            "Attachment point — lands on the hit point when the asset \
                             is dropped onto another object (and parents to it).",
                        ),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(label).on_hover_text(hint);
                            for axis in [&mut value.x, &mut value.y, &mut value.z] {
                                ui.add(egui::DragValue::new(axis).speed(0.05));
                            }
                        });
                    }
                }
                ui.add_space(6.0);
                let valid = !name.trim().is_empty()
                    && (editing.is_some() || !selection.is_empty());
                if ui.add_enabled(valid, egui::Button::new("Save")).clicked() {
                    confirmed = true;
                }
            });

        if confirmed {
            match editing {
                None => {
                    let objects = library::capture_objects(scene, selection.selected());
                    if objects.is_empty() {
                        status = Some("nothing selected to save".to_string());
                    } else {
                        let preview = preview::render_preview_base64(&objects);
                        let id = library.add_asset(
                            name.trim(),
                            description.trim(),
                            objects,
                            preview,
                        );
                        let saved = library.asset(id).map(|a| a.name.clone()).unwrap_or_default();
                        status = Some(format!("saved '{saved}' to the library"));
                    }
                }
                Some(asset_id) => {
                    library.rename_asset(asset_id, name.trim());
                    if let Some(asset) = library.asset_mut(asset_id) {
                        asset.description = description.trim().to_string();
                        asset.pivot = points.0;
                        asset.anchor = points.1;
                    }
                    status = Some("library item updated".to_string());
                }
            }
            self.dialog = Dialog::Closed;
        } else if !open {
            self.dialog = Dialog::Closed;
        } else {
            // write edits back into the dialog state
            self.dialog = match editing {
                None => Dialog::Create { name, description },
                Some(asset_id) => Dialog::Edit {
                    asset_id,
                    name,
                    description,
                    pivot: points.0,
                    anchor: points.1,
                },
            };
        }
        status
    }

    /// Decode + cache the preview texture for an asset.
    fn texture_for(
        &mut self,
        ctx: &egui::Context,
        id: u64,
        preview_base64: &Option<String>,
    ) -> Option<egui::TextureHandle> {
        let data = preview_base64.as_ref()?;
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut hasher);
            hasher.finish()
        };
        if let Some((cached_hash, texture)) = self.textures.get(&id) {
            if *cached_hash == hash {
                return Some(texture.clone());
            }
        }
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
        let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let (w, h) = image.dimensions();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            image.as_raw(),
        );
        let texture = ctx.load_texture(
            format!("library-preview-{id}"),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        self.textures.insert(id, (hash, texture.clone()));
        Some(texture)
    }
}
