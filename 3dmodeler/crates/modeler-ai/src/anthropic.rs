//! Anthropic Messages API (`/v1/messages`, `/v1/models`).
//!
//! The catalog endpoint does not publish prices, so the static table in
//! [`crate::pricing`] fills them in.

use crate::pricing;
use crate::types::*;
use crate::{Provider, ProviderConfig};
use serde_json::{json, Value};

pub struct Anthropic;

const VERSION: &str = "2023-06-01";

fn headers(cfg: &ProviderConfig) -> Vec<(String, String)> {
    vec![
        ("x-api-key".into(), cfg.api_key.trim().to_string()),
        ("anthropic-version".into(), VERSION.into()),
        ("content-type".into(), "application/json".into()),
        // lets the wasm build call the API from the browser (CORS opt-in)
        ("anthropic-dangerous-direct-browser-access".into(), "true".into()),
    ]
}

/// Surface `{"type":"error","error":{"message":…}}` bodies as readable errors.
fn api_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    let message = error["message"].as_str().unwrap_or("unknown error");
    let kind = error["type"].as_str().unwrap_or("error");
    Some(format!("{kind}: {message}"))
}

impl Provider for Anthropic {
    fn list_models_request(&self, cfg: &ProviderConfig) -> Result<HttpRequest, String> {
        if cfg.api_key.trim().is_empty() {
            return Err("set the Anthropic API key first".into());
        }
        Ok(HttpRequest {
            method: "GET",
            url: format!("{}/v1/models?limit=100", cfg.base_url()),
            headers: headers(cfg),
            body: None,
        })
    }

    fn parse_models(&self, body: &str) -> Result<Vec<ModelInfo>, String> {
        let value: Value = serde_json::from_str(body)
            .map_err(|e| format!("bad JSON: {e}\n{}", crate::excerpt(body)))?;
        if let Some(message) = api_error(&value) {
            return Err(message);
        }
        let data = value["data"].as_array().ok_or("no 'data' array in response")?;
        Ok(data
            .iter()
            .filter_map(|m| {
                let id = m["id"].as_str()?.to_string();
                let (input, output) = pricing::anthropic(&id);
                Some(ModelInfo {
                    name: m["display_name"].as_str().unwrap_or(&id).to_string(),
                    id,
                    input_per_mtok: input,
                    output_per_mtok: output,
                    context_length: None,
                    tools: Some(true), // every current Claude model has tool use
                })
            })
            .collect())
    }

    fn chat_request(&self, cfg: &ProviderConfig, req: &ChatRequest) -> Result<HttpRequest, String> {
        if cfg.api_key.trim().is_empty() {
            return Err("set the Anthropic API key first".into());
        }
        let messages: Vec<Value> = req.messages.iter().map(message_json).collect();
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
        }
        Ok(HttpRequest {
            method: "POST",
            url: format!("{}/v1/messages", cfg.base_url()),
            headers: headers(cfg),
            body: Some(body.to_string()),
        })
    }

    fn parse_chat(&self, body: &str) -> Result<ChatResponse, String> {
        let value: Value = serde_json::from_str(body)
            .map_err(|e| format!("bad JSON: {e}\n{}", crate::excerpt(body)))?;
        if let Some(message) = api_error(&value) {
            return Err(message);
        }
        let content = value["content"].as_array().ok_or("no 'content' in response")?;
        let blocks = content
            .iter()
            .filter_map(|block| match block["type"].as_str()? {
                "text" => Some(ContentBlock::Text(block["text"].as_str()?.to_string())),
                "tool_use" => Some(ContentBlock::ToolUse {
                    id: block["id"].as_str()?.to_string(),
                    name: block["name"].as_str()?.to_string(),
                    input: block["input"].clone(),
                }),
                _ => None, // thinking blocks etc. are not replayed
            })
            .collect();
        let usage = Usage {
            input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0),
        };
        let stop_reason = match value["stop_reason"].as_str() {
            Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::EndTurn,
        };
        Ok(ChatResponse { blocks, usage, stop_reason })
    }
}

fn message_json(message: &ChatMessage) -> Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = message
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => json!({"type": "text", "text": text}),
            ContentBlock::ToolUse { id, name, input } => {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            ContentBlock::ToolResult { id, content, is_error, image_png_base64 } => {
                let mut inner = vec![json!({"type": "text", "text": content})];
                if let Some(png) = image_png_base64 {
                    inner.push(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": png},
                    }));
                }
                json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": inner,
                    "is_error": is_error,
                })
            }
        })
        .collect();
    json!({"role": role, "content": content})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderKind;

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            kind: ProviderKind::Anthropic,
            api_key: "sk-ant-test".into(),
            base_url: String::new(),
        }
    }

    #[test]
    fn chat_request_shape() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5".into(),
            system: "you are helpful".into(),
            messages: vec![
                ChatMessage::user_text("add a cube"),
                ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "add_object".into(),
                        input: serde_json::json!({"primitive": "cube"}),
                    }],
                },
                ChatMessage {
                    role: Role::User,
                    blocks: vec![ContentBlock::ToolResult {
                        id: "tu_1".into(),
                        content: "{\"ok\":true}".into(),
                        is_error: false,
                        image_png_base64: None,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "add_object",
                description: "Add a primitive".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 4096,
        };
        let http = Anthropic.chat_request(&cfg(), &req).unwrap();
        assert_eq!(http.url, "https://api.anthropic.com/v1/messages");
        assert!(http.headers.iter().any(|(k, _)| k == "x-api-key"));
        let body: Value = serde_json::from_str(http.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["system"], "you are helpful");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["name"], "add_object");
    }

    #[test]
    fn parse_chat_tool_use() {
        let body = r#"{
            "content": [
                {"type": "text", "text": "Adding a cube."},
                {"type": "tool_use", "id": "tu_1", "name": "add_object",
                 "input": {"primitive": "cube"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }"#;
        let response = Anthropic.parse_chat(body).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.input_tokens, 100);
        assert_eq!(response.text(), "Adding a cube.");
        let (id, name, input) = response.tool_uses().next().unwrap();
        assert_eq!((id, name), ("tu_1", "add_object"));
        assert_eq!(input["primitive"], "cube");
    }

    #[test]
    fn parse_models_with_static_pricing() {
        let body = r#"{"data": [
            {"id": "claude-sonnet-4-5-20250929", "display_name": "Claude Sonnet 4.5"},
            {"id": "claude-mystery-9", "display_name": "Mystery"}
        ]}"#;
        let models = Anthropic.parse_models(body).unwrap();
        assert_eq!(models[0].name, "Claude Sonnet 4.5");
        assert!(models[0].input_per_mtok.is_some());
        assert!(models[1].input_per_mtok.is_none(), "unknown model has no price");
    }

    #[test]
    fn parse_error_body() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#;
        assert!(Anthropic.parse_chat(body).unwrap_err().contains("bad key"));
    }
}
