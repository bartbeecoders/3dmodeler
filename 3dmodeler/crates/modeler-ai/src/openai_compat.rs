//! The OpenAI chat-completions dialect (`/chat/completions`), spoken — with
//! small accents — by OpenAI, OpenRouter, x.ai and most self-hosted stacks.
//! One implementation, parameterized by the model-catalog flavor: that is the
//! only part the vendors truly disagree on.

use crate::pricing;
use crate::types::*;
use crate::{Provider, ProviderConfig};
use serde_json::{json, Value};

/// How to fetch & read the model catalog.
#[derive(Clone, Copy, PartialEq)]
enum Catalog {
    /// `/models`: ids only — prices come from the static fallback table.
    OpenAi,
    /// `/models`: includes `pricing` in USD per single token (strings).
    OpenRouter,
    /// `/language-models`: includes token prices in 1/100 cent per MTok.
    XAi,
    /// `/models`, ids only, no fallback table (unknown vendor).
    Plain,
}

pub struct OpenAiCompat {
    catalog: Catalog,
    /// Key is optional (local endpoints); the big vendors require it.
    key_required: bool,
}

pub const OPENAI: OpenAiCompat = OpenAiCompat { catalog: Catalog::OpenAi, key_required: true };
pub const OPENROUTER: OpenAiCompat =
    OpenAiCompat { catalog: Catalog::OpenRouter, key_required: true };
pub const XAI: OpenAiCompat = OpenAiCompat { catalog: Catalog::XAi, key_required: true };
pub const CUSTOM: OpenAiCompat = OpenAiCompat { catalog: Catalog::Plain, key_required: false };

fn headers(cfg: &ProviderConfig) -> Vec<(String, String)> {
    let mut headers = vec![("content-type".into(), "application/json".into())];
    if !cfg.api_key.trim().is_empty() {
        headers.push(("authorization".into(), format!("Bearer {}", cfg.api_key.trim())));
    }
    headers
}

/// Surface `{"error":{"message":…}}` bodies as readable errors.
fn api_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    // some vendors return {"error": "..."} rather than an object
    if let Some(message) = error.as_str() {
        return Some(message.to_string());
    }
    let message = error["message"].as_str().unwrap_or("unknown error");
    Some(message.to_string())
}

impl OpenAiCompat {
    fn check_key(&self, cfg: &ProviderConfig) -> Result<(), String> {
        if self.key_required && cfg.api_key.trim().is_empty() {
            return Err(format!("set the {} API key first", cfg.kind.label()));
        }
        Ok(())
    }
}

impl Provider for OpenAiCompat {
    fn list_models_request(&self, cfg: &ProviderConfig) -> Result<HttpRequest, String> {
        self.check_key(cfg)?;
        let path = match self.catalog {
            Catalog::XAi => "language-models", // the priced catalog
            _ => "models",
        };
        Ok(HttpRequest {
            method: "GET",
            url: format!("{}/{path}", cfg.base_url()),
            headers: headers(cfg),
            body: None,
        })
    }

    fn parse_models(&self, body: &str) -> Result<Vec<ModelInfo>, String> {
        let value: Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
        if let Some(message) = api_error(&value) {
            return Err(message);
        }
        // xAI nests under "models", everyone else under "data"
        let data = value["data"]
            .as_array()
            .or_else(|| value["models"].as_array())
            .ok_or("no model list in response")?;
        let mut models: Vec<ModelInfo> = data
            .iter()
            .filter_map(|m| {
                let id = m["id"].as_str()?.to_string();
                if self.catalog == Catalog::OpenAi && !pricing::openai_is_chat_model(&id) {
                    return None; // hide whisper/tts/embeddings/dall-e etc.
                }
                let (input, output) = match self.catalog {
                    // strings, USD per single token
                    Catalog::OpenRouter => {
                        let per_tok = |key: &str| {
                            m["pricing"][key]
                                .as_str()
                                .and_then(|s| s.parse::<f64>().ok())
                                .map(|v| v * 1e6)
                        };
                        (per_tok("prompt"), per_tok("completion"))
                    }
                    // integers in 1/100 cent per MTok: 20000 = $2.00/MTok
                    Catalog::XAi => {
                        let per_mtok = |key: &str| m[key].as_f64().map(|v| v / 10_000.0);
                        (
                            per_mtok("prompt_text_token_price"),
                            per_mtok("completion_text_token_price"),
                        )
                    }
                    Catalog::OpenAi => pricing::openai(&id),
                    Catalog::Plain => (None, None),
                };
                Some(ModelInfo {
                    name: m["name"].as_str().unwrap_or(&id).to_string(),
                    id,
                    input_per_mtok: input,
                    output_per_mtok: output,
                    context_length: m["context_length"].as_u64(),
                })
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    fn chat_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> Result<HttpRequest, String> {
        self.check_key(cfg)?;
        let mut messages: Vec<Value> = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for message in &req.messages {
            messages_json(message, &mut messages);
        }
        let mut body = json!({
            "model": req.model,
            "messages": messages,
        });
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        },
                    })
                })
                .collect();
        }
        // deliberately no max_tokens: OpenAI renamed it (max_completion_tokens)
        // and reasoning models reject the old one; the defaults are fine
        Ok(HttpRequest {
            method: "POST",
            url: format!("{}/chat/completions", cfg.base_url()),
            headers: headers(cfg),
            body: Some(body.to_string()),
        })
    }

    fn parse_chat(&self, body: &str) -> Result<ChatResponse, String> {
        let value: Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
        if let Some(message) = api_error(&value) {
            return Err(message);
        }
        let choice = &value["choices"][0];
        if choice.is_null() {
            return Err("no choices in response".into());
        }
        let message = &choice["message"];
        let mut blocks = Vec::new();
        if let Some(text) = message["content"].as_str() {
            if !text.is_empty() {
                blocks.push(ContentBlock::Text(text.to_string()));
            }
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
                blocks.push(ContentBlock::ToolUse {
                    id: call["id"].as_str().unwrap_or_default().to_string(),
                    name: call["function"]["name"].as_str().unwrap_or_default().to_string(),
                    // arguments arrive as a JSON *string*
                    input: serde_json::from_str(arguments)
                        .unwrap_or(Value::Object(Default::default())),
                });
            }
        }
        let usage = Usage {
            input_tokens: value["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: value["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        };
        let stop_reason = match choice["finish_reason"].as_str() {
            Some("stop") | None => StopReason::EndTurn,
            Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
        };
        Ok(ChatResponse { blocks, usage, stop_reason })
    }
}

/// One neutral message → one or more wire messages. Assistant tool calls ride
/// on the assistant message; each tool result becomes a `role:"tool"` message.
/// Screenshot results additionally emit a user message with the image (the
/// tool role cannot carry images in this dialect).
fn messages_json(message: &ChatMessage, out: &mut Vec<Value>) {
    match message.role {
        Role::Assistant => {
            let mut text_parts: Vec<&str> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for block in &message.blocks {
                match block {
                    ContentBlock::Text(t) => text_parts.push(t),
                    ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                    ContentBlock::ToolResult { .. } => {} // never in assistant messages
                }
            }
            let mut m = json!({"role": "assistant"});
            m["content"] = if text_parts.is_empty() {
                Value::Null
            } else {
                json!(text_parts.join("\n"))
            };
            if !tool_calls.is_empty() {
                m["tool_calls"] = json!(tool_calls);
            }
            out.push(m);
        }
        Role::User => {
            let mut images: Vec<&str> = Vec::new();
            let mut texts: Vec<&str> = Vec::new();
            for block in &message.blocks {
                match block {
                    ContentBlock::Text(t) => texts.push(t),
                    ContentBlock::ToolResult { id, content, is_error, image_png_base64 } => {
                        let text = if *is_error {
                            format!("ERROR: {content}")
                        } else {
                            content.clone()
                        };
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": text,
                        }));
                        if let Some(png) = image_png_base64 {
                            images.push(png);
                        }
                    }
                    ContentBlock::ToolUse { .. } => {} // never in user messages
                }
            }
            if !texts.is_empty() {
                out.push(json!({"role": "user", "content": texts.join("\n")}));
            }
            for png in images {
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "image_url",
                        "image_url": {"url": format!("data:image/png;base64,{png}")},
                    }],
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderKind;

    fn cfg(kind: ProviderKind) -> ProviderConfig {
        ProviderConfig { kind, api_key: "sk-test".into(), base_url: String::new() }
    }

    #[test]
    fn chat_request_shape() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            system: "assist".into(),
            messages: vec![
                ChatMessage::user_text("add a cube"),
                ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "add_object".into(),
                        input: json!({"primitive": "cube"}),
                    }],
                },
                ChatMessage {
                    role: Role::User,
                    blocks: vec![ContentBlock::ToolResult {
                        id: "call_1".into(),
                        content: "{\"ok\":true}".into(),
                        is_error: false,
                        image_png_base64: None,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "add_object",
                description: "Add a primitive".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 4096,
        };
        let http = OPENAI.chat_request(&cfg(ProviderKind::OpenAi), &req).unwrap();
        assert_eq!(http.url, "https://api.openai.com/v1/chat/completions");
        let body: Value = serde_json::from_str(http.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
        // arguments must be a string, not an object
        assert!(body["messages"][2]["tool_calls"][0]["function"]["arguments"].is_string());
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["tools"][0]["function"]["name"], "add_object");
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn parse_chat_tool_calls() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "add_object",
                                     "arguments": "{\"primitive\": \"torus\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        }"#;
        let response = OPENAI.parse_chat(body).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        let (id, name, input) = response.tool_uses().next().unwrap();
        assert_eq!((id, name), ("call_9", "add_object"));
        assert_eq!(input["primitive"], "torus");
        assert_eq!(response.usage.output_tokens, 10);
    }

    #[test]
    fn openrouter_models_carry_prices() {
        let body = r#"{"data": [{
            "id": "anthropic/claude-sonnet-4.5",
            "name": "Anthropic: Claude Sonnet 4.5",
            "context_length": 200000,
            "pricing": {"prompt": "0.000003", "completion": "0.000015"}
        }]}"#;
        let models = OPENROUTER.parse_models(body).unwrap();
        assert_eq!(models[0].input_per_mtok, Some(3.0));
        assert_eq!(models[0].output_per_mtok, Some(15.0));
        assert_eq!(models[0].context_length, Some(200000));
    }

    #[test]
    fn xai_models_price_units() {
        // 20000 (1/100 cent per MTok) = $2.00 per MTok
        let body = r#"{"models": [{
            "id": "grok-3",
            "prompt_text_token_price": 20000,
            "completion_text_token_price": 100000
        }]}"#;
        let models = XAI.parse_models(body).unwrap();
        assert_eq!(models[0].input_per_mtok, Some(2.0));
        assert_eq!(models[0].output_per_mtok, Some(10.0));
    }

    #[test]
    fn openai_models_filtered_and_priced() {
        let body = r#"{"data": [
            {"id": "gpt-4o"},
            {"id": "whisper-1"},
            {"id": "text-embedding-3-small"}
        ]}"#;
        let models = OPENAI.parse_models(body).unwrap();
        assert_eq!(models.len(), 1, "non-chat models are hidden");
        assert_eq!(models[0].id, "gpt-4o");
        assert!(models[0].input_per_mtok.is_some());
    }

    #[test]
    fn parse_error_body() {
        let body = r#"{"error": {"message": "Incorrect API key provided"}}"#;
        assert!(OPENAI.parse_chat(body).unwrap_err().contains("Incorrect API key"));
    }
}
