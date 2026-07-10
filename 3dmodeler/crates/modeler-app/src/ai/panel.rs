//! The AI assistant chat panel (left dock): provider/model configuration,
//! the conversation log with per-interaction costs, and the message input.

use super::{cost_summary, ChatSession, Entry};
use crate::settings::Settings;
use crate::theme;
use modeler_ai::{format_usd, ProviderKind};
use three_d::egui;

pub struct ChatPanel {
    pub open: bool,
    show_config: bool,
    input: String,
    /// Re-focus the input after sending.
    focus_input: bool,
}

impl ChatPanel {
    pub fn new() -> Self {
        Self { open: false, show_config: false, input: String::new(), focus_input: false }
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
                    config_ui(ui, session, settings);
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
                                for entry in &session.entries {
                                    entry_ui(ui, entry);
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
}

fn entry_ui(ui: &mut egui::Ui, entry: &Entry) {
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
        Entry::Tool { name, summary, ok } => {
            let line = if summary.is_empty() {
                format!("⚙ {name}")
            } else {
                format!("⚙ {name} · {summary}")
            };
            let text = egui::RichText::new(line).small().monospace();
            if *ok {
                ui.weak(text);
            } else {
                ui.label(text.color(ui.visuals().warn_fg_color));
            }
        }
        Entry::Error(message) => {
            ui.add_space(4.0);
            ui.colored_label(ui.visuals().warn_fg_color, message);
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

// --- provider & model configuration ------------------------------------------

fn config_ui(ui: &mut egui::Ui, session: &mut ChatSession, settings: &mut Settings) {
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

            ui.label("Model");
            ui.vertical(|ui| {
                let state = &mut *state;
                if state.models.is_empty() {
                    ui.add(
                        egui::TextEdit::singleline(&mut state.model)
                            .hint_text("fetch models, or type a model id")
                            .desired_width(f32::INFINITY),
                    );
                } else {
                    let selected = state
                        .models
                        .iter()
                        .find(|m| m.id == state.model)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| state.model.clone());
                    egui::ComboBox::from_id_salt("ai-model")
                        .selected_text(selected)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for model in &state.models {
                                let label =
                                    format!("{} — {}", model.name, model.price_label());
                                ui.selectable_value(&mut state.model, model.id.clone(), label);
                            }
                        });
                }
            });
            ui.end_row();
        });

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let kind = settings.ai.active;
        if session.fetching_models() {
            ui.add(egui::Spinner::new().size(14.0));
            ui.weak("fetching models…");
        } else if ui
            .button("Fetch models")
            .on_hover_text("List this provider's models (uses the API key)")
            .clicked()
        {
            session.fetch_models(settings, kind);
        }
        // the chosen model's price tag
        if let Some(state) = settings.ai.state(kind) {
            if let Some(model) = state.models.iter().find(|m| m.id == state.model) {
                ui.weak(egui::RichText::new(model.price_label()).small());
            }
        }
    });
    if let Some(error) = &session.model_fetch_error {
        ui.colored_label(ui.visuals().warn_fg_color, error);
    }
    match settings.ai.active {
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
