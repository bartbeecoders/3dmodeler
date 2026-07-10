//! The AI assistant chat panel (left dock): provider/model configuration
//! with a price-grouped model picker, the conversation log with expandable
//! error details and per-interaction costs, and the message input.

use super::{cost_summary, ChatSession, Entry};
use crate::settings::Settings;
use crate::theme;
use modeler_ai::{format_usd, ModelInfo, ProviderKind};
use three_d::egui;

pub struct ChatPanel {
    pub open: bool,
    show_config: bool,
    input: String,
    /// Re-focus the input after sending.
    focus_input: bool,
    /// Substring filter for the model list (large catalogs).
    model_filter: String,
}

/// Price bands for the model picker, by input price ($ per million tokens).
/// Index = position in the picker, top to bottom.
const PRICE_BANDS: [&str; 6] = [
    "Free",
    "Up to $1 / MTok",
    "$1 – $3 / MTok",
    "$3 – $10 / MTok",
    "Over $10 / MTok",
    "Price unknown",
];

fn price_band(model: &ModelInfo) -> usize {
    match (model.input_per_mtok, model.output_per_mtok) {
        (Some(input), Some(output)) if input == 0.0 && output == 0.0 => 0,
        (Some(input), _) if input <= 1.0 => 1,
        (Some(input), _) if input <= 3.0 => 2,
        (Some(input), _) if input <= 10.0 => 3,
        (Some(_), _) => 4,
        (None, _) => 5,
    }
}

/// Compact `$in / $out` tag for a model row.
fn short_price(model: &ModelInfo) -> String {
    match (model.input_per_mtok, model.output_per_mtok) {
        (Some(input), Some(output)) if input == 0.0 && output == 0.0 => "free".into(),
        (Some(input), Some(output)) => format!("${input:.2} / ${output:.2}"),
        _ => "$?".into(),
    }
}

/// Tool-capability tag for a model row ("can it drive the modeler?").
fn tools_tag(model: &ModelInfo) -> &'static str {
    match model.tools {
        Some(true) => "  ·  tools ✔",
        Some(false) => "  ·  no tools",
        None => "",
    }
}

/// The panel's text is a size up from the app default — chat is prose, not
/// chrome, and API keys / model ids need to be legible.
fn bigger_text(ui: &mut egui::Ui) {
    let styles = &mut ui.style_mut().text_styles;
    let mut bump = |style: egui::TextStyle, size: f32| {
        if let Some(font) = styles.get_mut(&style) {
            font.size = size;
        }
    };
    bump(egui::TextStyle::Body, 15.0);
    bump(egui::TextStyle::Button, 15.0);
    bump(egui::TextStyle::Monospace, 13.5);
    bump(egui::TextStyle::Small, 11.5);
}

impl ChatPanel {
    pub fn new() -> Self {
        Self {
            open: false,
            show_config: false,
            input: String::new(),
            focus_input: false,
            model_filter: String::new(),
        }
    }

    /// Draw the panel; returns its width (the viewport's left offset).
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        session: &mut ChatSession,
        settings: &mut Settings,
    ) -> f32 {
        if !self.open {
            return 0.0;
        }
        // until a model is picked the config IS the panel content
        let configured = !settings
            .ai
            .state(settings.ai.active)
            .map(|s| s.model.trim().is_empty())
            .unwrap_or(true);
        let show_config = self.show_config || !configured;

        #[allow(deprecated)]
        let response = egui::Panel::left("ai_chat")
            .default_size(340.0)
            .size_range(240.0..=520.0) // never squeeze the viewport out
            .show(ctx, |ui| {
                bigger_text(ui);
                ui.horizontal(|ui| {
                    theme::section_header(ui, "AI Assistant");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .selectable_label(show_config, "⚙")
                            .on_hover_text("Provider & model settings")
                            .clicked()
                        {
                            self.show_config = !show_config;
                        }
                        // the active model, as a compact reminder
                        if let Some(state) = settings.ai.state(settings.ai.active) {
                            if !state.model.is_empty() {
                                ui.weak(egui::RichText::new(&state.model).small());
                            }
                        }
                    });
                });
                ui.separator();
                if show_config {
                    self.config_ui(ui, session, settings);
                    ui.separator();
                }
                // bottom-up: footer, input and status pin to the panel
                // bottom; the log scrolls in whatever space remains
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    self.input_ui(ui, session, settings, configured);
                    ui.add_space(2.0);
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                if session.entries.is_empty() {
                                    ui.add_space(8.0);
                                    ui.weak("Ask for anything: “recreate the Eiffel tower”,");
                                    ui.weak("“make it taller”, “add some lights”,");
                                    ui.weak("“make it night time”…");
                                }
                                for (index, entry) in session.entries.iter().enumerate() {
                                    entry_ui(ui, index, entry);
                                }
                                ui.add_space(4.0);
                            });
                    });
                });
            });
        response.response.rect.width()
    }

    /// Bottom block, drawn bottom-up: cost footer, then the input row, then
    /// the busy status row (which thus sits directly above the input).
    fn input_ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut ChatSession,
        settings: &mut Settings,
        configured: bool,
    ) {
        // session footer: running total + clear (bottom row)
        ui.horizontal(|ui| {
            ui.weak(egui::RichText::new(cost_summary(session)).small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !session.entries.is_empty()
                    && !session.busy()
                    && ui.small_button("Clear").on_hover_text("New conversation").clicked()
                {
                    session.clear();
                }
            });
        });

        // input row
        let mut send = false;
        ui.horizontal(|ui| {
            let can_send = configured && !session.busy();
            let inner = ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    let clicked = ui
                        .add_enabled(can_send, egui::Button::new("Send"))
                        .clicked();
                    let edit = ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::singleline(&mut self.input)
                            .hint_text("ask the assistant…"),
                    );
                    if self.focus_input {
                        edit.request_focus();
                        self.focus_input = false;
                    }
                    let entered = edit.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    clicked || entered
                },
            );
            send = inner.inner && can_send;
        });
        if send && !self.input.trim().is_empty() {
            let text = self.input.clone();
            session.send(&text, settings);
            self.input.clear();
            self.focus_input = true;
        }

        // status row while the model works (top of the bottom block)
        if let Some(status) = session.status_line() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.weak(status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("⏹").on_hover_text("Stop").clicked() {
                        session.cancel();
                    }
                });
            });
        }
    }

    // --- provider & model configuration --------------------------------------

    fn config_ui(&mut self, ui: &mut egui::Ui, session: &mut ChatSession, settings: &mut Settings) {
        egui::Grid::new("ai-config")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Provider");
                let active = settings.ai.active;
                egui::ComboBox::from_id_salt("ai-provider")
                    .selected_text(active.label())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for kind in ProviderKind::ALL {
                            ui.selectable_value(&mut settings.ai.active, kind, kind.label());
                        }
                    });
                ui.end_row();

                let kind = settings.ai.active;
                let state = settings.ai.state_mut(kind);

                ui.label("API key");
                ui.add(
                    egui::TextEdit::singleline(&mut state.config.api_key)
                        .password(true)
                        .hint_text(kind.key_hint())
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label("Endpoint");
                ui.add(
                    egui::TextEdit::singleline(&mut state.config.base_url)
                        .hint_text(kind.default_base_url())
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });

        ui.add_space(6.0);
        let kind = settings.ai.active;

        // model catalog: fetch + status row
        ui.horizontal(|ui| {
            ui.label("Model");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if session.fetching_models() {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.weak("fetching…");
                } else if ui
                    .button("Fetch models")
                    .on_hover_text("List this provider's models (uses the API key)")
                    .clicked()
                {
                    session.fetch_models(settings, kind);
                }
            });
        });
        if let Some(error) = &session.model_fetch_error {
            ui.colored_label(ui.visuals().warn_fg_color, error);
        }

        let state = settings.ai.state_mut(kind);
        if state.models.is_empty() {
            ui.add(
                egui::TextEdit::singleline(&mut state.model)
                    .hint_text("fetch models, or type a model id")
                    .desired_width(f32::INFINITY),
            );
        } else {
            self.model_list_ui(ui, state);
        }

        // the chosen model's full price tag + what it can do here
        if let Some(model) = state.models.iter().find(|m| m.id == state.model) {
            ui.weak(egui::RichText::new(model.price_label()).small());
            match model.tools {
                Some(true) => {
                    ui.weak(egui::RichText::new(
                        "Tool use ✔ — the model can inspect and edit the scene.",
                    ).small());
                }
                Some(false) => {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        egui::RichText::new(
                            "No tool use — chat only, this model cannot edit the scene.",
                        )
                        .small(),
                    );
                }
                None => {
                    ui.weak(egui::RichText::new(
                        "Tool support unknown (depends on the loaded model) — tools will be offered.",
                    ).small());
                }
            }
        }
        match kind {
            ProviderKind::Anthropic | ProviderKind::OpenAi => {
                ui.weak(egui::RichText::new(
                    "Prices are a built-in approximation — this provider's API does not publish them.",
                ).small());
            }
            _ => {}
        }
        #[cfg(target_arch = "wasm32")]
        ui.weak(egui::RichText::new(
            "Browser build: some providers (e.g. OpenAI) block direct browser calls (CORS); \
             Anthropic and OpenRouter work.",
        ).small());
        ui.weak(egui::RichText::new(
            "The key is stored in the app settings on this machine.",
        ).small());
    }

    /// The fetched catalog as a filterable list, grouped by price band.
    fn model_list_ui(&mut self, ui: &mut egui::Ui, state: &mut crate::settings::AiProviderState) {
        ui.add(
            egui::TextEdit::singleline(&mut self.model_filter)
                .hint_text(format!("filter {} models…", state.models.len()))
                .desired_width(f32::INFINITY),
        );
        let filter = self.model_filter.trim().to_lowercase();
        let matches = |m: &ModelInfo| {
            filter.is_empty()
                || m.id.to_lowercase().contains(&filter)
                || m.name.to_lowercase().contains(&filter)
        };

        egui::ScrollArea::vertical()
            .id_salt("ai-model-list")
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let mut shown = 0usize;
                for (band, band_label) in PRICE_BANDS.iter().enumerate() {
                    let mut members: Vec<&ModelInfo> = state
                        .models
                        .iter()
                        .filter(|m| price_band(m) == band && matches(m))
                        .collect();
                    if members.is_empty() {
                        continue;
                    }
                    members.sort_by(|a, b| {
                        a.input_per_mtok
                            .unwrap_or(f64::MAX)
                            .total_cmp(&b.input_per_mtok.unwrap_or(f64::MAX))
                            .then_with(|| a.name.cmp(&b.name))
                    });
                    shown += members.len();
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(*band_label)
                            .small()
                            .strong()
                            .color(theme::accent(ui)),
                    );
                    for model in members {
                        let selected = state.model == model.id;
                        let row = format!(
                            "{}   ·   {}{}",
                            model.name,
                            short_price(model),
                            tools_tag(model)
                        );
                        if ui
                            .selectable_label(selected, row)
                            .on_hover_text(&model.id)
                            .clicked()
                        {
                            state.model = model.id.clone();
                        }
                    }
                }
                if shown == 0 {
                    ui.add_space(6.0);
                    ui.weak("no model matches the filter");
                }
            });
    }
}

fn entry_ui(ui: &mut egui::Ui, index: usize, entry: &Entry) {
    match entry {
        Entry::User(text) => {
            ui.add_space(6.0);
            // no explicit width: growing the frame beyond the available
            // width would feed the panel's auto-sizing loop
            egui::Frame::group(ui.style())
                .fill(ui.visuals().faint_bg_color)
                .stroke(egui::Stroke::new(1.0, theme::accent(ui).gamma_multiply(0.5)))
                .show(ui, |ui| {
                    ui.label(text);
                });
        }
        Entry::Assistant(text) => {
            ui.add_space(6.0);
            ui.label(text);
        }
        Entry::Tool { name, summary, ok, detail } => {
            let line = if summary.is_empty() {
                format!("⚙ {name}")
            } else {
                format!("⚙ {name} · {summary}")
            };
            let text = egui::RichText::new(line).small().monospace();
            match detail {
                // failed call: the chip expands to the full input + response
                Some(detail) => {
                    egui::CollapsingHeader::new(text.color(ui.visuals().warn_fg_color))
                        .id_salt(("ai-tool-detail", index))
                        .show(ui, |ui| detail_block(ui, detail));
                }
                None if *ok => {
                    ui.weak(text);
                }
                None => {
                    ui.label(text.color(ui.visuals().warn_fg_color));
                }
            }
        }
        Entry::Error(message) => {
            ui.add_space(4.0);
            let first_line = message.lines().next().unwrap_or_default();
            let long = message.lines().count() > 1 || first_line.chars().count() > 90;
            if long {
                // headline stays short; the full error expands on demand
                let headline: String = first_line.chars().take(90).collect();
                let title = egui::RichText::new(format!("{headline}…"))
                    .color(ui.visuals().warn_fg_color);
                egui::CollapsingHeader::new(title)
                    .id_salt(("ai-error-detail", index))
                    .show(ui, |ui| detail_block(ui, message));
            } else {
                ui.colored_label(ui.visuals().warn_fg_color, message);
            }
        }
        Entry::Cost { usd, usage, requests } => {
            let cost = match usd {
                Some(usd) => format_usd(*usd),
                None => "cost unknown (no price data)".to_string(),
            };
            ui.weak(
                egui::RichText::new(format!(
                    "{cost} · {} in / {} out tokens · {requests} request{}",
                    usage.input_tokens,
                    usage.output_tokens,
                    if *requests == 1 { "" } else { "s" }
                ))
                .small(),
            );
        }
    }
}

/// Full error/tool text: monospace, wrapped, in a quiet frame.
fn detail_block(ui: &mut egui::Ui, text: &str) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().extreme_bg_color)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(text).small().monospace())
                    .wrap(),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(input: Option<f64>, output: Option<f64>) -> ModelInfo {
        ModelInfo {
            id: "m".into(),
            name: "M".into(),
            input_per_mtok: input,
            output_per_mtok: output,
            context_length: None,
            tools: None,
        }
    }

    #[test]
    fn price_bands_match_the_ranges() {
        assert_eq!(price_band(&model(Some(0.0), Some(0.0))), 0, "free");
        assert_eq!(price_band(&model(Some(0.0), Some(0.5))), 1, "free input, paid output");
        assert_eq!(price_band(&model(Some(0.25), Some(2.0))), 1);
        assert_eq!(price_band(&model(Some(1.0), Some(8.0))), 1, "boundary goes low");
        assert_eq!(price_band(&model(Some(2.5), Some(10.0))), 2);
        assert_eq!(price_band(&model(Some(3.0), Some(15.0))), 2);
        assert_eq!(price_band(&model(Some(5.0), Some(25.0))), 3);
        assert_eq!(price_band(&model(Some(15.0), Some(75.0))), 4);
        assert_eq!(price_band(&model(None, None)), 5);
    }

    #[test]
    fn short_price_tags() {
        assert_eq!(short_price(&model(Some(0.0), Some(0.0))), "free");
        assert_eq!(short_price(&model(Some(3.0), Some(15.0))), "$3.00 / $15.00");
        assert_eq!(short_price(&model(None, None)), "$?");
    }
}
