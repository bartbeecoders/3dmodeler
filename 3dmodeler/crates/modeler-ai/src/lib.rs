//! Transport-agnostic AI provider layer ("sans-io").
//!
//! A [`Provider`] never talks to the network: it only *builds* [`HttpRequest`]s
//! and *parses* response bodies. The app owns the transport (a background
//! thread natively, `fetch` in the browser), which keeps this crate free of
//! async runtimes, portable to wasm, and unit-testable with plain strings.
//!
//! Adding a provider = implementing request building + parsing for its API
//! shape. Most vendors (OpenRouter, x.ai, Groq, Mistral, Ollama, …) speak the
//! OpenAI chat-completions dialect, so they reuse [`openai_compat`] with a
//! different base URL and model-catalog parser.

mod anthropic;
mod openai_compat;
pub mod pricing;
mod types;

pub use types::*;

/// The built-in provider set. `LmStudio` is the local LM Studio server;
/// `Custom` is any other OpenAI-compatible endpoint the user points at
/// (Ollama, vLLM, a proxy, …).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    OpenRouter,
    XAi,
    LmStudio,
    Custom,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 6] = [
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::OpenRouter,
        ProviderKind::XAi,
        ProviderKind::LmStudio,
        ProviderKind::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::OpenAi => "OpenAI",
            ProviderKind::OpenRouter => "OpenRouter",
            ProviderKind::XAi => "xAI",
            ProviderKind::LmStudio => "LM Studio (local)",
            ProviderKind::Custom => "Custom (OpenAI-compatible)",
        }
    }

    /// Base URL up to (not including) the endpoint path.
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::OpenAi => "https://api.openai.com/v1",
            ProviderKind::OpenRouter => "https://openrouter.ai/api/v1",
            ProviderKind::XAi => "https://api.x.ai/v1",
            ProviderKind::LmStudio => "http://localhost:1234/v1",
            ProviderKind::Custom => "http://localhost:11434/v1",
        }
    }

    /// Placeholder shown in the API-key field.
    pub fn key_hint(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "sk-ant-…",
            ProviderKind::OpenAi => "sk-…",
            ProviderKind::OpenRouter => "sk-or-…",
            ProviderKind::XAi => "xai-…",
            ProviderKind::LmStudio => "(not needed)",
            ProviderKind::Custom => "(optional)",
        }
    }
}

/// Per-provider user configuration (persisted by the app).
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    #[serde(default)]
    pub api_key: String,
    /// Empty = use `kind.default_base_url()`.
    #[serde(default)]
    pub base_url: String,
}

impl ProviderConfig {
    pub fn new(kind: ProviderKind) -> Self {
        Self { kind, api_key: String::new(), base_url: String::new() }
    }

    pub fn base_url(&self) -> &str {
        let trimmed = self.base_url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            self.kind.default_base_url()
        } else {
            trimmed
        }
    }
}

/// Request builder + response parser for one API dialect.
pub trait Provider {
    /// GET the model catalog.
    fn list_models_request(&self, cfg: &ProviderConfig) -> Result<HttpRequest, String>;
    fn parse_models(&self, body: &str) -> Result<Vec<ModelInfo>, String>;
    /// POST one (non-streaming) chat turn, tools included.
    fn chat_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> Result<HttpRequest, String>;
    fn parse_chat(&self, body: &str) -> Result<ChatResponse, String>;
}

/// The provider implementation for a configured kind.
pub fn provider_for(kind: ProviderKind) -> &'static dyn Provider {
    match kind {
        ProviderKind::Anthropic => &anthropic::Anthropic,
        ProviderKind::OpenAi => &openai_compat::OPENAI,
        ProviderKind::OpenRouter => &openai_compat::OPENROUTER,
        ProviderKind::XAi => &openai_compat::XAI,
        ProviderKind::LmStudio => &openai_compat::LMSTUDIO,
        ProviderKind::Custom => &openai_compat::CUSTOM,
    }
}

/// A short slice of a response body for error messages — enough to see what
/// the server actually sent (HTML error page, proxy message, …).
pub(crate) fn excerpt(body: &str) -> String {
    const LIMIT: usize = 280;
    let trimmed = body.trim();
    let cut: String = trimmed.chars().take(LIMIT).collect();
    if trimmed.chars().count() > LIMIT {
        format!("{cut}…")
    } else if cut.is_empty() {
        "(empty response)".to_string()
    } else {
        cut
    }
}
