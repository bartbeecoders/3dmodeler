//! Provider-neutral chat types. Providers translate these to/from their own
//! wire formats, so the app and the chat loop never see vendor JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A network request the app's transport executes verbatim.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message. Tool results travel in User messages (that is how
/// both API dialects model them).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    /// The model asked to run a tool.
    ToolUse { id: String, name: String, input: Value },
    /// The app's answer to a tool call. `image_png_base64` carries viewport
    /// screenshots to vision models.
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
        image_png_base64: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: Role::User, blocks: vec![ContentBlock::Text(text.into())] }
    }
}

/// A tool the model may call. `input_schema` is standard JSON Schema.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    pub input_schema: Value,
}

/// One (non-streaming) model turn.
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    /// Response cap; providers that dislike the parameter may ignore it.
    pub max_tokens: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct ChatResponse {
    /// Text and ToolUse blocks, in model order.
    pub blocks: Vec<ContentBlock>,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

impl ChatResponse {
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.blocks.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.as_str(), name.as_str(), input))
            }
            _ => None,
        })
    }

    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }
}

/// A model as advertised by a provider's catalog. Prices are USD per million
/// tokens; None = the provider does not publish them and no fallback matched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    pub context_length: Option<u64>,
    /// Can this model call tools (drive the modeler)? None = the provider
    /// does not say (local/custom endpoints — depends on the loaded model).
    #[serde(default)]
    pub tools: Option<bool>,
}

impl ModelInfo {
    /// Cost of `usage` in USD, when the model's prices are known.
    pub fn cost(&self, usage: &Usage) -> Option<f64> {
        let input = self.input_per_mtok? * usage.input_tokens as f64 / 1e6;
        let output = self.output_per_mtok? * usage.output_tokens as f64 / 1e6;
        Some(input + output)
    }

    /// Short price tag for pickers, e.g. `"$3.00 / $15.00 per MTok"`.
    pub fn price_label(&self) -> String {
        match (self.input_per_mtok, self.output_per_mtok) {
            (Some(i), Some(o)) => format!("${i:.2} in / ${o:.2} out per MTok"),
            _ => "price unknown".to_string(),
        }
    }
}

/// `"$0.0042"` style money formatting for the chat log (sub-cent amounts keep
/// enough digits to not read as zero).
pub fn format_usd(amount: f64) -> String {
    if amount == 0.0 {
        "$0.00".to_string()
    } else if amount < 0.01 {
        format!("${amount:.4}")
    } else {
        format!("${amount:.2}")
    }
}
