//! OpenAI Responses API wire format — inbound (server) side.

use serde_json::Value;

use crate::canonical::*;
use crate::error::BridgeError;

pub fn parse_request(body: &Value) -> Result<CanonicalRequest, BridgeError> {
    let obj = body
        .as_object()
        .ok_or_else(|| BridgeError::BadRequest("body must be a JSON object".into()))?;

    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::BadRequest("missing `model`".into()))?
        .to_string();

    let mut system = obj
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut messages = Vec::new();
    match obj.get("input") {
        Some(Value::String(s)) => messages.push(Message::User(s.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                parse_input_item(item, &mut messages, &mut system);
            }
        }
        Some(_) => return Err(BridgeError::BadRequest("`input` must be string or array".into())),
        None => {}
    }

    let tools = parse_tools(obj.get("tools"))?;
    let tool_choice = parse_tool_choice(obj.get("tool_choice"));
    let temperature = obj.get("temperature").and_then(|v| v.as_f64()).map(|f| f as f32);
    let top_p = obj.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32);
    let max_output_tokens = obj.get("max_output_tokens").and_then(|v| v.as_u64()).map(|n| n as u32);
    let reasoning_effort = obj
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .and_then(ReasoningEffort::from_str_opt);
    let parallel_tool_calls = obj.get("parallel_tool_calls").and_then(|v| v.as_bool());
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    Ok(CanonicalRequest {
        model,
        system,
        messages,
        tools,
        tool_choice,
        temperature,
        top_p,
        max_output_tokens,
        reasoning_effort,
        parallel_tool_calls,
        stream,
    })
}

fn parse_input_item(item: &Value, messages: &mut Vec<Message>, system: &mut Option<String>) {
    let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("message");
    match kind {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let text = extract_text(item.get("content"));
            match role {
                "system" | "developer" => append_system(system, &text),
                "assistant" => messages.push(Message::Assistant { text: Some(text), tool_calls: vec![] }),
                _ => messages.push(Message::User(text)),
            }
        }
        "function_call" => {
            let call = ToolCall {
                call_id: str_field(item, "call_id"),
                name: str_field(item, "name"),
                arguments: str_field(item, "arguments"),
            };
            if let Some(Message::Assistant { tool_calls, .. }) = messages.last_mut() {
                tool_calls.push(call);
            } else {
                messages.push(Message::Assistant { text: None, tool_calls: vec![call] });
            }
        }
        "function_call_output" => {
            let output = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            messages.push(Message::Tool { call_id: str_field(item, "call_id"), output });
        }
        // "reasoning" items are intentionally dropped (Chat Completions is stateless).
        _ => {}
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn append_system(system: &mut Option<String>, text: &str) {
    match system {
        Some(s) => {
            s.push('\n');
            s.push_str(text);
        }
        None => *system = Some(text.to_string()),
    }
}

fn parse_tools(v: Option<&Value>) -> Result<Vec<ToolDef>, BridgeError> {
    let Some(Value::Array(arr)) = v else { return Ok(vec![]) };
    let mut tools = Vec::new();
    for t in arr {
        if t.get("type").and_then(|x| x.as_str()) != Some("function") {
            continue;
        }
        let name = t
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| BridgeError::BadRequest("tool missing `name`".into()))?
            .to_string();
        tools.push(ToolDef {
            name,
            description: t.get("description").and_then(|x| x.as_str()).map(|s| s.to_string()),
            parameters: t.get("parameters").cloned().unwrap_or(Value::Null),
        });
    }
    Ok(tools)
}

fn parse_tool_choice(v: Option<&Value>) -> ToolChoice {
    match v {
        Some(Value::String(s)) => match s.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(Value::Object(o)) => o
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Function(n.to_string()))
            .unwrap_or(ToolChoice::Auto),
        _ => ToolChoice::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::*;
    use serde_json::json;

    #[test]
    fn parses_instructions_and_string_input() {
        let body = json!({
            "model": "gpt-5.5",
            "instructions": "be terse",
            "input": "hello",
            "stream": true
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.model, "gpt-5.5");
        assert_eq!(req.system.as_deref(), Some("be terse"));
        assert_eq!(req.messages, vec![Message::User("hello".into())]);
        assert!(req.stream);
    }

    #[test]
    fn parses_array_input_with_tool_roundtrip() {
        let body = json!({
            "model": "gpt-5.5",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "weather?"}]},
                {"type": "function_call", "call_id": "c1", "name": "get_weather",
                 "arguments": "{\"city\":\"SF\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "sunny"}
            ],
            "tools": [{"type": "function", "name": "get_weather",
                       "description": "w", "parameters": {"type": "object"}}],
            "tool_choice": "auto"
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0], Message::User("weather?".into()));
        match &req.messages[1] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls[0].name, "get_weather");
                assert_eq!(tool_calls[0].call_id, "c1");
            }
            other => panic!("expected assistant tool call, got {other:?}"),
        }
        assert_eq!(req.messages[2], Message::Tool { call_id: "c1".into(), output: "sunny".into() });
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "get_weather");
        assert_eq!(req.tool_choice, ToolChoice::Auto);
    }

    #[test]
    fn parses_reasoning_effort_and_max_tokens() {
        let body = json!({
            "model": "gpt-5.5",
            "input": "hi",
            "reasoning": {"effort": "xhigh"},
            "max_output_tokens": 2048
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::XHigh));
        assert_eq!(req.max_output_tokens, Some(2048));
    }

    #[test]
    fn rejects_missing_model() {
        let err = parse_request(&json!({"input": "x"})).unwrap_err();
        assert!(matches!(err, crate::error::BridgeError::BadRequest(_)));
    }
}
