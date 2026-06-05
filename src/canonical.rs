//! Provider-neutral request + streaming-event model. Every wire format
//! translates to/from these types, so we never translate format→format directly.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User(String),
    Assistant { text: Option<String>, reasoning_content: Option<String>, tool_calls: Vec<ToolCall> },
    Tool { call_id: String, output: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    /// Raw JSON arguments string (as the model emits them).
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Function(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            _ => return None,
        })
    }
}

/// Provider-neutral streaming events. Both wire formats serialize to/from this.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalEvent {
    Created { response_id: String, model: String },
    ReasoningDelta { text: String },
    TextDelta { text: String },
    ToolCallStart { index: u32, call_id: String, name: String },
    ToolCallArgsDelta { index: u32, delta: String },
    ToolCallDone { index: u32 },
    Usage { input_tokens: u32, output_tokens: u32, total_tokens: u32 },
    Completed,
    Error { message: String, status: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_roundtrip() {
        assert_eq!(ReasoningEffort::from_str_opt("xhigh"), Some(ReasoningEffort::XHigh));
        assert_eq!(ReasoningEffort::XHigh.as_str(), "xhigh");
        assert_eq!(ReasoningEffort::from_str_opt("bogus"), None);
    }

    #[test]
    fn tool_choice_default_is_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }
}
