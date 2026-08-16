//! Dockable panel layout: which panel lives in which dock node, tab
//! grouping, floating panel windows and layout persistence.
//!
//! The 3D viewport is itself a tab in the dock tree — transparent (the
//! scene renders underneath egui and shows through), non-closable and
//! pinned to the main surface. Its body rect is what the overlays and
//! the pointer-over-UI test treat as "the viewport", so panels can be
//! dragged, stacked as tabs or torn off into floating windows and the
//! viewport math follows automatically.

use crate::gfx::egui;
use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};

/// Every dockable panel. `Viewport` is special: transparent background,
/// cannot be closed and never leaves the main surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PanelId {
    Viewport,
    Outliner,
    Library,
    Properties,
    PbrLibrary,
    AiChat,
}

impl PanelId {
    pub fn title(self) -> &'static str {
        match self {
            Self::Viewport => "Viewport",
            Self::Outliner => "Outliner",
            Self::Library => "Library",
            Self::Properties => "Properties",
            Self::PbrLibrary => "PBR Library",
            Self::AiChat => "AI Assistant",
        }
    }

    /// Panels the user can open and close (View menu); the viewport is fixed.
    pub const CLOSABLE: [PanelId; 5] = [
        PanelId::Outliner,
        PanelId::Library,
        PanelId::Properties,
        PanelId::PbrLibrary,
        PanelId::AiChat,
    ];
}

/// The dock tree plus persistence.
pub struct DockLayout {
    /// `Option` so `UiState::draw` can move the state out while the tab
    /// viewer borrows the rest of `UiState`, putting it back afterwards.
    pub state: Option<DockState<PanelId>>,
    /// Serialized form at the last save — layout changes (drags, resizes,
    /// closed tabs) are detected by comparing against this.
    saved: String,
}

/// The out-of-the-box arrangement: viewport left, a right dock split into
/// scene panels (top) and object panels (bottom), each a tab group.
fn default_state() -> DockState<PanelId> {
    let mut state = DockState::new(vec![PanelId::Viewport]);
    let tree = state.main_surface_mut();
    let [_, right] = tree.split_right(
        NodeIndex::root(),
        0.78,
        vec![PanelId::Outliner, PanelId::Library],
    );
    tree.split_below(right, 0.45, vec![PanelId::Properties, PanelId::PbrLibrary]);
    state
}

impl DockLayout {
    pub fn load() -> Self {
        let state = read_store()
            .and_then(|json| serde_json::from_str::<DockState<PanelId>>(&json).ok())
            // a stored layout without a viewport tab (corrupt / stale) would
            // leave no hole to see the scene through — fall back to default
            .filter(|s| s.find_tab(&PanelId::Viewport).is_some())
            .unwrap_or_else(default_state);
        Self {
            state: Some(state),
            saved: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.state = Some(default_state());
    }

    pub fn is_open(&self, panel: PanelId) -> bool {
        self.state
            .as_ref()
            .is_some_and(|s| s.find_tab(&panel).is_some())
    }

    /// Open (dock in a sensible spot, or focus if already open) or close a
    /// panel. The viewport can't be closed.
    pub fn set_open(&mut self, panel: PanelId, open: bool) {
        if panel == PanelId::Viewport {
            return;
        }
        let Some(state) = self.state.as_mut() else { return };
        if open {
            if let Some(path) = state.find_tab(&panel) {
                let _ = state.set_active_tab(path);
                return;
            }
            match panel {
                // chat docks along the left edge, like the old fixed panel
                PanelId::AiChat => {
                    state
                        .main_surface_mut()
                        .split_left(NodeIndex::root(), 0.22, vec![panel]);
                }
                _ => {
                    // join the bottom-most main-surface tab group that holds
                    // other side panels; else open a fresh right split
                    let target = state
                        .iter_leaves()
                        .filter(|(path, leaf)| {
                            path.surface.is_main()
                                && !leaf.tabs.contains(&PanelId::Viewport)
                                && !leaf.tabs.contains(&PanelId::AiChat)
                        })
                        .map(|(path, _)| path)
                        .last();
                    match target {
                        Some(path) => {
                            state.set_focused_node_and_surface(path);
                            state.push_to_focused_leaf(panel);
                        }
                        None => {
                            state
                                .main_surface_mut()
                                .split_right(NodeIndex::root(), 0.78, vec![panel]);
                        }
                    }
                }
            }
        } else if let Some(path) = state.find_tab(&panel) {
            state.remove_tab(path);
        }
    }

    /// Persist the layout when it changed. Skipped while a pointer button is
    /// down — node rects churn every frame during a drag or resize.
    pub fn save_if_changed(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        let Some(state) = self.state.as_ref() else { return };
        let Ok(json) = serde_json::to_string(state) else { return };
        if json != self.saved {
            write_store(&json);
            self.saved = json;
        }
    }
}

// --- storage backends (same scheme as settings.rs) ---------------------------

#[cfg(not(target_arch = "wasm32"))]
fn layout_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("box3d-modeler").join("layout.json"))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_store() -> Option<String> {
    std::fs::read_to_string(layout_path()?).ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn write_store(json: &str) {
    let Some(path) = layout_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "modeler_layout";

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
