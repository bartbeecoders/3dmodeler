//! Color themes for the editor UI.
//!
//! Each theme is a complete egui style (panels, widgets, selection) plus the
//! colors the app draws itself: the viewport clear color, the accent used for
//! section headers / highlights, and the ok/warn/error status colors.
//!
//! The accent is also stored in `visuals.hyperlink_color`, so widgets that
//! only have a `Ui` at hand (outliner drag highlight, section headers) can
//! read it back without threading `Settings` through every call.

use serde::{Deserialize, Serialize};
use three_d::egui;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Theme {
    /// Graphite panels with a warm amber accent (default).
    Dark,
    /// Bright panels with a blue accent.
    Light,
    /// Deep blue panels with a cyan accent.
    Ocean,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::Dark
    }
}

/// Everything a theme defines beyond the stock egui visuals.
pub struct Palette {
    pub dark: bool,
    pub accent: egui::Color32,
    pub selection_bg: egui::Color32,
    pub selection_fg: egui::Color32,
    pub panel: egui::Color32,
    pub window: egui::Color32,
    pub extreme: egui::Color32,
    pub faint: egui::Color32,
    pub ok: egui::Color32,
    pub warn: egui::Color32,
    pub err: egui::Color32,
    /// Viewport clear color, linear-ish RGB as passed to `ClearState`.
    pub viewport: [f32; 3],
}

impl Palette {
    pub fn viewport_color32(&self) -> egui::Color32 {
        egui::Color32::from_rgb(
            (self.viewport[0] * 255.0) as u8,
            (self.viewport[1] * 255.0) as u8,
            (self.viewport[2] * 255.0) as u8,
        )
    }
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::Dark, Theme::Light, Theme::Ocean];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::Ocean => "Ocean",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Theme::Dark => "Graphite panels, amber accent",
            Theme::Light => "Bright panels, blue accent",
            Theme::Ocean => "Deep blue panels, cyan accent",
        }
    }

    pub fn palette(self) -> Palette {
        use egui::Color32;
        match self {
            Theme::Dark => Palette {
                dark: true,
                accent: Color32::from_rgb(255, 160, 60),
                selection_bg: Color32::from_rgb(140, 80, 24),
                selection_fg: Color32::from_rgb(255, 238, 216),
                panel: Color32::from_rgb(30, 32, 36),
                window: Color32::from_rgb(36, 38, 43),
                extreme: Color32::from_rgb(16, 17, 20),
                faint: Color32::from_rgb(39, 42, 47),
                ok: Color32::from_rgb(90, 220, 110),
                warn: Color32::from_rgb(255, 200, 120),
                err: Color32::from_rgb(235, 100, 90),
                viewport: [0.12, 0.13, 0.16],
            },
            Theme::Light => Palette {
                dark: false,
                accent: Color32::from_rgb(35, 100, 210),
                selection_bg: Color32::from_rgb(198, 218, 250),
                selection_fg: Color32::from_rgb(18, 48, 110),
                panel: Color32::from_rgb(237, 239, 242),
                window: Color32::from_rgb(246, 247, 249),
                extreme: Color32::from_rgb(253, 253, 255),
                faint: Color32::from_rgb(226, 229, 235),
                ok: Color32::from_rgb(25, 140, 60),
                warn: Color32::from_rgb(180, 110, 15),
                err: Color32::from_rgb(200, 45, 40),
                viewport: [0.84, 0.855, 0.88],
            },
            Theme::Ocean => Palette {
                dark: true,
                accent: Color32::from_rgb(86, 196, 224),
                selection_bg: Color32::from_rgb(24, 90, 110),
                selection_fg: Color32::from_rgb(216, 244, 252),
                panel: Color32::from_rgb(26, 32, 42),
                window: Color32::from_rgb(31, 38, 50),
                extreme: Color32::from_rgb(13, 17, 24),
                faint: Color32::from_rgb(34, 42, 54),
                ok: Color32::from_rgb(95, 220, 140),
                warn: Color32::from_rgb(255, 205, 130),
                err: Color32::from_rgb(240, 110, 100),
                viewport: [0.07, 0.095, 0.14],
            },
        }
    }

    pub fn visuals(self) -> egui::Visuals {
        let palette = self.palette();
        let mut visuals = if palette.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.selection.bg_fill = palette.selection_bg;
        visuals.selection.stroke = egui::Stroke::new(1.0, palette.selection_fg);
        visuals.hyperlink_color = palette.accent;
        visuals.panel_fill = palette.panel;
        visuals.window_fill = palette.window;
        visuals.extreme_bg_color = palette.extreme;
        visuals.faint_bg_color = palette.faint;
        visuals.code_bg_color = palette.extreme;
        visuals.warn_fg_color = palette.warn;
        visuals.error_fg_color = palette.err;
        visuals.slider_trailing_fill = true; // sliders fill with the accent
        // accent outline on hovered / active widgets
        visuals.widgets.hovered.bg_stroke =
            egui::Stroke::new(1.0, palette.accent.gamma_multiply(0.6));
        visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette.accent);
        // menus / windows pick up the window fill; keep popups in sync
        visuals.widgets.noninteractive.bg_fill = palette.panel;
        visuals
    }

    /// Make this theme the active egui style.
    pub fn apply(self, ctx: &egui::Context) {
        let visuals = self.visuals();
        let egui_theme = egui::Theme::from_dark_mode(visuals.dark_mode);
        ctx.set_theme(egui_theme); // pin it — never follow the system theme
        ctx.set_visuals_of(egui_theme, visuals);
    }

    pub fn viewport_clear(self) -> [f32; 3] {
        self.palette().viewport
    }
}

/// Accent-colored sidebar section header ("Outliner", "Library", …).
/// Reads the accent back from the style so callers only need a `Ui`.
pub fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(text)
            .strong()
            .size(12.5)
            .color(accent(ui)),
    );
    ui.add_space(2.0);
}

/// The active theme's accent color (stored in the hyperlink color).
pub fn accent(ui: &egui::Ui) -> egui::Color32 {
    ui.visuals().hyperlink_color
}

/// Small rounded color swatch, used by the theme picker in Preferences.
pub fn swatch(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 14.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 3.0, color);
    ui.painter().rect_stroke(
        rect,
        3.0,
        ui.visuals().window_stroke,
        egui::StrokeKind::Inside,
    );
}
