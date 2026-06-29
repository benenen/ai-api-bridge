//! OpenAI Chat Completions wire format — outbound (client) side.

use std::borrow::Cow;

use serde_json::{Map, Value, json};

use crate::canonical::*;
use crate::config::Provider;
use crate::error::BridgeError;

/// Check whether any already-emitted assistant message (skipping past
/// intervening `tool` results) carries a tool_calls entry matching `call_id`.
/// This handles parallel tool calls: after `Assistant(c1,c2) → Tool(c1)`,
/// `Tool(c2)` must still find its parent by walking backwards.
fn find_matching_tool_call(msgs: &[Message], call_id: &str) -> bool {
    msgs.iter().rev().any(|m| match m {
        Message::Assistant { tool_calls, .. } => tool_calls.iter().any(|tc| tc.call_id == call_id),
        // Not a parent assistant — keep scanning backwards past tool results and
        // other turns (call_ids are unique, so no intervening message aliases it).
        _ => false,
    })
}

/// Ensure every `tool` message has a preceding `assistant` carrying a matching
/// `tool_calls` entry. Some clients (VS Code Copilot Chat via the Responses API)
/// send a `function_call_output` without repeating the `function_call` that
/// produced it — strict upstreams (DeepSeek) require the assistant→tool structure.
fn normalize_tool_parents(msgs: &[Message]) -> Cow<'_, [Message]> {
    // Fast path: no tool messages → nothing to fix, borrow without allocating.
    if !msgs.iter().any(|m| matches!(m, Message::Tool { .. })) {
        return Cow::Borrowed(msgs);
    }
    let mut out = Vec::with_capacity(msgs.len() + 4);
    for m in msgs {
        match m {
            Message::Tool { call_id, output } => {
                if !find_matching_tool_call(&out, call_id) {
                    // Synthesize an assistant carrying a dummy tool_calls entry so
                    // the following `tool` passes strict upstream validation
                    // (DeepSeek). Set reasoning_content to "" — in thinking mode
                    // DeepSeek requires the field to be present on every assistant,
                    // and an empty string means "this turn had no reasoning."
                    // reasoning_content is set unconditionally; non-thinking
                    // models ignore the extra field harmlessly.
                    out.push(Message::Assistant {
                        text: None,
                        reasoning_content: Some(String::new()),
                        tool_calls: vec![ToolCall {
                            call_id: call_id.clone(),
                            name: String::from("_"),
                            arguments: String::from("{}"),
                        }],
                    });
                }
                out.push(Message::Tool {
                    call_id: call_id.clone(),
                    output: output.clone(),
                });
            }
            other => out.push(other.clone()),
        }
    }
    Cow::Owned(out)
}

pub fn build_request(req: &CanonicalRequest, upstream_model: &str, provider: &Provider) -> Value {
    let messages_normalized = normalize_tool_parents(&req.messages);
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = &req.system {
        messages.push(json!({"role": "system", "content": sys}));
    }
    for m in messages_normalized.iter() {
        match m {
            Message::User(text) => messages.push(json!({"role": "user", "content": text})),
            Message::Assistant {
                text,
                reasoning_content,
                tool_calls,
            } => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert(
                    "content".into(),
                    text.clone().map(Value::String).unwrap_or(Value::Null),
                );
                if let Some(rc) = reasoning_content {
                    obj.insert("reasoning_content".into(), json!(rc));
                }
                if !tool_calls.is_empty() {
                    let tcs: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.call_id,
                                "type": "function",
                                "function": {"name": tc.name, "arguments": tc.arguments}
                            })
                        })
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
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": sanitize_parameters(&t.parameters),
                    }
                })
            })
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

/// Repair a tool's `parameters` JSON Schema before forwarding to a strict upstream.
///
/// Some clients (notably VS Code Copilot's built-in tools such as
/// `terminal_last_command`) emit schemas carrying `"type": null` instead of a
/// real type — invalid JSON Schema that DeepSeek and other strict validators
/// reject. Function `parameters` must also be an object schema, so a missing or
/// non-object schema is coerced to an empty one.
fn sanitize_parameters(params: &Value) -> Value {
    match params {
        Value::Object(_) => {
            let mut v = params.clone();
            sanitize_schema(&mut v, true);
            v
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// Recursively repair a JSON Schema node in place: replace an invalid
/// `"type": null` with an inferred type (`object`/`array` from shape hints, or
/// `object` at the schema root), otherwise drop the key — a schema with no
/// `type` is valid and means "any".
fn sanitize_schema(node: &mut Value, is_root: bool) {
    match node {
        Value::Object(map) => {
            if matches!(map.get("type"), Some(Value::Null)) {
                if is_root || map.contains_key("properties") {
                    map.insert("type".into(), json!("object"));
                } else if map.contains_key("items") {
                    map.insert("type".into(), json!("array"));
                } else {
                    map.remove("type");
                }
            }
            // At the root, a missing `type` also defaults to "object" —
            // function parameters must be an object schema, and an empty
            // `{}` (which VS Code Copilot sends for e.g. terminal_last_command)
            // is technically "any" but strict upstreams may reject it.
            if is_root && !map.contains_key("type") {
                map.insert("type".into(), json!("object"));
            }
            for child in map.values_mut() {
                sanitize_schema(child, false);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                sanitize_schema(child, false);
            }
        }
        _ => {}
    }
}

/// Parse a Chat Completions *request* body into the canonical request
/// (used when the bridge serves the Chat Completions inbound endpoint).
pub fn parse_request(body: &Value) -> Result<CanonicalRequest, BridgeError> {
    let obj = body
        .as_object()
        .ok_or_else(|| BridgeError::BadRequest("body must be a JSON object".into()))?;
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::BadRequest("missing `model`".into()))?
        .to_string();
    let mut system = None;
    let mut messages = Vec::new();
    if let Some(arr) = obj.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = m
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match role {
                "system" | "developer" => system = Some(content),
                "tool" => messages.push(Message::Tool {
                    call_id: m
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    output: content,
                }),
                "assistant" => {
                    let rc = m
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    // Null/absent/empty content (e.g. a pure tool-call turn) -> no text.
                    let text = m
                        .get("content")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    messages.push(Message::Assistant {
                        text,
                        reasoning_content: rc,
                        tool_calls: parse_chat_tool_calls(m.get("tool_calls")),
                    })
                }
                _ => messages.push(Message::User(content)),
            }
        }
    }
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(CanonicalRequest {
        model,
        system,
        messages,
        tools: parse_chat_tools(obj.get("tools")),
        tool_choice: parse_chat_tool_choice(obj.get("tool_choice")),
        temperature: obj
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32),
        top_p: None,
        max_output_tokens: obj
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32),
        reasoning_effort: None,
        parallel_tool_calls: obj.get("parallel_tool_calls").and_then(|v| v.as_bool()),
        stream,
    })
}

/// Parse Chat Completions `tools` (each def nested under `function`) into canonical
/// `ToolDef`s. Only `type: "function"` entries are kept; raw `parameters` are carried
/// through verbatim (`build_request` sanitizes the schema before forwarding upstream).
fn parse_chat_tools(v: Option<&Value>) -> Vec<ToolDef> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            let name = f.get("name").and_then(|v| v.as_str())?.to_string();
            Some(ToolDef {
                name,
                description: f
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                parameters: f.get("parameters").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Parse a Chat Completions `tool_choice` (`"none"`/`"auto"`/`"required"`, or
/// `{"type":"function","function":{"name":..}}`) into the canonical choice.
fn parse_chat_tool_choice(v: Option<&Value>) -> ToolChoice {
    match v {
        Some(Value::String(s)) => match s.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(Value::Object(o)) => o
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Function(n.to_string()))
            .unwrap_or(ToolChoice::Auto),
        _ => ToolChoice::Auto,
    }
}

/// Parse assistant-message `tool_calls` (Chat Completions shape:
/// `{"id":..,"function":{"name":..,"arguments":..}}`) into canonical `ToolCall`s.
fn parse_chat_tool_calls(v: Option<&Value>) -> Vec<ToolCall> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|tc| {
            let f = tc.get("function")?;
            let name = f.get("name").and_then(|v| v.as_str())?.to_string();
            Some(ToolCall {
                call_id: tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name,
                arguments: f
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

fn tool_choice_json(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Function(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

use std::collections::HashSet;

/// Stateful translator: upstream Chat Completions stream chunks -> canonical events.
#[derive(Default)]
pub struct ChatStreamParser {
    response_id: String,
    model: String,
    created: bool,
    started_tools: HashSet<u32>,
}

impl ChatStreamParser {
    pub fn new(response_id: String) -> Self {
        Self {
            response_id,
            ..Default::default()
        }
    }

    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<CanonicalEvent> {
        if let Some(err) = chunk.get("error")
            && !err.is_null()
        {
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("upstream error")
                .to_string();
            let status = err
                .get("code")
                .and_then(|v| v.as_u64())
                .map(|n| n as u16)
                .unwrap_or(502);
            return vec![CanonicalEvent::Error { message, status }];
        }
        let mut events = Vec::new();
        if !self.created {
            self.model = chunk
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            events.push(CanonicalEvent::Created {
                response_id: self.response_id.clone(),
                model: self.model.clone(),
            });
            self.created = true;
        }

        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());

        if let Some(choice) = choice {
            if let Some(delta) = choice.get("delta") {
                if let Some(rc) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                    && !rc.is_empty()
                {
                    events.push(CanonicalEvent::ReasoningDelta {
                        text: rc.to_string(),
                    });
                }
                if let Some(c) = delta.get("content").and_then(|v| v.as_str())
                    && !c.is_empty()
                {
                    events.push(CanonicalEvent::TextDelta {
                        text: c.to_string(),
                    });
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        if !self.started_tools.contains(&index) {
                            let call_id = tc
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            self.started_tools.insert(index);
                            events.push(CanonicalEvent::ToolCallStart {
                                index,
                                call_id,
                                name,
                            });
                        }
                        if let Some(args) = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            && !args.is_empty()
                        {
                            events.push(CanonicalEvent::ToolCallArgsDelta {
                                index,
                                delta: args.to_string(),
                            });
                        }
                    }
                }
            }
            if choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .is_some()
            {
                let mut idxs: Vec<u32> = self.started_tools.iter().copied().collect();
                idxs.sort_unstable();
                for i in idxs {
                    events.push(CanonicalEvent::ToolCallDone { index: i });
                }
            }
        }

        if let Some(u) = chunk.get("usage")
            && !u.is_null()
        {
            events.push(usage_event(u));
        }
        events
    }
}

/// Translate a full (non-streaming) Chat Completions response into canonical events.
pub fn completion_to_events(resp: &Value, response_id: &str) -> Vec<CanonicalEvent> {
    let mut events = Vec::new();
    let model = resp
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    events.push(CanonicalEvent::Created {
        response_id: response_id.to_string(),
        model,
    });

    if let Some(msg) = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
    {
        if let Some(rc) = msg
            .get("reasoning_content")
            .or_else(|| msg.get("reasoning"))
            .and_then(|v| v.as_str())
            && !rc.is_empty()
        {
            events.push(CanonicalEvent::ReasoningDelta {
                text: rc.to_string(),
            });
        }
        if let Some(c) = msg.get("content").and_then(|v| v.as_str())
            && !c.is_empty()
        {
            events.push(CanonicalEvent::TextDelta {
                text: c.to_string(),
            });
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for (i, tc) in tcs.iter().enumerate() {
                let index = i as u32;
                let call_id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                events.push(CanonicalEvent::ToolCallStart {
                    index,
                    call_id,
                    name,
                });
                if !args.is_empty() {
                    events.push(CanonicalEvent::ToolCallArgsDelta { index, delta: args });
                }
                events.push(CanonicalEvent::ToolCallDone { index });
            }
        }
    }

    if let Some(u) = resp.get("usage")
        && !u.is_null()
    {
        events.push(usage_event(u));
    }
    events.push(CanonicalEvent::Completed);
    events
}

fn usage_event(u: &Value) -> CanonicalEvent {
    // Prefix-cache hits: OpenAI exposes `prompt_tokens_details.cached_tokens`;
    // DeepSeek also reports `prompt_cache_hit_tokens`. Prefer the former, fall
    // back to the latter so both upstream dialects pass through.
    let cached_input_tokens = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0) as u32;
    // Reasoning tokens (reasoning models): a subset of completion_tokens.
    let reasoning_tokens = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    CanonicalEvent::Usage {
        input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        output_tokens: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        cached_input_tokens,
        reasoning_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProbeSource, Provider, WireName};
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
            probe_script: None,
            probe_script_text: None,
            probe_source: ProbeSource::Path,
            probe_enabled: None,
            probe_interval_secs: None,
            quota_min: None,
            cost_windows: Vec::new(),
            model_prices: std::collections::HashMap::new(),
            usage: Vec::new(),
            models: Vec::new(),
        }
    }

    #[test]
    fn builds_messages_tools_and_stream_options() {
        let req = CanonicalRequest {
            model: "gpt-5.5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::User("hi".into()),
                Message::Assistant {
                    text: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        call_id: "c1".into(),
                        name: "f".into(),
                        arguments: "{}".into(),
                    }],
                },
                Message::Tool {
                    call_id: "c1".into(),
                    output: "ok".into(),
                },
            ],
            tools: vec![ToolDef {
                name: "f".into(),
                description: Some("d".into()),
                parameters: json!({"type":"object"}),
            }],
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
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "f"
        );
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
    fn sanitizes_invalid_tool_parameter_schemas() {
        // Some clients (e.g. VS Code Copilot's built-in tools like
        // `terminal_last_command`) emit `"type": null` instead of `"object"`,
        // which strict upstreams (DeepSeek) reject. The bridge must repair the
        // schema before forwarding.
        let req = CanonicalRequest {
            model: "m".into(),
            system: None,
            messages: vec![Message::User("x".into())],
            tools: vec![
                // Top-level `type: null` with properties -> "object".
                ToolDef {
                    name: "with_props".into(),
                    description: None,
                    parameters: json!({"type": null, "properties": {"path": {"type": "string"}}}),
                },
                // No schema at all -> minimal valid object schema.
                ToolDef {
                    name: "no_args".into(),
                    description: None,
                    parameters: Value::Null,
                },
                // Nested `type: null` with no shape hint -> drop the key (valid "any");
                // sibling keys preserved.
                ToolDef {
                    name: "nested".into(),
                    description: None,
                    parameters: json!({
                        "type": "object",
                        "properties": {"x": {"type": null, "description": "d"}}
                    }),
                },
                // Empty schema {} (terminal_last_command style) -> gets type: "object".
                ToolDef {
                    name: "empty".into(),
                    description: None,
                    parameters: json!({}),
                },
            ],
            tool_choice: ToolChoice::Auto,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            reasoning_effort: None,
            parallel_tool_calls: None,
            stream: false,
        };
        let body = build_request(&req, "m", &provider());
        let tools = &body["tools"];

        // Top-level null type repaired to "object"; existing properties preserved.
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );

        // Missing schema becomes a valid empty object schema.
        assert_eq!(tools[1]["function"]["parameters"]["type"], "object");
        assert!(tools[1]["function"]["parameters"]["properties"].is_object());

        // Nested null type dropped, leaving a valid schema; sibling keys preserved.
        let x = &tools[2]["function"]["parameters"]["properties"]["x"];
        assert!(x.get("type").is_none());
        assert_eq!(x["description"], "d");

        // Empty {} root schema defaults to type: "object".
        assert_eq!(tools[3]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn normalizes_orphaned_tool_messages() {
        // Some clients (VS Code Copilot Chat) send function_call_output without
        // repeating the function_call. DeepSeek requires every `tool` to follow
        // an `assistant` with matching `tool_calls`. normalize_tool_parents must
        // synthesize the missing assistant.
        let req = CanonicalRequest {
            model: "m".into(),
            system: None,
            messages: vec![
                Message::User("do it".into()),
                // Orphaned tool — no preceding assistant with tool_calls.
                Message::Tool {
                    call_id: "c1".into(),
                    output: "result".into(),
                },
                Message::User("next".into()),
            ],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            reasoning_effort: None,
            parallel_tool_calls: None,
            stream: false,
        };
        let body = build_request(&req, "m", &provider());
        let msgs = body["messages"].as_array().unwrap();
        // messages should be: [user "do it", assistant {tool_calls: [c1/_]}, tool "result", user "next"]
        assert_eq!(msgs.len(), 4, "expected 4 messages, got {msgs:?}");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].is_null());
        // Synthesized assistant must carry reasoning_content (empty) so DeepSeek
        // thinking mode passes validation.
        assert_eq!(msgs[1]["reasoning_content"], "");
        let tc = &msgs[1]["tool_calls"][0];
        assert_eq!(tc["id"], "c1");
        assert_eq!(tc["function"]["name"], "_");
        assert_eq!(tc["function"]["arguments"], "{}");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"], "next");
    }

    #[test]
    fn leaves_paired_tool_messages_unchanged() {
        // When a tool message already has a preceding matching assistant, don't
        // add a duplicate.
        let req = CanonicalRequest {
            model: "m".into(),
            system: None,
            messages: vec![
                Message::User("do it".into()),
                Message::Assistant {
                    text: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        call_id: "c1".into(),
                        name: "exec".into(),
                        arguments: "{}".into(),
                    }],
                },
                Message::Tool {
                    call_id: "c1".into(),
                    output: "result".into(),
                },
            ],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            reasoning_effort: None,
            parallel_tool_calls: None,
            stream: false,
        };
        let body = build_request(&req, "m", &provider());
        let msgs = body["messages"].as_array().unwrap();
        // Should remain 3 messages — no synthesized assistant.
        assert_eq!(msgs.len(), 3, "expected 3 messages, got {msgs:?}");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "exec");
        assert_eq!(msgs[2]["role"], "tool");
    }

    #[test]
    fn finds_parent_across_parallel_tool_results() {
        // Assistant(c1,c2) → Tool(c1) → Tool(c2): when processing Tool(c2),
        // `out.last()` is Tool(c1) — a naive last() check would miss the
        // parent and synthesize a duplicate assistant.
        let req = CanonicalRequest {
            model: "m".into(),
            system: None,
            messages: vec![
                Message::User("do two things".into()),
                Message::Assistant {
                    text: None,
                    reasoning_content: Some("need parallel calls".into()),
                    tool_calls: vec![
                        ToolCall {
                            call_id: "c1".into(),
                            name: "read".into(),
                            arguments: r#"{"path":"a"}"#.into(),
                        },
                        ToolCall {
                            call_id: "c2".into(),
                            name: "read".into(),
                            arguments: r#"{"path":"b"}"#.into(),
                        },
                    ],
                },
                Message::Tool {
                    call_id: "c1".into(),
                    output: "content a".into(),
                },
                Message::Tool {
                    call_id: "c2".into(),
                    output: "content b".into(),
                },
                Message::User("summarize".into()),
            ],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            reasoning_effort: None,
            parallel_tool_calls: None,
            stream: false,
        };
        let body = build_request(&req, "m", &provider());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "expected 5 messages, got {msgs:?}");
        // msg[0]: user
        assert_eq!(msgs[0]["role"], "user");
        // msg[1]: assistant with both tool_calls
        assert_eq!(msgs[1]["role"], "assistant");
        let tcs = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0]["id"], "c1");
        assert_eq!(tcs[1]["id"], "c2");
        // reasoning_content must survive round-trip
        assert_eq!(msgs[1]["reasoning_content"], "need parallel calls");
        // msg[2]: tool result for c1
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "c1");
        // msg[3]: tool result for c2 — must NOT have an extra synthesized assistant
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c2");
        // msg[4]: user follow-up
        assert_eq!(msgs[4]["role"], "user");
    }

    #[test]
    fn honors_max_tokens_field_name() {
        let mut p = provider();
        p.max_tokens_field = "max_completion_tokens".into();
        let req = CanonicalRequest {
            model: "m".into(),
            system: None,
            messages: vec![Message::User("x".into())],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            temperature: None,
            top_p: None,
            max_output_tokens: Some(42),
            reasoning_effort: None,
            parallel_tool_calls: None,
            stream: false,
        };
        let body = build_request(&req, "m", &p);
        assert_eq!(body["max_completion_tokens"], 42);
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn stream_parser_emits_text_then_usage() {
        let mut p = ChatStreamParser::new("resp_x".into());
        let mut evs = p.on_chunk(&json!({"model":"opencode/gpt-5.5",
            "choices":[{"delta":{"content":"Hel"}}]}));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{"content":"lo"}}]})));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
        evs.extend(p.on_chunk(&json!({"choices":[],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,
                     "prompt_tokens_details":{"cached_tokens":1},
                     "completion_tokens_details":{"reasoning_tokens":1}}})));
        use CanonicalEvent::*;
        assert_eq!(
            evs[0],
            Created {
                response_id: "resp_x".into(),
                model: "opencode/gpt-5.5".into()
            }
        );
        assert_eq!(evs[1], TextDelta { text: "Hel".into() });
        assert_eq!(evs[2], TextDelta { text: "lo".into() });
        assert_eq!(
            evs.last().unwrap(),
            &Usage {
                input_tokens: 3,
                output_tokens: 2,
                total_tokens: 5,
                cached_input_tokens: 1,
                reasoning_tokens: 1
            }
        );
    }

    #[test]
    fn stream_parser_handles_tool_call_fragments() {
        let mut p = ChatStreamParser::new("r".into());
        let mut evs = p.on_chunk(&json!({"model":"m","choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"get","arguments":"{\"a\""}}]}}]}));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":":1}"}}]}}]})));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
        use CanonicalEvent::*;
        assert!(evs.contains(&ToolCallStart {
            index: 0,
            call_id: "call_1".into(),
            name: "get".into()
        }));
        assert!(evs.contains(&ToolCallArgsDelta {
            index: 0,
            delta: "{\"a\"".into()
        }));
        assert!(evs.contains(&ToolCallArgsDelta {
            index: 0,
            delta: ":1}".into()
        }));
        assert!(evs.contains(&ToolCallDone { index: 0 }));
    }

    #[test]
    fn stream_parser_surfaces_inband_error() {
        let mut p = ChatStreamParser::new("r".into());
        let evs = p.on_chunk(&json!({"error":{"message":"rate limited","code":429}}));
        use CanonicalEvent::*;
        assert_eq!(
            evs,
            vec![Error {
                message: "rate limited".into(),
                status: 429
            }]
        );
    }

    #[test]
    fn parses_chat_inbound_request() {
        let body = json!({"model": "gpt-5.5", "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "hi"}
        ], "stream": true, "max_tokens": 50});
        let req = parse_request(&body).unwrap();
        assert_eq!(req.model, "gpt-5.5");
        assert_eq!(req.system.as_deref(), Some("s"));
        assert_eq!(req.messages, vec![Message::User("hi".into())]);
        assert!(req.stream);
        assert_eq!(req.max_output_tokens, Some(50));
    }

    #[test]
    fn parses_chat_inbound_tools_and_tool_calls() {
        let body = json!({"model": "m", "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": "f", "arguments": "{\"a\":1}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ],
        "tools": [
            {"type": "function",
             "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}}
        ],
        "tool_choice": {"type": "function", "function": {"name": "f"}},
        "parallel_tool_calls": true});
        let req = parse_request(&body).unwrap();

        // Tools parsed from the nested Chat Completions shape (function.{name,..}).
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "f");
        assert_eq!(req.tools[0].description.as_deref(), Some("d"));
        assert_eq!(req.tools[0].parameters["type"], "object");

        // tool_choice object form reads function.name; parallel flag carried through.
        assert_eq!(req.tool_choice, ToolChoice::Function("f".into()));
        assert_eq!(req.parallel_tool_calls, Some(true));

        // Assistant tool_calls parsed; null content -> no text.
        match &req.messages[1] {
            Message::Assistant {
                text, tool_calls, ..
            } => {
                assert!(text.is_none());
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].call_id, "c1");
                assert_eq!(tool_calls[0].name, "f");
                assert_eq!(tool_calls[0].arguments, "{\"a\":1}");
            }
            other => panic!("expected assistant tool call, got {other:?}"),
        }
        // Tool result still parsed.
        assert_eq!(
            req.messages[2],
            Message::Tool {
                call_id: "c1".into(),
                output: "ok".into()
            }
        );
    }

    #[test]
    fn completion_to_events_full_message() {
        let resp = json!({"model":"m","choices":[{"message":{"content":"hi",
            "tool_calls":[{"id":"c","function":{"name":"f","arguments":"{}"}}]}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        let evs = completion_to_events(&resp, "r1");
        use CanonicalEvent::*;
        assert_eq!(
            evs.first().unwrap(),
            &Created {
                response_id: "r1".into(),
                model: "m".into()
            }
        );
        assert!(evs.contains(&TextDelta { text: "hi".into() }));
        assert!(evs.contains(&ToolCallStart {
            index: 0,
            call_id: "c".into(),
            name: "f".into()
        }));
        assert_eq!(evs.last().unwrap(), &Completed);
    }
}
