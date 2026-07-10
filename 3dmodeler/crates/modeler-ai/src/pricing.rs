//! Static price fallback for providers whose catalog APIs do not publish
//! prices (Anthropic, OpenAI). USD per million tokens, matched by id prefix
//! — LONGEST prefix first, so "gpt-4o-mini" wins over "gpt-4o".
//!
//! Prices drift; treat these as approximate (the UI labels them so). Vendors
//! that do publish prices (OpenRouter, x.ai) never consult this table.

/// (input $/MTok, output $/MTok) for an Anthropic model id.
pub fn anthropic(id: &str) -> (Option<f64>, Option<f64>) {
    const TABLE: &[(&str, f64, f64)] = &[
        ("claude-fable-5", 25.0, 125.0),
        ("claude-opus-4", 15.0, 75.0),
        ("claude-sonnet-5", 3.0, 15.0),
        ("claude-sonnet-4", 3.0, 15.0),
        ("claude-haiku-4-5", 1.0, 5.0),
        ("claude-3-7-sonnet", 3.0, 15.0),
        ("claude-3-5-sonnet", 3.0, 15.0),
        ("claude-3-5-haiku", 0.8, 4.0),
        ("claude-3-opus", 15.0, 75.0),
        ("claude-3-haiku", 0.25, 1.25),
    ];
    lookup(TABLE, id)
}

/// (input $/MTok, output $/MTok) for an OpenAI model id.
pub fn openai(id: &str) -> (Option<f64>, Option<f64>) {
    const TABLE: &[(&str, f64, f64)] = &[
        ("gpt-5-nano", 0.05, 0.4),
        ("gpt-5-mini", 0.25, 2.0),
        ("gpt-5", 1.25, 10.0),
        ("gpt-4.1-nano", 0.1, 0.4),
        ("gpt-4.1-mini", 0.4, 1.6),
        ("gpt-4.1", 2.0, 8.0),
        ("gpt-4o-mini", 0.15, 0.6),
        ("gpt-4o", 2.5, 10.0),
        ("gpt-4-turbo", 10.0, 30.0),
        ("gpt-4", 30.0, 60.0),
        ("gpt-3.5-turbo", 0.5, 1.5),
        ("o4-mini", 1.1, 4.4),
        ("o3-mini", 1.1, 4.4),
        ("o3", 2.0, 8.0),
        ("o1", 15.0, 60.0),
    ];
    lookup(TABLE, id)
}

fn lookup(table: &[(&str, f64, f64)], id: &str) -> (Option<f64>, Option<f64>) {
    for (prefix, input, output) in table {
        if id.starts_with(prefix) {
            return (Some(*input), Some(*output));
        }
    }
    (None, None)
}

/// OpenAI's `/models` mixes chat models with audio/image/embedding endpoints;
/// keep only what a chat window can talk to.
pub fn openai_is_chat_model(id: &str) -> bool {
    let chat = ["gpt-3.5-turbo", "gpt-4", "gpt-5", "o1", "o3", "o4", "chatgpt-"];
    let not_chat = [
        "-audio", "-realtime", "-transcribe", "-tts", "-search", "-instruct", "-image",
    ];
    chat.iter().any(|p| id.starts_with(p)) && !not_chat.iter().any(|s| id.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        assert_eq!(openai("gpt-4o-mini-2024-07-18").0, Some(0.15));
        assert_eq!(openai("gpt-4o-2024-08-06").0, Some(2.5));
        assert_eq!(anthropic("claude-haiku-4-5-20251001").0, Some(1.0));
        assert_eq!(anthropic("claude-sonnet-4-5-20250929").1, Some(15.0));
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert_eq!(openai("some-future-model"), (None, None));
        assert_eq!(anthropic("claude-x"), (None, None));
    }

    #[test]
    fn chat_model_filter() {
        assert!(openai_is_chat_model("gpt-4o"));
        assert!(openai_is_chat_model("o3-mini"));
        assert!(!openai_is_chat_model("whisper-1"));
        assert!(!openai_is_chat_model("text-embedding-3-small"));
        assert!(!openai_is_chat_model("gpt-4o-audio-preview"));
        assert!(!openai_is_chat_model("dall-e-3"));
    }
}
