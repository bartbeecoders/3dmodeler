//! Persisted application settings and the Preferences window
//! (Edit ▸ Preferences…, Blender-style: tab rail on the left, one page per
//! category — add future settings as new tabs or new sections in a tab).
//!
//! Settings are stored as JSON: native in the user's config dir next to the
//! recent-files list, web in localStorage. `#[serde(default)]` keeps old
//! settings files loadable when new fields are added.

use serde::{Deserialize, Serialize};
use three_d::egui;

/// Display unit for lengths. The world unit is ALWAYS 1 meter (box3d and the
/// scene format store meters); the unit only changes how values are shown
/// and typed. Metric only, by design.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Unit {
    Meters,
    Centimeters,
    Millimeters,
}

impl Unit {
    pub const ALL: [Unit; 3] = [Unit::Meters, Unit::Centimeters, Unit::Millimeters];

    pub fn label(self) -> &'static str {
        match self {
            Unit::Meters => "Meter",
            Unit::Centimeters => "Centimeter",
            Unit::Millimeters => "Millimeter",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Unit::Meters => "m",
            Unit::Centimeters => "cm",
            Unit::Millimeters => "mm",
        }
    }

    pub fn per_meter(self) -> f32 {
        match self {
            Unit::Meters => 1.0,
            Unit::Centimeters => 100.0,
            Unit::Millimeters => 1000.0,
        }
    }

    /// Digits shown after the decimal point — constant physical precision
    /// (1 mm) regardless of unit.
    pub fn decimals(self) -> usize {
        match self {
            Unit::Meters => 3,
            Unit::Centimeters => 1,
            Unit::Millimeters => 0,
        }
    }

    pub fn from_meters(self, meters: f32) -> f32 {
        meters * self.per_meter()
    }

    pub fn to_meters(self, value: f32) -> f32 {
        value / self.per_meter()
    }

    /// A length in meters formatted in this unit, suffix included.
    pub fn format(self, meters: f32) -> String {
        format!(
            "{:.prec$} {}",
            self.from_meters(meters),
            self.suffix(),
            prec = self.decimals()
        )
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Grid line spacing in meters (world units).
    pub grid_spacing: f32,
    pub grid_minor_color: [u8; 3],
    pub grid_major_color: [u8; 3],
    pub unit: Unit,
    /// Starting directory for Save/Open dialogs (native only).
    pub default_save_dir: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            grid_spacing: 1.0,
            grid_minor_color: [58, 61, 66],
            grid_major_color: [76, 80, 87],
            unit: Unit::Meters,
            default_save_dir: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        read_store()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            write_store(&json);
        }
    }

    pub fn save_dir(&self) -> Option<std::path::PathBuf> {
        self.default_save_dir
            .as_ref()
            .filter(|d| !d.trim().is_empty())
            .map(std::path::PathBuf::from)
    }
}

// --- storage backends --------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
fn settings_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("box3d-modeler").join("settings.json"))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_store() -> Option<String> {
    std::fs::read_to_string(settings_path()?).ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn write_store(json: &str) {
    let Some(path) = settings_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "modeler_settings";

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

// --- folder picker (native, async like io.rs dialogs) ------------------------

#[cfg(not(target_arch = "wasm32"))]
static PENDING_DIR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Show a folder picker on a background thread (a blocking rfd dialog inside
/// the render loop freezes winit — see io.rs). Result arrives via
/// `poll_pick_dir` on a later frame.
#[cfg(not(target_arch = "wasm32"))]
fn request_pick_dir(start: Option<std::path::PathBuf>) {
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new();
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_folder() {
            if let Ok(mut pending) = PENDING_DIR.lock() {
                *pending = Some(path.display().to_string());
            }
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn poll_pick_dir() -> Option<String> {
    PENDING_DIR.lock().ok().and_then(|mut p| p.take())
}

// --- Preferences window ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Viewport,
    Units,
    Files,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Viewport, Tab::Units, Tab::Files];

    fn label(self) -> &'static str {
        match self {
            Tab::Viewport => "Viewport",
            Tab::Units => "Units",
            Tab::Files => "Files",
        }
    }
}

pub struct SettingsWindow {
    pub open: bool,
    tab: Tab,
}

impl SettingsWindow {
    pub fn new() -> Self {
        Self {
            open: false,
            tab: Tab::Viewport,
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, settings: &mut Settings) {
        if !self.open {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dir) = poll_pick_dir() {
            settings.default_save_dir = Some(dir);
        }

        let mut open = self.open;
        egui::Window::new("Preferences")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([720.0, 500.0])
            .min_width(640.0)
            .min_height(420.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    // tab rail (add future categories here)
                    ui.vertical(|ui| {
                        ui.set_width(140.0);
                        for tab in Tab::ALL {
                            if ui
                                .selectable_label(self.tab == tab, tab.label())
                                .clicked()
                            {
                                self.tab = tab;
                            }
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.set_min_height(400.0);
                        egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                            Tab::Viewport => viewport_tab(ui, settings),
                            Tab::Units => units_tab(ui, settings),
                            Tab::Files => files_tab(ui, settings),
                        });
                    });
                });
            });
        self.open = open;
    }
}

fn viewport_tab(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.heading("Grid");
    ui.add_space(6.0);

    egui::Grid::new("grid-settings")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("Spacing");
            let unit = settings.unit;
            let mut value = unit.from_meters(settings.grid_spacing);
            if ui
                .add(
                    egui::DragValue::new(&mut value)
                        .speed(0.05 * unit.per_meter() as f64)
                        .range(unit.from_meters(0.05)..=unit.from_meters(10.0))
                        .suffix(format!(" {}", unit.suffix())),
                )
                .changed()
            {
                settings.grid_spacing = unit.to_meters(value).clamp(0.05, 10.0);
            }
            ui.end_row();

            ui.label("Minor line color");
            ui.color_edit_button_srgb(&mut settings.grid_minor_color);
            ui.end_row();

            ui.label("Major line color");
            ui.color_edit_button_srgb(&mut settings.grid_major_color);
            ui.end_row();
        });

    ui.add_space(10.0);
    if ui.button("Reset grid to defaults").clicked() {
        let defaults = Settings::default();
        settings.grid_spacing = defaults.grid_spacing;
        settings.grid_minor_color = defaults.grid_minor_color;
        settings.grid_major_color = defaults.grid_major_color;
    }
    ui.add_space(4.0);
    ui.weak("Every 10th line uses the major color; the X and Y axes keep their red/green.");
}

fn units_tab(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.heading("Units");
    ui.add_space(6.0);
    ui.label("The world unit is fixed: 1 unit = 1 meter (metric only).");
    ui.label("The display unit changes how lengths are shown and typed:");
    ui.add_space(8.0);
    for unit in Unit::ALL {
        ui.radio_value(
            &mut settings.unit,
            unit,
            format!("{} ({})", unit.label(), unit.suffix()),
        );
    }
    ui.add_space(10.0);
    ui.weak(format!(
        "Example: a 1 m edge reads as {}.",
        settings.unit.format(1.0)
    ));
    ui.weak("Applies to measurements, dimensions, the grid label and typed transform values.");
}

#[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
fn files_tab(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.heading("Files");
    ui.add_space(6.0);
    ui.label("Default save location");
    ui.add_space(4.0);

    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut dir = settings.default_save_dir.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            if ui
                .add(egui::TextEdit::singleline(&mut dir).desired_width(380.0))
                .changed()
            {
                settings.default_save_dir =
                    (!dir.trim().is_empty()).then(|| dir.trim().to_string());
            }
            if ui.button("Browse…").clicked() {
                request_pick_dir(settings.save_dir());
            }
            if ui.button("Clear").clicked() {
                settings.default_save_dir = None;
            }
        });
        match settings.save_dir() {
            Some(path) if !path.is_dir() => {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 160, 80),
                    "⚠ this folder does not exist — dialogs will fall back to the last used location",
                );
            }
            Some(_) => {
                ui.weak("Save and Open dialogs start in this folder.");
            }
            None => {
                ui.weak("Not set — dialogs open in the last used location.");
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        ui.weak("Not available in the browser build — files are saved via downloads.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_conversions_roundtrip() {
        for unit in Unit::ALL {
            let meters = 1.234;
            let shown = unit.from_meters(meters);
            assert!((unit.to_meters(shown) - meters).abs() < 1e-6);
        }
        assert_eq!(Unit::Centimeters.from_meters(0.25), 25.0);
        assert_eq!(Unit::Millimeters.to_meters(500.0), 0.5);
    }

    #[test]
    fn unit_formatting() {
        assert_eq!(Unit::Meters.format(1.5), "1.500 m");
        assert_eq!(Unit::Centimeters.format(1.5), "150.0 cm");
        assert_eq!(Unit::Millimeters.format(1.5), "1500 mm");
    }

    #[test]
    fn settings_json_roundtrip_and_defaults() {
        let mut s = Settings::default();
        s.unit = Unit::Centimeters;
        s.grid_spacing = 0.25;
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(back == s);
        // old/partial settings files still load thanks to serde(default)
        let partial: Settings = serde_json::from_str(r#"{"grid_spacing": 2.0}"#).unwrap();
        assert_eq!(partial.grid_spacing, 2.0);
        assert_eq!(partial.unit, Unit::Meters);
    }
}
