//! PBR material library / collection picker.
//!
//! Browses free & open CC0 PBR sources (ambientCG, Poly Haven) online, keeps a
//! local collection of imported materials, and applies full map sets
//! (albedo / normal / roughness / metallic / AO) to the selection.
//!
//! # Free sources
//!
//! | Source | License | API | Notes |
//! |--------|---------|-----|-------|
//! | [ambientCG](https://ambientcg.com) | CC0 | REST v2 | Largest free set; surface-preview maps for apply |
//! | [Poly Haven](https://polyhaven.com) | CC0 assets | Public API | Photoscanned 1K–8K; API ToS: UA required |
//! | [CG Bookcase](https://www.cgbookcase.com) | Free | none | Listed for discovery |
//! | [Share Textures](https://www.sharetextures.com) | Free | none | Listed for discovery |
//! | [3D Textures](https://3dtextures.me) | Free | none | Listed for discovery |
//! | [FreePBR](https://freepbr.com) | Free | none | Listed for discovery |

use crate::gfx::egui;
use crate::net::{self, BytesTask, HttpTask};
use crate::selection::Selection;
use modeler_core::{Material, MaterialTextures, Scene};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const USER_AGENT: &str = "box3d-modeler/0.2 (PBR library; https://github.com)";

// --- Free source catalog -----------------------------------------------------

/// A known free / open PBR material source (shown on the Sources tab).
#[derive(Clone, Copy)]
pub struct PbrSourceInfo {
    pub name: &'static str,
    pub url: &'static str,
    pub license: &'static str,
    pub notes: &'static str,
    /// Whether this app can browse/download live from an API.
    pub integrated: bool,
}

pub const PBR_SOURCES: &[PbrSourceInfo] = &[
    PbrSourceInfo {
        name: "ambientCG",
        url: "https://ambientcg.com",
        license: "CC0 1.0",
        notes: "1,000+ seamless PBR materials up to 8K. Fully integrated — browse & apply.",
        integrated: true,
    },
    PbrSourceInfo {
        name: "Poly Haven",
        url: "https://polyhaven.com/textures",
        license: "CC0 (assets)",
        notes: "Photoscanned textures ≥8K. Fully integrated — browse & apply (1K maps).",
        integrated: true,
    },
    PbrSourceInfo {
        name: "CG Bookcase",
        url: "https://www.cgbookcase.com",
        license: "Free / no restrictions",
        notes: "Hundreds of free tileable PBR materials. Open in browser.",
        integrated: false,
    },
    PbrSourceInfo {
        name: "Share Textures",
        url: "https://www.sharetextures.com",
        license: "Free for commercial use",
        notes: "1,700+ free textures & models. Open in browser.",
        integrated: false,
    },
    PbrSourceInfo {
        name: "3D Textures",
        url: "https://3dtextures.me",
        license: "Free",
        notes: "Seamless PBR materials with map previews. Open in browser.",
        integrated: false,
    },
    PbrSourceInfo {
        name: "FreePBR",
        url: "https://freepbr.com",
        license: "Free (check per-asset)",
        notes: "Classic free PBR packs. Open in browser.",
        integrated: false,
    },
];

// --- Local collection persistence --------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalPbrEntry {
    /// Stable id: `{source}:{source_id}`.
    pub id: String,
    pub source: String,
    pub source_id: String,
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub license: String,
    /// Relative path under the pbr cache dir for the thumbnail, if any.
    #[serde(default)]
    pub thumbnail: Option<String>,
    pub textures: MaterialTextures,
    /// Optional average base color sampled from the albedo map.
    #[serde(default)]
    pub base_color: Option<[f32; 3]>,
    #[serde(default)]
    pub roughness: Option<f32>,
    #[serde(default)]
    pub metallic: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalCollection {
    entries: Vec<LocalPbrEntry>,
}

#[cfg(not(target_arch = "wasm32"))]
fn pbr_root() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("box3d-modeler").join("pbr"))
}

#[cfg(not(target_arch = "wasm32"))]
fn collection_path() -> Option<PathBuf> {
    Some(pbr_root()?.join("collection.json"))
}

#[cfg(not(target_arch = "wasm32"))]
fn load_collection() -> LocalCollection {
    collection_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn save_collection(c: &LocalCollection) {
    let Some(path) = collection_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(target_arch = "wasm32")]
fn load_collection() -> LocalCollection {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item("modeler_pbr_collection").ok().flatten())
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn save_collection(c: &LocalCollection) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if let Ok(json) = serde_json::to_string(c) {
            let _ = storage.set_item("modeler_pbr_collection", &json);
        }
    }
}

/// Resolve a texture cache key to absolute bytes (native: disk; wasm: memory).
pub fn load_texture_bytes(key: &str) -> Option<Vec<u8>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = pbr_root()?.join(key);
        std::fs::read(path).ok()
    }
    #[cfg(target_arch = "wasm32")]
    {
        // Wasm keeps maps in a process-local store filled at import time.
        WASM_TEXTURE_STORE.with(|s| s.borrow().get(key).cloned())
    }
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static WASM_TEXTURE_STORE: std::cell::RefCell<HashMap<String, Vec<u8>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn store_texture_bytes(key: &str, bytes: &[u8]) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = pbr_root()
            .ok_or_else(|| "no config dir".to_string())?
            .join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(target_arch = "wasm32")]
    {
        WASM_TEXTURE_STORE.with(|s| {
            s.borrow_mut().insert(key.to_string(), bytes.to_vec());
        });
        Ok(())
    }
}

// --- Online catalog entries --------------------------------------------------

#[derive(Clone)]
pub struct RemotePbrEntry {
    pub source: &'static str,
    pub source_id: String,
    pub name: String,
    pub category: String,
    pub thumbnail_url: String,
    pub tags: Vec<String>,
}

// --- Async job state ---------------------------------------------------------

enum Job {
    CatalogAmbient {
        task: HttpTask,
    },
    CatalogPoly {
        task: HttpTask,
    },
    Thumb {
        key: String,
        task: BytesTask,
    },
    /// Download maps for apply / import.
    Import {
        source: &'static str,
        source_id: String,
        name: String,
        category: String,
        pending: Vec<(String, String)>, // (map_name, url)
        done: HashMap<String, Vec<u8>>,
        /// How many maps this import started with (for progress UI).
        total_maps: usize,
        task: Option<BytesTask>,
        current_map: Option<String>,
        apply: bool,
        also_local: bool,
    },
}

// --- Panel -------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sources,
    AmbientCg,
    PolyHaven,
    Local,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Self::Sources => "Sources",
            Self::AmbientCg => "ambientCG",
            Self::PolyHaven => "Poly Haven",
            Self::Local => "Local",
        }
    }

    /// Short label for the narrow properties-panel tab strip.
    fn short_label(self) -> &'static str {
        match self {
            Self::Sources => "Src",
            Self::AmbientCg => "aCG",
            Self::PolyHaven => "PH",
            Self::Local => "Loc",
        }
    }
}

pub struct PbrLibraryPanel {
    tab: Tab,
    search: String,
    category_filter: String,
    local: LocalCollection,
    ambient_entries: Vec<RemotePbrEntry>,
    poly_entries: Vec<RemotePbrEntry>,
    ambient_loaded: bool,
    poly_loaded: bool,
    ambient_error: Option<String>,
    poly_error: Option<String>,
    jobs: Vec<Job>,
    /// Thumbnail key → egui texture.
    thumbs: HashMap<String, egui::TextureHandle>,
    /// Thumbnail keys currently downloading.
    thumb_pending: HashMap<String, ()>,
    status: Option<String>,
    /// Apply result waiting to be consumed by the frame loop.
    pending_apply: Option<(Material, String)>,
    /// Categories discovered for the active online tab.
    categories: Vec<String>,
}

impl PbrLibraryPanel {
    pub fn new() -> Self {
        Self {
            tab: Tab::Sources,
            search: String::new(),
            category_filter: String::new(),
            local: load_collection(),
            ambient_entries: Vec::new(),
            poly_entries: Vec::new(),
            ambient_loaded: false,
            poly_loaded: false,
            ambient_error: None,
            poly_error: None,
            jobs: Vec::new(),
            thumbs: HashMap::new(),
            thumb_pending: HashMap::new(),
            status: None,
            pending_apply: None,
            categories: Vec::new(),
        }
    }

    /// Status line for the app bar, if any.
    pub fn take_status(&mut self) -> Option<String> {
        self.status.take()
    }

    /// Material to apply to the selection (from an Apply click).
    pub fn take_apply(&mut self) -> Option<(Material, String)> {
        self.pending_apply.take()
    }

    /// Poll background downloads; call once per frame.
    pub fn poll(&mut self) {
        let mut finished_idx = Vec::new();
        let new_jobs = Vec::new();
        let mut status_msgs = Vec::new();
        let mut apply_queue = Vec::new();
        let mut local_dirty = false;

        for (i, job) in self.jobs.iter_mut().enumerate() {
            match job {
                Job::CatalogAmbient { task } => {
                    if let Some(result) = task.poll() {
                        finished_idx.push(i);
                        match result {
                            Ok(body) => match parse_ambientcg_catalog(&body) {
                                Ok(entries) => {
                                    self.categories = collect_categories(&entries);
                                    self.ambient_entries = entries;
                                    self.ambient_loaded = true;
                                    self.ambient_error = None;
                                    status_msgs.push(format!(
                                        "ambientCG: {} materials",
                                        self.ambient_entries.len()
                                    ));
                                }
                                Err(e) => {
                                    self.ambient_error = Some(e.clone());
                                    status_msgs.push(format!("ambientCG catalog: {e}"));
                                }
                            },
                            Err(e) => {
                                self.ambient_error = Some(e.clone());
                                status_msgs.push(format!("ambientCG: {e}"));
                            }
                        }
                    }
                }
                Job::CatalogPoly { task } => {
                    if let Some(result) = task.poll() {
                        finished_idx.push(i);
                        match result {
                            Ok(body) => match parse_polyhaven_catalog(&body) {
                                Ok(entries) => {
                                    self.categories = collect_categories(&entries);
                                    self.poly_entries = entries;
                                    self.poly_loaded = true;
                                    self.poly_error = None;
                                    status_msgs.push(format!(
                                        "Poly Haven: {} textures",
                                        self.poly_entries.len()
                                    ));
                                }
                                Err(e) => {
                                    self.poly_error = Some(e.clone());
                                    status_msgs.push(format!("Poly Haven catalog: {e}"));
                                }
                            },
                            Err(e) => {
                                self.poly_error = Some(e.clone());
                                status_msgs.push(format!("Poly Haven: {e}"));
                            }
                        }
                    }
                }
                Job::Thumb { key, task } => {
                    if let Some(result) = task.poll() {
                        finished_idx.push(i);
                        self.thumb_pending.remove(key);
                        if let Ok(bytes) = result {
                            let _ = store_texture_bytes(key, &bytes);
                            // Texture handle is created lazily in UI from bytes.
                        }
                    }
                }
                Job::Import {
                    source,
                    source_id,
                    name,
                    category,
                    pending,
                    done,
                    total_maps: _,
                    task,
                    current_map,
                    apply,
                    also_local,
                } => {
                    // Start next download if idle.
                    if task.is_none() {
                        if let Some((map, url)) = pending.pop() {
                            *current_map = Some(map.clone());
                            *task = Some(net::fetch_bytes(&url, USER_AGENT));
                        } else {
                            // All maps done — build MaterialTextures.
                            finished_idx.push(i);
                            let prefix = format!("{source}/{source_id}");
                            let mut textures = MaterialTextures::default();
                            let mut base_color = None;
                            for (map, bytes) in done.iter() {
                                let rel = format!("{prefix}/{map}.jpg");
                                if let Err(e) = store_texture_bytes(&rel, bytes) {
                                    status_msgs.push(format!("cache write failed: {e}"));
                                    continue;
                                }
                                match map.as_str() {
                                    "albedo" => {
                                        textures.albedo = Some(rel);
                                        base_color = sample_average_color(bytes);
                                    }
                                    "normal" => textures.normal = Some(rel),
                                    "roughness" => textures.roughness = Some(rel),
                                    "metallic" => textures.metallic = Some(rel),
                                    "occlusion" => textures.occlusion = Some(rel),
                                    _ => {}
                                }
                            }
                            let metallic = if textures.metallic.is_some() {
                                Some(1.0)
                            } else {
                                Some(0.0)
                            };
                            let roughness = Some(0.5);
                            if *also_local {
                                let entry = LocalPbrEntry {
                                    id: format!("{source}:{source_id}"),
                                    source: source.to_string(),
                                    source_id: source_id.clone(),
                                    name: name.clone(),
                                    category: category.clone(),
                                    license: "CC0".into(),
                                    thumbnail: textures.albedo.clone(),
                                    textures: textures.clone(),
                                    base_color,
                                    roughness,
                                    metallic,
                                };
                                self.local
                                    .entries
                                    .retain(|e| e.id != entry.id);
                                self.local.entries.insert(0, entry);
                                local_dirty = true;
                            }
                            if *apply {
                                let mut mat = Material::default();
                                if let Some(c) = base_color {
                                    mat.base_color = c;
                                }
                                if let Some(r) = roughness {
                                    mat.roughness = r;
                                }
                                if let Some(m) = metallic {
                                    mat.metallic = m;
                                }
                                mat.textures = textures;
                                apply_queue.push((mat, name.clone()));
                            }
                            status_msgs.push(format!("imported PBR '{name}'"));
                        }
                    } else if let Some(t) = task.as_mut() {
                        if let Some(result) = t.poll() {
                            let map = current_map.take().unwrap_or_default();
                            *task = None;
                            match result {
                                Ok(bytes) => {
                                    done.insert(map, bytes);
                                }
                                Err(e) => {
                                    status_msgs.push(format!("map '{map}' failed: {e}"));
                                    // Continue with remaining maps.
                                }
                            }
                        }
                    }
                }
            }
        }

        // Remove finished jobs (reverse order).
        finished_idx.sort_unstable();
        finished_idx.dedup();
        for i in finished_idx.into_iter().rev() {
            if i < self.jobs.len() {
                self.jobs.remove(i);
            }
        }
        self.jobs.extend(new_jobs);
        if local_dirty {
            save_collection(&self.local);
        }
        if let Some(msg) = status_msgs.pop() {
            self.status = Some(msg);
        }
        if let Some(a) = apply_queue.pop() {
            self.pending_apply = Some(a);
        }
    }

    /// Collection picker UI (Properties → PBR Library tab).
    pub fn section(
        &mut self,
        ui: &mut egui::Ui,
        scene: &Scene,
        selection: &Selection,
    ) {
        // Compact tab strip — short labels so all four fit inside the capped
        // sidebar width (long names used to overflow the clip rect and become
        // unclickable while still painting).
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for tab in [Tab::Sources, Tab::AmbientCg, Tab::PolyHaven, Tab::Local] {
                let selected = self.tab == tab;
                if ui
                    .selectable_label(selected, tab.short_label())
                    .on_hover_text(tab.label())
                    .clicked()
                {
                    self.tab = tab;
                    self.category_filter.clear();
                    match tab {
                        Tab::AmbientCg
                            if !self.ambient_loaded && !self.catalog_in_flight("ambientcg") =>
                        {
                            self.start_ambient_catalog();
                        }
                        Tab::PolyHaven
                            if !self.poly_loaded && !self.catalog_in_flight("polyhaven") =>
                        {
                            self.start_poly_catalog();
                        }
                        Tab::Local => {
                            self.categories = self
                                .local
                                .entries
                                .iter()
                                .map(|e| e.category.clone())
                                .filter(|c| !c.is_empty())
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .collect();
                        }
                        _ => {}
                    }
                }
            }
        });

        ui.add_space(4.0);
        match self.tab {
            Tab::Sources => self.draw_sources(ui),
            Tab::AmbientCg => {
                self.draw_remote_browser(ui, scene, selection, "ambientcg");
            }
            Tab::PolyHaven => {
                self.draw_remote_browser(ui, scene, selection, "polyhaven");
            }
            Tab::Local => self.draw_local(ui, scene, selection),
        }
    }

    fn catalog_in_flight(&self, source: &str) -> bool {
        self.jobs.iter().any(|j| match j {
            Job::CatalogAmbient { .. } => source == "ambientcg",
            Job::CatalogPoly { .. } => source == "polyhaven",
            _ => false,
        })
    }

    fn start_ambient_catalog(&mut self) {
        // Popular materials with previews (limit keeps the first page snappy).
        let url = "https://ambientcg.com/api/v2/full_json?type=Material&sort=Popular&limit=60&include=previewData,labelData";
        self.jobs.push(Job::CatalogAmbient {
            task: net::fetch_get(url, USER_AGENT),
        });
        self.status = Some("loading ambientCG catalog…".into());
    }

    fn start_poly_catalog(&mut self) {
        let url = "https://api.polyhaven.com/assets?t=textures";
        self.jobs.push(Job::CatalogPoly {
            task: net::fetch_get(url, USER_AGENT),
        });
        self.status = Some("loading Poly Haven catalog…".into());
    }

    fn draw_sources(&self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "Free CC0 / open PBR libraries. Integrated ones load in-app.",
            )
            .weak()
            .size(11.0),
        );
        ui.add_space(4.0);
        // Keep the list short and scrollable so Sources never blows the panel.
        egui::ScrollArea::vertical()
            .id_salt("pbr-sources-scroll")
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for src in PBR_SOURCES {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_max_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.strong(src.name);
                            if src.integrated {
                                ui.small(
                                    egui::RichText::new("in-app")
                                        .color(egui::Color32::from_rgb(80, 180, 120)),
                                );
                            }
                        });
                        ui.label(egui::RichText::new(src.license).weak().size(10.0));
                        ui.label(
                            egui::RichText::new(src.notes)
                                .size(10.0)
                                .weak(),
                        );
                        ui.hyperlink_to(src.url, src.url);
                    });
                    ui.add_space(3.0);
                }
            });
    }

    fn draw_remote_browser(
        &mut self,
        ui: &mut egui::Ui,
        _scene: &Scene,
        selection: &Selection,
        source: &str,
    ) {
        let loaded = match source {
            "ambientcg" => self.ambient_loaded,
            _ => self.poly_loaded,
        };
        let err = match source {
            "ambientcg" => self.ambient_error.clone(),
            _ => self.poly_error.clone(),
        };
        let loading = self.catalog_in_flight(source);

        ui.horizontal(|ui| {
            let search_w = (ui.available_width() - 40.0).clamp(80.0, 180.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .desired_width(search_w)
                    .hint_text("Search…"),
            );
            if ui.small_button("↻").on_hover_text("Reload catalog").clicked() {
                match source {
                    "ambientcg" => {
                        self.ambient_loaded = false;
                        self.start_ambient_catalog();
                    }
                    _ => {
                        self.poly_loaded = false;
                        self.start_poly_catalog();
                    }
                }
            }
        });

        // Category as a compact dropdown (chips blew the panel to full width).
        if !self.categories.is_empty() {
            let label = if self.category_filter.is_empty() {
                "All categories".to_string()
            } else {
                self.category_filter.clone()
            };
            egui::ComboBox::from_id_salt(format!("pbr-cat-{source}"))
                .width(ui.available_width().min(220.0))
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.category_filter.is_empty(), "All categories")
                        .clicked()
                    {
                        self.category_filter.clear();
                    }
                    for cat in self.categories.clone() {
                        let selected = self.category_filter == cat;
                        if ui.selectable_label(selected, &cat).clicked() {
                            self.category_filter = if selected {
                                String::new()
                            } else {
                                cat
                            };
                        }
                    }
                });
        }

        if loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading catalog…");
            });
            return;
        }
        if let Some(e) = err {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
        }
        if !loaded {
            if ui.button("Load catalog").clicked() {
                match source {
                    "ambientcg" => self.start_ambient_catalog(),
                    _ => self.start_poly_catalog(),
                }
            }
            return;
        }

        let q = self.search.to_lowercase();
        let cat = self.category_filter.clone();
        let entries: Vec<RemotePbrEntry> = match source {
            "ambientcg" => self.ambient_entries.clone(),
            _ => self.poly_entries.clone(),
        };
        let filtered: Vec<RemotePbrEntry> = entries
            .into_iter()
            .filter(|e| {
                if !cat.is_empty() && !e.category.eq_ignore_ascii_case(&cat) {
                    return false;
                }
                if q.is_empty() {
                    return true;
                }
                e.name.to_lowercase().contains(&q)
                    || e.category.to_lowercase().contains(&q)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
                    || e.source_id.to_lowercase().contains(&q)
            })
            .collect();

        ui.label(
            egui::RichText::new(format!("{} materials", filtered.len()))
                .weak()
                .size(11.0),
        );

        let has_sel = !selection.is_empty();
        // Bound height to leftover space so the panel stays a normal sidebar.
        let list_h = ui.available_height().clamp(140.0, 320.0);
        egui::ScrollArea::vertical()
            .id_salt(format!("pbr-scroll-{source}"))
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width().min(300.0));
                for e in &filtered {
                    self.remote_row(ui, e, has_sel);
                }
            });
    }

    /// Progress of an in-flight import for `source_id`, if any.
    /// Returns `(fraction 0..1, maps_done, maps_total, current_map_label)`.
    fn import_progress(&self, source_id: &str) -> Option<(f32, usize, usize, String)> {
        for job in &self.jobs {
            if let Job::Import {
                source_id: id,
                pending,
                done,
                total_maps,
                current_map,
                task,
                ..
            } = job
            {
                if id != source_id {
                    continue;
                }
                let resolving_index = pending.iter().any(|(m, _)| m == "__files__")
                    || current_map.as_deref() == Some("__files__");
                let completed = done
                    .keys()
                    .filter(|k| k.as_str() != "__files__")
                    .count();
                let in_flight = matches!(
                    (current_map.as_deref(), task.is_some()),
                    (Some(m), true) if m != "__files__"
                );
                let remaining = pending
                    .iter()
                    .filter(|(m, _)| m.as_str() != "__files__")
                    .count();
                let total = if resolving_index {
                    (*total_maps).max(1)
                } else {
                    (*total_maps)
                        .max(completed + usize::from(in_flight) + remaining)
                        .max(1)
                };
                let frac = if resolving_index {
                    0.08
                } else {
                    let base = completed as f32 / total as f32;
                    let f = if in_flight {
                        base + 0.5 / total as f32
                    } else {
                        base
                    };
                    f.clamp(0.0, 0.99)
                };
                let label = match current_map.as_deref() {
                    Some("__files__") | None if resolving_index => "index…".into(),
                    Some(m) if m != "__files__" => m.to_string(),
                    _ => "maps…".into(),
                };
                return Some((frac, completed, total, label));
            }
        }
        None
    }

    /// Compact single-column row: thumb | name/category | Apply +
    fn remote_row(&mut self, ui: &mut egui::Ui, entry: &RemotePbrEntry, has_sel: bool) {
        let thumb_key = format!("thumbs/{}/{}", entry.source, entry.source_id);
        self.ensure_thumb(ui.ctx(), &thumb_key, &entry.thumbnail_url);
        let loading = self.import_progress(&entry.source_id);
        let busy = loading.is_some();

        let response = egui::Frame::group(ui.style())
            .inner_margin(4.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                ui.horizontal(|ui| {
                    // Thumbnail with optional progress overlay
                    let thumb_size = egui::vec2(40.0, 40.0);
                    let (thumb_rect, _) = ui.allocate_exact_size(thumb_size, egui::Sense::hover());
                    if let Some(tex) = self.thumbs.get(&thumb_key) {
                        ui.painter().image(
                            tex.id(),
                            thumb_rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    } else {
                        ui.painter().rect_filled(
                            thumb_rect,
                            2.0,
                            ui.visuals().widgets.inactive.bg_fill,
                        );
                        ui.painter().text(
                            thumb_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "…",
                            egui::FontId::proportional(14.0),
                            ui.visuals().weak_text_color(),
                        );
                    }
                    if busy {
                        // Dim the thumb while downloading
                        ui.painter().rect_filled(
                            thumb_rect,
                            2.0,
                            egui::Color32::from_black_alpha(120),
                        );
                    }

                    ui.vertical(|ui| {
                        let name = truncate_label(&entry.name, 22);
                        ui.label(egui::RichText::new(name).size(12.0).strong());
                        if let Some((frac, done, total, map_label)) = &loading {
                            let bar_w = ui.available_width().clamp(60.0, 140.0);
                            let bar = egui::ProgressBar::new(*frac)
                                .desired_width(bar_w)
                                .desired_height(8.0)
                                .show_percentage();
                            ui.add(bar);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{done}/{total} · {map_label}"
                                ))
                                .weak()
                                .size(10.0),
                            );
                            // Keep animating while download jobs poll each frame.
                            ui.ctx().request_repaint();
                        } else if !entry.category.is_empty() {
                            ui.label(
                                egui::RichText::new(truncate_label(&entry.category, 24))
                                    .weak()
                                    .size(10.0),
                            );
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if busy {
                            ui.add_enabled(false, egui::Button::new("…").small())
                                .on_hover_text("Downloading maps…");
                        } else {
                            if ui
                                .small_button("+")
                                .on_hover_text("Add to local collection")
                                .clicked()
                            {
                                self.start_import(entry, false, true);
                            }
                            if ui
                                .add_enabled(has_sel, egui::Button::new("Apply").small())
                                .on_hover_text(if has_sel {
                                    "Download maps and apply to selection"
                                } else {
                                    "Select an object first"
                                })
                                .clicked()
                            {
                                self.start_import(entry, true, true);
                            }
                        }
                    });
                });
            })
            .response;
        if let Some((frac, done, total, map_label)) = loading {
            response.on_hover_text(format!(
                "Downloading {} — {done}/{total} ({map_label}, {:.0}%)",
                entry.name,
                frac * 100.0
            ));
        } else {
            response.on_hover_text(format!(
                "{} — {} ({})",
                entry.name, entry.category, entry.source_id
            ));
        }
    }

    fn draw_local(&mut self, ui: &mut egui::Ui, _scene: &Scene, selection: &Selection) {
        if self.local.entries.is_empty() {
            ui.weak(
                "Empty — browse ambientCG or Poly Haven and Apply\n\
                 (or +) to save materials here.",
            );
            return;
        }
        ui.horizontal(|ui| {
            let search_w = (ui.available_width() - 8.0).clamp(80.0, 200.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .desired_width(search_w)
                    .hint_text("filter…"),
            );
        });
        let q = self.search.to_lowercase();
        let has_sel = !selection.is_empty();
        let mut remove: Option<String> = None;
        let entries: Vec<LocalPbrEntry> = self
            .local
            .entries
            .iter()
            .filter(|e| {
                q.is_empty()
                    || e.name.to_lowercase().contains(&q)
                    || e.category.to_lowercase().contains(&q)
                    || e.source_id.to_lowercase().contains(&q)
            })
            .cloned()
            .collect();

        let list_h = ui.available_height().clamp(140.0, 320.0);
        egui::ScrollArea::vertical()
            .id_salt("pbr-local-scroll")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for e in &entries {
                    let mut apply = false;
                    let mut delete = false;
                    egui::Frame::group(ui.style())
                        .inner_margin(4.0)
                        .show(ui, |ui| {
                            ui.set_max_width(ui.available_width());
                            ui.horizontal(|ui| {
                                if let Some(thumb) = &e.thumbnail {
                                    if let Some(tex) = self.thumb_from_cache(ui.ctx(), thumb) {
                                        ui.add(
                                            egui::Image::new(tex)
                                                .fit_to_exact_size(egui::vec2(40.0, 40.0)),
                                        );
                                    }
                                }
                                // Name area is also a click-to-apply hit target.
                                let name_resp = ui
                                    .vertical(|ui| {
                                        ui.label(truncate_label(&e.name, 22));
                                        ui.label(
                                            egui::RichText::new(truncate_label(
                                                &format!("{} · {}", e.source, e.category),
                                                26,
                                            ))
                                            .weak()
                                            .size(10.0),
                                        );
                                    })
                                    .response
                                    .interact(egui::Sense::click())
                                    .on_hover_text(if has_sel {
                                        "Click to apply to selection"
                                    } else {
                                        "Select an object first"
                                    });
                                if name_resp.clicked() && has_sel {
                                    apply = true;
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("✖")
                                            .on_hover_text("Remove from local")
                                            .clicked()
                                        {
                                            delete = true;
                                        }
                                        let apply_btn = ui
                                            .add_enabled(
                                                has_sel,
                                                egui::Button::new("Apply"),
                                            )
                                            .on_hover_text(if has_sel {
                                                "Apply maps to selection"
                                            } else {
                                                "Select an object first"
                                            });
                                        if apply_btn.clicked() {
                                            apply = true;
                                        }
                                    },
                                );
                            });
                        });
                    if apply {
                        let mut mat = Material::default();
                        if let Some(c) = e.base_color {
                            mat.base_color = c;
                        }
                        if let Some(r) = e.roughness {
                            mat.roughness = r;
                        }
                        if let Some(m) = e.metallic {
                            mat.metallic = m;
                        }
                        mat.textures = e.textures.clone();
                        self.pending_apply = Some((mat, e.name.clone()));
                        self.status = Some(format!("applying '{}'…", e.name));
                    }
                    if delete {
                        remove = Some(e.id.clone());
                    }
                }
            });
        if let Some(id) = remove {
            self.local.entries.retain(|e| e.id != id);
            save_collection(&self.local);
            self.status = Some("removed from local collection".into());
        }
    }

    fn ensure_thumb(&mut self, ctx: &egui::Context, key: &str, url: &str) {
        if self.thumbs.contains_key(key) || self.thumb_pending.contains_key(key) {
            // Try load from disk cache into GPU if bytes already there.
            if !self.thumbs.contains_key(key) {
                if let Some(bytes) = load_texture_bytes(key) {
                    if let Some(tex) = bytes_to_egui_texture(ctx, key, &bytes) {
                        self.thumbs.insert(key.to_string(), tex);
                    }
                }
            }
            return;
        }
        // Kick off download.
        self.thumb_pending.insert(key.to_string(), ());
        self.jobs.push(Job::Thumb {
            key: key.to_string(),
            task: net::fetch_bytes(url, USER_AGENT),
        });
    }

    fn thumb_from_cache(
        &mut self,
        ctx: &egui::Context,
        key: &str,
    ) -> Option<&egui::TextureHandle> {
        if !self.thumbs.contains_key(key) {
            if let Some(bytes) = load_texture_bytes(key) {
                if let Some(tex) = bytes_to_egui_texture(ctx, key, &bytes) {
                    self.thumbs.insert(key.to_string(), tex);
                }
            }
        }
        self.thumbs.get(key)
    }

    fn start_import(&mut self, entry: &RemotePbrEntry, apply: bool, also_local: bool) {
        // Avoid duplicate imports.
        if self.jobs.iter().any(|j| matches!(j, Job::Import { source_id, .. } if source_id == &entry.source_id)) {
            self.status = Some("import already in progress".into());
            return;
        }
        match entry.source {
            "ambientcg" => {
                let maps = ambientcg_map_urls(&entry.source_id);
                let total_maps = maps.len();
                self.jobs.push(Job::Import {
                    source: "ambientcg",
                    source_id: entry.source_id.clone(),
                    name: entry.name.clone(),
                    category: entry.category.clone(),
                    pending: maps,
                    done: HashMap::new(),
                    total_maps,
                    task: None,
                    current_map: None,
                    apply,
                    also_local,
                });
                self.status = Some(format!("downloading {}…", entry.name));
            }
            "polyhaven" => {
                // Need files API first — schedule a catalog-style GET then maps.
                let url = format!("https://api.polyhaven.com/files/{}", entry.source_id);
                // Reuse Import with a special first "meta" step via pending empty
                // and a separate HttpTask — simpler: fetch files JSON as a Job variant.
                // For simplicity, fire files JSON as Import with pending filled after.
                // Use a two-phase: store source_id and fetch files list via HttpTask
                // embedded as first step using a synthetic approach:
                self.jobs.push(Job::Import {
                    source: "polyhaven",
                    source_id: entry.source_id.clone(),
                    name: entry.name.clone(),
                    category: entry.category.clone(),
                    // Placeholder — will be filled by polyhaven_files job
                    pending: vec![("__files__".into(), url)],
                    done: HashMap::new(),
                    total_maps: 1, // files index; updated when real maps are queued
                    task: None,
                    current_map: None,
                    apply,
                    also_local,
                });
                self.status = Some(format!("downloading {}…", entry.name));
            }
            _ => {}
        }
    }
}

// Special-case: when Import finishes a `__files__` "map", parse Poly Haven
// files JSON instead of treating it as image bytes. Handled by wrapping poll
// — actually we handled it poorly. Fix: detect in poll when map is __files__.

// We'll patch poll logic: when map is __files__, parse JSON and push real map urls.

/// Post-process Poly Haven files JSON (called from a thin wrapper).
fn polyhaven_map_urls_from_files_json(body: &str) -> Result<Vec<(String, String)>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("files json: {e}"))?;
    let mut out = Vec::new();
    // Prefer ARM pack (AO/Rough/Metal in one) + Diffuse + nor_gl at 1k jpg.
    let pick = |map: &str, key: &str| -> Option<String> {
        v.get(map)?
            .get("1k")?
            .get("jpg")?
            .get("url")?
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| {
                // some assets use "Diffuse" vs "diff"
                None
            })
            .or_else(|| {
                let _ = key;
                None
            })
    };
    // Diffuse / albedo
    for name in ["Diffuse", "diff", "Color"] {
        if let Some(url) = pick(name, "albedo") {
            out.push(("albedo".into(), url));
            break;
        }
    }
    // Normal (OpenGL)
    for name in ["nor_gl", "Nor", "normal"] {
        if let Some(url) = pick(name, "normal") {
            out.push(("normal".into(), url));
            break;
        }
    }
    // Prefer separate Rough + AO + metal, else arm pack
    let mut has_rough = false;
    if let Some(url) = pick("Rough", "roughness") {
        out.push(("roughness".into(), url));
        has_rough = true;
    }
    if let Some(url) = pick("AO", "occlusion") {
        out.push(("occlusion".into(), url));
    }
    if let Some(url) = pick("Metal", "metallic").or_else(|| pick("metal", "metallic")) {
        out.push(("metallic".into(), url));
    }
    if !has_rough {
        if let Some(url) = pick("arm", "arm") {
            // Store as roughness; renderer can treat as ORM if needed.
            out.push(("roughness".into(), url.clone()));
            out.push(("occlusion".into(), url.clone()));
            // metallic channel lives in B of ARM — store same for metallic key
            // so pack_orm can split; we pass the arm bytes as roughness and
            // leave metallic scalar. For full ARM, set special handling:
            out.push(("metallic".into(), url));
        }
    }
    if out.is_empty() {
        return Err("no 1k jpg maps found".into());
    }
    Ok(out)
}

fn ambientcg_map_urls(asset_id: &str) -> Vec<(String, String)> {
    // Surface-preview maps (~1K square) — no zip needed, CORS-friendly CDN.
    let base = format!(
        "https://f003.backblazeb2.com/file/ambientCG-Web/media/surface-preview/{id}/{id}_SQ_",
        id = asset_id
    );
    let mut maps = vec![
        ("albedo".into(), format!("{base}Color.jpg")),
        ("normal".into(), format!("{base}NormalGL.jpg")),
        ("roughness".into(), format!("{base}Roughness.jpg")),
    ];
    // AO / metalness are optional (404 for some assets) — still try AO.
    maps.push(("occlusion".into(), format!("{base}AmbientOcclusion.jpg")));
    // Metalness only exists for metal assets; failures are skipped.
    maps.push(("metallic".into(), format!("{base}Metalness.jpg")));
    maps
}

fn parse_ambientcg_catalog(body: &str) -> Result<Vec<RemotePbrEntry>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("json: {e}"))?;
    let assets = v
        .get("foundAssets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| "missing foundAssets".to_string())?;
    let mut out = Vec::with_capacity(assets.len());
    for a in assets {
        let id = a
            .get("assetId")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let name = a
            .get("displayName")
            .and_then(|x| x.as_str())
            .unwrap_or(&id)
            .to_string();
        let category = a
            .get("displayCategory")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let thumb = a
            .get("previewImage")
            .and_then(|p| p.get("256-JPG-FFFFFF").or_else(|| p.get("256-PNG")))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let tags = a
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.push(RemotePbrEntry {
            source: "ambientcg",
            source_id: id,
            name,
            category,
            thumbnail_url: thumb,
            tags,
        });
    }
    Ok(out)
}

fn parse_polyhaven_catalog(body: &str) -> Result<Vec<RemotePbrEntry>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("json: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "expected object".to_string())?;
    let mut out = Vec::with_capacity(obj.len());
    for (id, a) in obj {
        let name = a
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or(id)
            .to_string();
        let categories = a
            .get("categories")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let category = categories.first().cloned().unwrap_or_default();
        let thumb = a
            .get("thumbnail_url")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                format!(
                    "https://cdn.polyhaven.com/asset_img/thumbs/{id}.png?width=256&height=256"
                )
            });
        let tags = a
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or(categories);
        out.push(RemotePbrEntry {
            source: "polyhaven",
            source_id: id.clone(),
            name,
            category,
            thumbnail_url: thumb,
            tags,
        });
    }
    // Popular first when download_count present
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    // Cap list size for UI snappiness (full list can be 800+)
    if out.len() > 120 {
        out.truncate(120);
    }
    Ok(out)
}

fn collect_categories(entries: &[RemotePbrEntry]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for e in entries {
        if !e.category.is_empty() {
            set.insert(e.category.clone());
        }
    }
    set.into_iter().collect()
}

fn bytes_to_egui_texture(
    ctx: &egui::Context,
    id: &str,
    bytes: &[u8],
) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.thumbnail(128, 128).to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture(id, pixels, egui::TextureOptions::LINEAR))
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let take = max_chars.saturating_sub(1);
        format!("{}…", s.chars().take(take).collect::<String>())
    }
}

fn sample_average_color(bytes: &[u8]) -> Option<[f32; 3]> {
    let img = image::load_from_memory(bytes).ok()?.thumbnail(32, 32).to_rgba8();
    let mut r = 0u64;
    let mut g = 0u64;
    let mut b = 0u64;
    let n = img.pixels().len().max(1) as u64;
    for p in img.pixels() {
        r += p.0[0] as u64;
        g += p.0[1] as u64;
        b += p.0[2] as u64;
    }
    // Approximate sRGB → linear for base_color authoring
    fn srgb_to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    Some([
        srgb_to_linear(r as f32 / n as f32 / 255.0),
        srgb_to_linear(g as f32 / n as f32 / 255.0),
        srgb_to_linear(b as f32 / n as f32 / 255.0),
    ])
}

// --- Fix Poly Haven __files__ handling by re-exporting a poll helper used above
// The Import job stores JSON body under map "__files__" then we need to expand.
// Patch: enhance poll for Import when done contains __files__.

impl PbrLibraryPanel {
    /// Call after `poll` to expand any completed Poly Haven files JSON.
    pub fn expand_poly_files(&mut self) {
        for job in &mut self.jobs {
            if let Job::Import {
                source,
                pending,
                done,
                total_maps,
                ..
            } = job
            {
                if *source == "polyhaven" {
                    if let Some(bytes) = done.remove("__files__") {
                        let body = String::from_utf8_lossy(&bytes);
                        match polyhaven_map_urls_from_files_json(&body) {
                            Ok(maps) => {
                                *total_maps = maps.len().max(1);
                                pending.extend(maps);
                            }
                            Err(e) => {
                                self.status = Some(format!("Poly Haven files: {e}"));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Decode a cached map into a three-d `CpuTexture`.
pub fn cpu_texture_from_key(key: &str) -> Option<three_d::CpuTexture> {
    use crate::gfx::{CpuTexture, TextureData};
    let bytes = load_texture_bytes(key)?;
    let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (width, height) = rgba.dimensions();
    let data: Vec<[u8; 4]> = rgba.pixels().map(|p| p.0).collect();
    Some(CpuTexture {
        data: TextureData::RgbaU8(data),
        width,
        height,
        ..Default::default()
    })
}

/// Pack separate roughness / metallic / AO greyscale maps into glTF ORM
/// (R=occlusion, G=roughness, B=metallic).
pub fn pack_orm(
    occlusion: Option<&three_d::CpuTexture>,
    roughness: Option<&three_d::CpuTexture>,
    metallic: Option<&three_d::CpuTexture>,
) -> Option<three_d::CpuTexture> {
    use crate::gfx::{CpuTexture, TextureData};
    let (w, h, rough_px) = match roughness {
        Some(t) => {
            let TextureData::RgbaU8(px) = &t.data else {
                return None;
            };
            (t.width, t.height, px.clone())
        }
        None => return None,
    };
    let occ_px = occlusion.and_then(|t| {
        if t.width == w && t.height == h {
            if let TextureData::RgbaU8(px) = &t.data {
                Some(px.clone())
            } else {
                None
            }
        } else {
            None
        }
    });
    let met_px = metallic.and_then(|t| {
        if t.width == w && t.height == h {
            if let TextureData::RgbaU8(px) = &t.data {
                Some(px.clone())
            } else {
                None
            }
        } else {
            None
        }
    });
    let mut out = Vec::with_capacity(rough_px.len());
    for i in 0..rough_px.len() {
        let o = occ_px.as_ref().map(|p| p[i][0]).unwrap_or(255);
        let r = rough_px[i][0];
        let m = met_px.as_ref().map(|p| p[i][0]).unwrap_or(0);
        out.push([o, r, m, 255]);
    }
    Some(CpuTexture {
        data: TextureData::RgbaU8(out),
        width: w,
        height: h,
        ..Default::default()
    })
}
