//! OpenAI Chat Completions wire format — outbound (client) side.

use serde_json::{json, Map, Value};

use crate::canonical::*;
use crate::config::Provider;

pub fn build_request(req: &CanonicalRequest, upstream_model: &str, provider: &Provider) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = &req.system {
        messages.push(json!({"role": "system", "content": sys}));
    }
    for m in &req.messages {
        match m {
            Message::User(text) => messages.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert("content".into(), text.clone().map(Value::String).unwrap_or(Value::Null));
                if !tool_calls.is_empty() {
                    let tcs: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| json!({
                            "id": tc.call_id,
                            "type": "function",
                            "function": {"name": tc.name, "arguments": tc.arguments}
                        }))
                        .collect();
                    obj.insert("tool_calls".into(), json!(tcs));
                }
                messages.push(Value::Object(obj));
            }
            Message::Tool { call_id, output } => {
                messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}))
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(upstream_model));
    body.insert("messages".into(), json!(messages));

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
            }))
            .collect();
        body.insert("tools".into(), json!(tools));
        body.insert("tool_choice".into(), tool_choice_json(&req.tool_choice));
    }

    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(m) = req.max_output_tokens {
        body.insert(provider.max_tokens_field.clone(), json!(m));
    }
    if let Some(e) = req.reasoning_effort {
        body.insert("reasoning_effort".into(), json!(e.as_str()));
    }
    if let Some(p) = req.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), json!(p));
    }
    if req.stream {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }

    Value::Object(body)
}

fn tool_choice_json(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Function(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::*;
    use crate::config::{Provider, WireName};
    use serde_json::json;
    use std::collections::HashMap;

    fn provider() -> Provider {
        Provider {
            wire: WireName::OpenaiChat,
            base_url: "https://opencode.ai/zen/v1".into(),
            api_key: Some("k".into()),
            model_prefix: Some("opencode/".into()),
            max_tokens_field: "max_tokens".into(),
            extra_headers: HashMap::new(),
        }
    }

    #[test]
    fn builds_messages_tools_and_stream_options() {
        let req = CanonicalRequest {
            model: "gpt-5.5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::User("hi".into()),
                Message::Assistant { text: None, tool_calls: vec![ToolCall {
                    call_id: "c1".into(), name: "f".into(), arguments: "{}".into() }] },
                Message::Tool { call_id: "c1".into(), output: "ok".into() },
            ],
            tools: vec![ToolDef { name: "f".into(), description: Some("d".into()), parameters: json!({"type":"object"}) }],
            tool_choice: ToolChoice::Required,
            temperature: Some(0.5),
            top_p: None,
            max_output_tokens: Some(100),
            reasoning_effort: Some(ReasoningEffort::High),
            parallel_tool_calls: Some(true),
            stream: true,
        };
        let body = build_request(&req, "opencode/gpt-5.5", &provider());
        assert_eq!(body["model"], "opencode/gpt-5.5");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "f");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn honors_max_tokens_field_name() {
        let mut p = provider();
        p.max_tokens_field = "max_completion_tokens".into();
        let req = CanonicalRequest {
            model: "m".into(), system: None, messages: vec![Message::User("x".into())],
            tools: vec![], tool_choice: ToolChoice::Auto, temperature: None, top_p: None,
            max_output_tokens: Some(42), reasoning_effort: None, parallel_tool_calls: None, stream: false,
        };
        let body = build_request(&req, "m", &p);
        assert_eq!(body["max_completion_tokens"], 42);
        assert!(body.get("stream").is_none());
    }
}
