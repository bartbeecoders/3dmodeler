//! The AI assistant: a chat session that lets a language model drive the
//! modeler through the same command set as the MCP server.
//!
//! One user message starts an agentic loop: the model replies with tool
//! calls, the render loop executes them against the live scene, the results
//! go back to the model — repeated until it answers in plain text. All
//! network I/O is fire-and-poll ([`crate::net`]); every frame calls
//! [`ChatSession::poll`], so the viewport keeps rendering (and the user
//! watches the scene grow) while the model thinks.

mod panel;
mod tools;

pub use panel::ChatPanel;

use crate::net::{self, HttpTask};
use crate::physics::PhysicsMirror;
use crate::scene_render::{LightingMode, ShadeMode};
use crate::selection::Selection;
use crate::settings::Settings;
use modeler_ai::{
    format_usd, provider_for, ChatMessage, ChatRequest, ContentBlock, ModelInfo, ProviderKind,
    Role, StopReason, Usage,
};
use modeler_core::{Library, Scene};

/// Everything the tools may touch, borrowed from the render loop each frame.
pub struct ToolContext<'a> {
    pub scene: &'a mut Scene,
    pub selection: &'a mut Selection,
    pub physics: &'a mut PhysicsMirror,
    pub library: &'a mut Library,
    pub shade_mode: &'a mut ShadeMode,
    pub lighting_mode: &'a mut LightingMode,
}

/// One line of the chat log. The provider conversation (`messages`) is the
/// model's view; entries are the human's.
pub enum Entry {
    User(String),
    Assistant(String),
    /// A tool call: name, compact argument summary, ok flag. `detail` holds
    /// the full response for failed calls (expandable in the log).
    Tool { name: String, summary: String, ok: bool, detail: Option<String> },
    Error(String),
    /// Per-interaction footer: cost (None = no price data), tokens, requests.
    Cost { usd: Option<f64>, usage: Usage, requests: u32 },
}

enum Phase {
    Idle,
    /// A chat request is in flight.
    Waiting(HttpTask),
    /// The model called `screenshot`: results wait for this frame's pixels.
    /// Holds the pending tool-result blocks (screenshot ones incomplete).
    AwaitScreenshot(Vec<ContentBlock>),
}

/// Tool rounds allowed per user message — enough for a small city, small
/// enough to stop a confused model from spinning up an infinite bill.
const MAX_TOOL_ROUNDS: u32 = 48;

/// Response budget per request.
const MAX_TOKENS: u32 = 8192;

/// Screenshots are downscaled to this edge before going to the model:
/// vision tokens are priced per pixel, and composition — not pixels — is
/// what the model needs to see.
const SCREENSHOT_MAX_EDGE: u32 = 768;

pub struct ChatSession {
    messages: Vec<ChatMessage>,
    pub entries: Vec<Entry>,
    phase: Phase,
    /// Which provider/model this conversation runs on, frozen at the first
    /// send so a mid-chat provider switch starts cleanly (tool-call ids are
    /// not portable between dialects).
    active: Option<(ProviderKind, String)>,
    // per-interaction accounting (reset on each user send)
    turn_usage: Usage,
    turn_cost: Option<f64>,
    turn_priced: bool,
    turn_requests: u32,
    tool_rounds: u32,
    // session totals
    pub total_usage: Usage,
    pub total_cost: f64,
    /// False once any request ran on a model with unknown prices.
    pub total_priced: bool,
    // model-catalog fetch (config UI)
    model_fetch: Option<(ProviderKind, HttpTask)>,
    pub model_fetch_error: Option<String>,
}

impl ChatSession {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            entries: Vec::new(),
            phase: Phase::Idle,
            active: None,
            turn_usage: Usage::default(),
            turn_cost: Some(0.0),
            turn_priced: true,
            turn_requests: 0,
            tool_rounds: 0,
            total_usage: Usage::default(),
            total_cost: 0.0,
            total_priced: true,
            model_fetch: None,
            model_fetch_error: None,
        }
    }

    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// What the status line should say while the model works.
    pub fn status_line(&self) -> Option<String> {
        match &self.phase {
            Phase::Idle => None,
            Phase::AwaitScreenshot(_) => Some("capturing the viewport…".into()),
            Phase::Waiting(_) => Some(if self.tool_rounds == 0 {
                "thinking…".to_string()
            } else {
                format!("working… (step {})", self.tool_rounds + 1)
            }),
        }
    }

    pub fn clear(&mut self) {
        if self.busy() {
            return; // the ⏹ button cancels first
        }
        self.messages.clear();
        self.entries.clear();
        self.active = None;
    }

    /// Send a user message and start the tool loop.
    pub fn send(&mut self, text: &str, settings: &Settings) {
        let text = text.trim();
        if text.is_empty() || self.busy() {
            return;
        }
        // freeze the provider/model for this conversation
        let kind = settings.ai.active;
        let model = settings.ai.state(kind).map(|s| s.model.clone()).unwrap_or_default();
        if model.trim().is_empty() {
            self.entries.push(Entry::Error(
                "pick a provider and model first (⚙ in the panel header)".into(),
            ));
            return;
        }
        if let Some((active_kind, active_model)) = &self.active {
            if *active_kind != kind || *active_model != model {
                // switching models mid-chat: keep the log, restart the
                // conversation (tool ids don't transfer between dialects)
                self.messages.clear();
                self.entries.push(Entry::Error(format!(
                    "switched to {} · {} — the model starts fresh from here",
                    kind.label(),
                    model
                )));
            }
        }
        self.active = Some((kind, model));
        self.entries.push(Entry::User(text.to_string()));
        self.messages.push(ChatMessage::user_text(text));
        self.turn_usage = Usage::default();
        self.turn_cost = Some(0.0);
        self.turn_priced = true;
        self.turn_requests = 0;
        self.tool_rounds = 0;
        self.request(settings);
    }

    /// Abort the in-flight interaction. The conversation stays consistent:
    /// unanswered tool calls get error results so the next send is valid.
    pub fn cancel(&mut self) {
        if !self.busy() {
            return;
        }
        self.phase = Phase::Idle;
        self.close_dangling_tool_calls("cancelled by the user");
        self.entries.push(Entry::Error("stopped".into()));
        self.finish_turn();
    }

    fn close_dangling_tool_calls(&mut self, note: &str) {
        let Some(last) = self.messages.last() else { return };
        if last.role != Role::Assistant {
            return;
        }
        let results: Vec<ContentBlock> = last
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(ContentBlock::ToolResult {
                    id: id.clone(),
                    content: note.to_string(),
                    is_error: true,
                    image_png_base64: None,
                }),
                _ => None,
            })
            .collect();
        if !results.is_empty() {
            self.messages.push(ChatMessage { role: Role::User, blocks: results });
        }
    }

    /// The selected model's catalog entry (for pricing).
    fn model_info(&self, settings: &Settings) -> Option<ModelInfo> {
        let (kind, model) = self.active.as_ref()?;
        settings
            .ai
            .state(*kind)
            .and_then(|s| s.models.iter().find(|m| &m.id == model))
            .cloned()
    }

    fn request(&mut self, settings: &Settings) {
        let Some((kind, model)) = self.active.clone() else { return };
        let Some(state) = settings.ai.state(kind) else {
            self.entries.push(Entry::Error("provider not configured".into()));
            return;
        };
        let request = ChatRequest {
            model,
            system: tools::system_prompt(),
            messages: self.messages.clone(),
            tools: tools::catalog(),
            max_tokens: MAX_TOKENS,
        };
        match provider_for(kind).chat_request(&state.config, &request) {
            Ok(http) => {
                self.turn_requests += 1;
                self.phase = Phase::Waiting(net::fetch(http));
            }
            Err(e) => {
                self.entries.push(Entry::Error(e));
                self.finish_turn();
            }
        }
    }

    fn finish_turn(&mut self) {
        self.phase = Phase::Idle;
        if self.turn_requests > 0 {
            self.entries.push(Entry::Cost {
                usd: if self.turn_priced { self.turn_cost } else { None },
                usage: self.turn_usage,
                requests: self.turn_requests,
            });
        }
    }

    /// Drive the session. Call once per frame from the render loop.
    pub fn poll(&mut self, settings: &mut Settings, mut ctx: ToolContext) {
        self.poll_model_fetch(settings);
        let outcome = match &mut self.phase {
            Phase::Waiting(task) => match task.poll() {
                Some(outcome) => outcome,
                None => return,
            },
            _ => return, // Idle, or AwaitScreenshot (render loop delivers)
        };
        let body = match outcome {
            Ok(body) => body,
            Err(e) => {
                self.entries.push(Entry::Error(e));
                self.finish_turn();
                return;
            }
        };
        let Some((kind, _)) = self.active else {
            self.finish_turn();
            return;
        };
        let response = match provider_for(kind).parse_chat(&body) {
            Ok(response) => response,
            Err(e) => {
                self.close_dangling_tool_calls("request failed");
                self.entries.push(Entry::Error(e));
                self.finish_turn();
                return;
            }
        };

        // account the leg's usage/cost
        self.turn_usage.add(response.usage);
        self.total_usage.add(response.usage);
        match self.model_info(settings).and_then(|m| m.cost(&response.usage)) {
            Some(cost) => {
                self.turn_cost = self.turn_cost.map(|c| c + cost);
                self.total_cost += cost;
            }
            None => {
                self.turn_priced = false;
                self.total_priced = false;
            }
        }

        if response.blocks.is_empty() {
            self.entries.push(Entry::Error("the model returned an empty response".into()));
            self.finish_turn();
            return;
        }
        let text = response.text();
        if !text.trim().is_empty() {
            self.entries.push(Entry::Assistant(text.trim().to_string()));
        }
        self.messages.push(ChatMessage {
            role: Role::Assistant,
            blocks: response.blocks.clone(),
        });

        let tool_uses: Vec<(String, String, serde_json::Value)> = response
            .tool_uses()
            .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
            .collect();
        if tool_uses.is_empty() {
            if response.stop_reason == StopReason::MaxTokens {
                self.entries.push(Entry::Error(
                    "response hit the length limit — ask to continue".into(),
                ));
            }
            self.finish_turn();
            return;
        }

        // tool round: budget check, then execute everything the model asked
        self.tool_rounds += 1;
        if self.tool_rounds > MAX_TOOL_ROUNDS {
            let results: Vec<ContentBlock> = tool_uses
                .iter()
                .map(|(id, _, _)| ContentBlock::ToolResult {
                    id: id.clone(),
                    content: "tool budget exhausted for this message — stopping here".into(),
                    is_error: true,
                    image_png_base64: None,
                })
                .collect();
            self.messages.push(ChatMessage { role: Role::User, blocks: results });
            self.entries.push(Entry::Error(format!(
                "stopped after {MAX_TOOL_ROUNDS} tool rounds — send a follow-up to continue"
            )));
            self.finish_turn();
            return;
        }

        let mut results: Vec<ContentBlock> = Vec::new();
        let mut needs_screenshot = false;
        for (id, name, input) in &tool_uses {
            if name == "screenshot" {
                // resolved after this frame renders (deliver_screenshot)
                needs_screenshot = true;
                results.push(ContentBlock::ToolResult {
                    id: id.clone(),
                    content: String::new(),
                    is_error: false,
                    image_png_base64: None,
                });
                self.entries.push(Entry::Tool {
                    name: name.clone(),
                    summary: String::new(),
                    ok: true,
                    detail: None,
                });
                continue;
            }
            let response = tools::dispatch(name, input, &mut ctx);
            let ok = response["ok"].as_bool().unwrap_or(false);
            // failed calls keep the full exchange for the expandable log entry
            let detail = (!ok).then(|| {
                format!(
                    "input:\n{}\n\nresponse:\n{}",
                    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string()),
                    serde_json::to_string_pretty(&response)
                        .unwrap_or_else(|_| response.to_string()),
                )
            });
            self.entries.push(Entry::Tool {
                name: name.clone(),
                summary: tools::summarize(name, input, &response),
                ok,
                detail,
            });
            results.push(ContentBlock::ToolResult {
                id: id.clone(),
                content: response.to_string(),
                is_error: !ok,
                image_png_base64: None,
            });
        }
        if needs_screenshot {
            self.phase = Phase::AwaitScreenshot(results);
        } else {
            self.messages.push(ChatMessage { role: Role::User, blocks: results });
            self.request(settings);
        }
    }

    /// Does the render loop need to capture this frame's pixels?
    pub fn wants_screenshot(&self) -> bool {
        matches!(self.phase, Phase::AwaitScreenshot(_))
    }

    /// Complete a screenshot round with the freshly rendered frame.
    pub fn deliver_screenshot(
        &mut self,
        pixels: &[[u8; 4]],
        width: u32,
        height: u32,
        settings: &Settings,
    ) {
        let Phase::AwaitScreenshot(mut results) = std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            return;
        };
        let encoded = encode_downscaled(pixels, width, height, SCREENSHOT_MAX_EDGE);
        for block in &mut results {
            if let ContentBlock::ToolResult { content, is_error, image_png_base64, .. } = block {
                if !content.is_empty() {
                    continue; // a regular tool result from the same round
                }
                match &encoded {
                    Ok(png) => {
                        *content = "current viewport:".into();
                        *image_png_base64 = Some(png.clone());
                    }
                    Err(e) => {
                        *content = format!("screenshot failed: {e}");
                        *is_error = true;
                    }
                }
            }
        }
        self.messages.push(ChatMessage { role: Role::User, blocks: results });
        self.request(settings);
    }

    // --- model catalog fetching (config UI) --------------------------------

    /// Kick off a model-list fetch for the provider (config UI button).
    pub fn fetch_models(&mut self, settings: &Settings, kind: ProviderKind) {
        let Some(state) = settings.ai.state(kind) else { return };
        self.model_fetch_error = None;
        match provider_for(kind).list_models_request(&state.config) {
            Ok(http) => self.model_fetch = Some((kind, net::fetch(http))),
            Err(e) => self.model_fetch_error = Some(e),
        }
    }

    pub fn fetching_models(&self) -> bool {
        self.model_fetch.is_some()
    }

    fn poll_model_fetch(&mut self, settings: &mut Settings) {
        let Some((kind, task)) = &mut self.model_fetch else { return };
        let kind = *kind;
        let Some(outcome) = task.poll() else { return };
        self.model_fetch = None;
        let result = outcome.and_then(|body| provider_for(kind).parse_models(&body));
        match result {
            Ok(models) => {
                let state = settings.ai.state_mut(kind);
                // keep the selection when the refreshed list still has it
                if !models.iter().any(|m| m.id == state.model) {
                    state.model = models.first().map(|m| m.id.clone()).unwrap_or_default();
                }
                state.models = models;
            }
            Err(e) => self.model_fetch_error = Some(e),
        }
    }
}

/// Downscale + PNG-encode a frame for the model. `pixels` are three-d's
/// `read_color` output (top-down RGBA rows).
fn encode_downscaled(
    pixels: &[[u8; 4]],
    width: u32,
    height: u32,
    max_edge: u32,
) -> Result<String, String> {
    if width == 0 || height == 0 || pixels.len() != (width * height) as usize {
        return Err("empty frame".into());
    }
    let mut rgba: Vec<u8> = Vec::with_capacity(pixels.len() * 4);
    for pixel in pixels {
        rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
    let image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or("pixel buffer does not match the frame size")?;
    let scale = (max_edge as f32 / width.max(height) as f32).min(1.0);
    let (w, h) = (
        ((width as f32 * scale) as u32).max(1),
        ((height as f32 * scale) as u32).max(1),
    );
    let resized = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
    let mut png_bytes: Vec<u8> = Vec::new();
    resized
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&png_bytes))
}

/// Session cost summary for the panel footer.
pub fn cost_summary(session: &ChatSession) -> String {
    let cost = if session.total_priced {
        format_usd(session.total_cost)
    } else if session.total_cost > 0.0 {
        format!("≥ {}", format_usd(session.total_cost))
    } else {
        "unknown".to_string()
    };
    format!(
        "session: {cost} · {} in / {} out tokens",
        session.total_usage.input_tokens, session.total_usage.output_tokens
    )
}
