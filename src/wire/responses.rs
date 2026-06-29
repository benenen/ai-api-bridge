//! OpenAI Responses API wire format — inbound (server) side.

use serde_json::Value;

use crate::canonical::*;
use crate::error::BridgeError;
use crate::wire::{CanonicalEmitter, SseFrame, frame, new_id};

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
    // Reasoning items precede the assistant turn they belong to; stash the most
    // recent one and attach it to the next assistant message/function_call.
    let mut pending_reasoning: Option<String> = None;
    match obj.get("input") {
        Some(Value::String(s)) => messages.push(Message::User(s.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                parse_input_item(item, &mut messages, &mut system, &mut pending_reasoning);
            }
        }
        Some(_) => {
            return Err(BridgeError::BadRequest(
                "`input` must be string or array".into(),
            ));
        }
        None => {}
    }

    let tools = parse_tools(obj.get("tools"))?;
    let tool_choice = parse_tool_choice(obj.get("tool_choice"));
    let temperature = obj
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    let top_p = obj.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32);
    let max_output_tokens = obj
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
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

fn parse_input_item(
    item: &Value,
    messages: &mut Vec<Message>,
    system: &mut Option<String>,
    pending_reasoning: &mut Option<String>,
) {
    let kind = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");
    match kind {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let text = extract_text(item.get("content"));
            match role {
                "system" | "developer" => append_system(system, &text),
                // An assistant preamble message opens the assistant turn; it owns
                // any reasoning that preceded it. A following function_call in the
                // same turn merges into this message.
                "assistant" => messages.push(Message::Assistant {
                    text: Some(text),
                    reasoning_content: pending_reasoning.take(),
                    tool_calls: vec![],
                }),
                // A user turn starts; any unconsumed reasoning is orphaned.
                _ => {
                    *pending_reasoning = None;
                    messages.push(Message::User(text));
                }
            }
        }
        "function_call" => {
            let call = ToolCall {
                call_id: str_field(item, "call_id"),
                name: str_field(item, "name"),
                arguments: str_field(item, "arguments"),
            };
            match messages.last_mut() {
                // Same assistant turn (opened by a preamble message or an earlier
                // function_call): append the call, and adopt pending reasoning if
                // this assistant doesn't have any yet.
                Some(Message::Assistant {
                    tool_calls,
                    reasoning_content,
                    ..
                }) => {
                    tool_calls.push(call);
                    if reasoning_content.is_none() {
                        *reasoning_content = pending_reasoning.take();
                    }
                }
                // The function_call opens a fresh assistant turn.
                _ => messages.push(Message::Assistant {
                    text: None,
                    reasoning_content: pending_reasoning.take(),
                    tool_calls: vec![call],
                }),
            }
        }
        "function_call_output" => {
            let output = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let call_id = str_field(item, "call_id");
            // The tool result ends the assistant turn; drop any unconsumed reasoning.
            *pending_reasoning = None;
            // Some clients (notably VS Code Copilot Chat) send only the
            // function_call_output without repeating the function_call that
            // produced it. Strict upstreams (DeepSeek) require every `tool`
            // message to follow an `assistant` carrying matching `tool_calls`,
            // but synthesizing that missing parent is the job of the single
            // outbound chokepoint (`normalize_tool_parents` in
            // wire::chat::build_request) — which also covers the Anthropic
            // inbound path — so the parser just emits the bare tool result here.
            messages.push(Message::Tool { call_id, output });
        }
        // Stash reasoning to attach to the assistant turn that follows it.
        "reasoning" => {
            let text = item
                .get("summary")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| e.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            if !text.is_empty() {
                *pending_reasoning = Some(text);
            }
        }
        _ => {}
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
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
    let Some(Value::Array(arr)) = v else {
        return Ok(vec![]);
    };
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
            description: t
                .get("description")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
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

use serde_json::json;

struct OpenItem {
    id: String,
    output_index: u32,
}

struct ToolItem {
    id: String,
    output_index: u32,
    call_id: String,
    name: String,
    args: String,
}

/// Stateful translator: canonical events -> Responses API SSE frames.
#[derive(Default)]
pub struct ResponsesEmitter {
    response_id: String,
    model: String,
    next_output_index: u32,
    reasoning_item: Option<OpenItem>,
    reasoning_text: String,
    message_item: Option<OpenItem>,
    message_text: String,
    tools: std::collections::BTreeMap<u32, ToolItem>,
    /// (input_tokens, output_tokens, total_tokens, cached_input_tokens, reasoning_tokens)
    usage: Option<(u32, u32, u32, u32, u32)>,
    final_items: Vec<Value>,
    seq: u64,
}

impl CanonicalEmitter for ResponsesEmitter {
    fn emit(&mut self, ev: &CanonicalEvent) -> Vec<SseFrame> {
        self.on_event(ev)
    }
}

impl ResponsesEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_event(&mut self, ev: &CanonicalEvent) -> Vec<SseFrame> {
        let mut f = Vec::new();
        match ev {
            CanonicalEvent::Created { response_id, model } => {
                self.response_id = response_id.clone();
                self.model = model.clone();
                f.push(frame(
                    "response.created",
                    json!({
                        "type": "response.created",
                        "response": self.skeleton("in_progress")
                    }),
                ));
            }
            CanonicalEvent::ReasoningDelta { text } => {
                self.ensure_reasoning_open(&mut f);
                self.reasoning_text.push_str(text);
                let item = self.reasoning_item.as_ref().unwrap();
                f.push(frame(
                    "response.reasoning_summary_text.delta",
                    json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": item.id, "output_index": item.output_index,
                        "summary_index": 0, "delta": text
                    }),
                ));
            }
            CanonicalEvent::TextDelta { text } => {
                self.close_reasoning(&mut f);
                self.ensure_message_open(&mut f);
                self.message_text.push_str(text);
                let item = self.message_item.as_ref().unwrap();
                f.push(frame(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": item.id, "output_index": item.output_index,
                        "content_index": 0, "delta": text
                    }),
                ));
            }
            CanonicalEvent::ToolCallStart {
                index,
                call_id,
                name,
            } => {
                self.close_reasoning(&mut f);
                self.close_message(&mut f);
                let output_index = self.alloc_index();
                let id = new_id("fc");
                f.push(frame(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {"type": "function_call", "id": id, "call_id": call_id,
                                 "name": name, "arguments": "", "status": "in_progress"}
                    }),
                ));
                self.tools.insert(
                    *index,
                    ToolItem {
                        id,
                        output_index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args: String::new(),
                    },
                );
            }
            CanonicalEvent::ToolCallArgsDelta { index, delta } => {
                if let Some(t) = self.tools.get_mut(index) {
                    t.args.push_str(delta);
                    f.push(frame(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": t.id, "output_index": t.output_index, "delta": delta
                        }),
                    ));
                }
            }
            CanonicalEvent::ToolCallDone { index } => {
                if let Some(t) = self.tools.remove(index) {
                    self.finish_tool(&mut f, t);
                }
            }
            CanonicalEvent::Usage {
                input_tokens,
                output_tokens,
                total_tokens,
                cached_input_tokens,
                reasoning_tokens,
            } => {
                self.usage = Some((
                    *input_tokens,
                    *output_tokens,
                    *total_tokens,
                    *cached_input_tokens,
                    *reasoning_tokens,
                ));
            }
            CanonicalEvent::Completed => {
                self.close_reasoning(&mut f);
                self.close_message(&mut f);
                self.close_open_tools(&mut f);
                f.push(frame(
                    "response.completed",
                    json!({
                        "type": "response.completed",
                        "response": self.completed_response()
                    }),
                ));
            }
            CanonicalEvent::Error { message, status } => {
                f.push(frame(
                    "response.failed",
                    json!({
                        "type": "response.failed",
                        "response": {"id": self.response_id, "status": "failed",
                            "error": {"code": status, "message": message}}
                    }),
                ));
            }
        }
        for fr in &mut f {
            if let Value::Object(map) = &mut fr.data {
                map.insert("sequence_number".into(), serde_json::json!(self.seq));
            }
            self.seq += 1;
        }
        f
    }

    /// The full Responses object (valid after a `Completed` event has been processed).
    pub fn final_response(&self) -> Value {
        self.completed_response()
    }

    fn alloc_index(&mut self) -> u32 {
        let i = self.next_output_index;
        self.next_output_index += 1;
        i
    }

    fn ensure_reasoning_open(&mut self, f: &mut Vec<SseFrame>) {
        if self.reasoning_item.is_some() {
            return;
        }
        let output_index = self.alloc_index();
        let id = new_id("rs");
        f.push(frame(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added", "output_index": output_index,
                "item": {"type": "reasoning", "id": id, "summary": []}
            }),
        ));
        self.reasoning_item = Some(OpenItem { id, output_index });
    }

    fn close_reasoning(&mut self, f: &mut Vec<SseFrame>) {
        if let Some(item) = self.reasoning_item.take() {
            let item_json = json!({"type": "reasoning", "id": item.id,
                "summary": [{"type": "summary_text", "text": self.reasoning_text}]});
            f.push(frame(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": item.output_index, "item": item_json.clone()
                }),
            ));
            self.final_items.push(item_json);
        }
    }

    fn ensure_message_open(&mut self, f: &mut Vec<SseFrame>) {
        if self.message_item.is_some() {
            return;
        }
        let output_index = self.alloc_index();
        let id = new_id("msg");
        f.push(frame(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added", "output_index": output_index,
                "item": {"type": "message", "id": id, "role": "assistant",
                         "status": "in_progress", "content": []}
            }),
        ));
        f.push(frame(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added", "item_id": id,
                "output_index": output_index, "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }),
        ));
        self.message_item = Some(OpenItem { id, output_index });
    }

    fn close_message(&mut self, f: &mut Vec<SseFrame>) {
        if let Some(item) = self.message_item.take() {
            f.push(frame(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done", "item_id": item.id,
                    "output_index": item.output_index, "content_index": 0, "text": self.message_text
                }),
            ));
            f.push(frame(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done", "item_id": item.id,
                    "output_index": item.output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": self.message_text, "annotations": []}
                }),
            ));
            let item_json = json!({"type": "message", "id": item.id, "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": self.message_text, "annotations": []}]});
            f.push(frame(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": item.output_index, "item": item_json.clone()
                }),
            ));
            self.final_items.push(item_json);
        }
    }

    fn finish_tool(&mut self, f: &mut Vec<SseFrame>, t: ToolItem) {
        let item = json!({"type": "function_call", "id": t.id, "call_id": t.call_id,
            "name": t.name, "arguments": t.args, "status": "completed"});
        f.push(frame(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": t.id, "output_index": t.output_index, "arguments": t.args
            }),
        ));
        f.push(frame(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": t.output_index, "item": item.clone()
            }),
        ));
        self.final_items.push(item);
    }

    fn close_open_tools(&mut self, f: &mut Vec<SseFrame>) {
        let open: Vec<ToolItem> = std::mem::take(&mut self.tools).into_values().collect();
        for t in open {
            self.finish_tool(f, t);
        }
    }

    fn skeleton(&self, status: &str) -> Value {
        json!({"id": self.response_id, "object": "response", "status": status,
            "model": self.model, "output": []})
    }

    fn completed_response(&self) -> Value {
        let (i, o, t, c, r) = self.usage.unwrap_or((0, 0, 0, 0, 0));
        json!({"id": self.response_id, "object": "response", "status": "completed",
            "model": self.model, "output": self.final_items,
            "usage": {"input_tokens": i, "input_tokens_details": {"cached_tokens": c},
                      "output_tokens": o, "output_tokens_details": {"reasoning_tokens": r},
                      "total_tokens": t}})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(
            req.messages[2],
            Message::Tool {
                call_id: "c1".into(),
                output: "sunny".into()
            }
        );
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

    #[test]
    fn reasoning_preamble_and_tool_call_merge_into_one_assistant() {
        // A single assistant turn replayed by Codex as: reasoning, a short preamble
        // message, then the function_call. The reasoning must land on the SAME
        // assistant that carries the tool call (DeepSeek thinking mode requires it),
        // and there must be exactly one assistant message (no spurious split).
        let body = json!({
            "model": "gpt-5.5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "do it"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "I will run ls"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Let me check."}]},
                {"type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"}
            ]
        });
        let req = parse_request(&body).unwrap();
        let assistants: Vec<_> = req
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant {
                    text,
                    reasoning_content,
                    tool_calls,
                } => Some((text, reasoning_content, tool_calls)),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistants.len(),
            1,
            "reasoning + preamble + tool_call must be ONE assistant, got {}",
            assistants.len()
        );
        let (text, reasoning_content, tool_calls) = assistants[0];
        assert_eq!(
            tool_calls.len(),
            1,
            "the tool call must be on the assistant"
        );
        assert_eq!(text.as_deref(), Some("Let me check."));
        assert_eq!(
            reasoning_content.as_deref(),
            Some("I will run ls"),
            "reasoning_content must be on the tool-call assistant"
        );
    }

    #[test]
    fn reasoning_then_function_call_carries_reasoning() {
        // No preamble: reasoning directly followed by a tool call.
        let body = json!({
            "model": "gpt-5.5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "step one"}]},
                {"type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"}
            ]
        });
        let req = parse_request(&body).unwrap();
        let a = req
            .messages
            .iter()
            .find_map(|m| match m {
                Message::Assistant {
                    reasoning_content,
                    tool_calls,
                    ..
                } => Some((reasoning_content, tool_calls)),
                _ => None,
            })
            .expect("assistant");
        assert_eq!(a.1.len(), 1);
        assert_eq!(a.0.as_deref(), Some("step one"));
    }

    #[test]
    fn orphaned_output_parses_to_bare_tool() {
        // VS Code Copilot Chat sends function_call_output without repeating the
        // function_call. The parser leaves it as a bare `tool` message; the
        // assistant→tool_calls parent that strict upstreams (DeepSeek) require is
        // synthesized later by the outbound chokepoint (normalize_tool_parents in
        // wire::chat::build_request, tested there), not here.
        let body = json!({
            "model": "gpt-5.5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
            ]
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(
            req.messages,
            vec![
                Message::User("go".into()),
                Message::Tool {
                    call_id: "c1".into(),
                    output: "ok".into()
                },
                Message::User("next".into()),
            ],
            "orphaned output should parse to a bare tool, got {:?}",
            req.messages
        );
    }

    #[test]
    fn merges_consecutive_function_calls_into_one_assistant() {
        // User → function_call(c1) → function_call(c2) →
        //      function_call_output(c1) → function_call_output(c2):
        // the two consecutive function_call items must merge into a single
        // assistant carrying both tool_calls, and each output parses to a bare
        // tool result in order (no spurious extra assistant between them).
        let body = json!({
            "model": "gpt-5.5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "do two things"}]},
                {"type": "function_call", "call_id": "c1", "name": "read", "arguments": r#"{"path":"a"}"#},
                {"type": "function_call", "call_id": "c2", "name": "read", "arguments": r#"{"path":"b"}"#},
                {"type": "function_call_output", "call_id": "c1", "output": "content a"},
                {"type": "function_call_output", "call_id": "c2", "output": "content b"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
            ]
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(
            req.messages.len(),
            5,
            "expected [user, assistant(c1,c2), tool(c1), tool(c2), user], got {:?}",
            req.messages
        );
        // msg[1]: single assistant with both tool_calls (merged from the two
        // consecutive function_call items).
        match &req.messages[1] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(
                    tool_calls.len(),
                    2,
                    "both c1 and c2 must be on the same assistant"
                );
                assert_eq!(tool_calls[0].call_id, "c1");
                assert_eq!(tool_calls[1].call_id, "c2");
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        // msg[2]: tool result for c1
        assert_eq!(
            req.messages[2],
            Message::Tool {
                call_id: "c1".into(),
                output: "content a".into()
            }
        );
        // msg[3]: tool result for c2 — must NOT have a duplicate assistant inserted
        assert_eq!(
            req.messages[3],
            Message::Tool {
                call_id: "c2".into(),
                output: "content b".into()
            }
        );
        // msg[4]: user follow-up
        assert_eq!(req.messages[4], Message::User("next".into()));
    }

    use crate::canonical::CanonicalEvent::*;

    fn event_names(frames: &[SseFrame]) -> Vec<String> {
        frames.iter().map(|f| f.event.clone()).collect()
    }

    #[test]
    fn emits_message_sequence_for_text() {
        let mut e = ResponsesEmitter::new();
        let mut frames = Vec::new();
        frames.extend(e.on_event(&Created {
            response_id: "r".into(),
            model: "m".into(),
        }));
        frames.extend(e.on_event(&TextDelta { text: "Hi".into() }));
        frames.extend(e.on_event(&Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cached_input_tokens: 7,
            reasoning_tokens: 3,
        }));
        frames.extend(e.on_event(&Completed));
        let names = event_names(&frames);
        assert_eq!(names.first().unwrap(), "response.created");
        assert!(names.contains(&"response.output_item.added".to_string()));
        assert!(names.contains(&"response.content_part.added".to_string()));
        assert!(names.contains(&"response.output_text.delta".to_string()));
        assert_eq!(names.last().unwrap(), "response.completed");
        let delta = frames
            .iter()
            .find(|f| f.event == "response.output_text.delta")
            .unwrap();
        assert_eq!(delta.data["delta"], "Hi");
        let completed = frames.last().unwrap();
        assert_eq!(completed.data["response"]["usage"]["total_tokens"], 15);
        assert_eq!(
            completed.data["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            7
        );
        assert_eq!(
            completed.data["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            3
        );
        assert_eq!(
            completed.data["response"]["output"][0]["content"][0]["text"],
            "Hi"
        );
    }

    #[test]
    fn emits_function_call_sequence() {
        let mut e = ResponsesEmitter::new();
        let mut frames = Vec::new();
        frames.extend(e.on_event(&Created {
            response_id: "r".into(),
            model: "m".into(),
        }));
        frames.extend(e.on_event(&ToolCallStart {
            index: 0,
            call_id: "c1".into(),
            name: "f".into(),
        }));
        frames.extend(e.on_event(&ToolCallArgsDelta {
            index: 0,
            delta: "{}".into(),
        }));
        frames.extend(e.on_event(&ToolCallDone { index: 0 }));
        frames.extend(e.on_event(&Completed));
        let names = event_names(&frames);
        assert!(names.contains(&"response.function_call_arguments.delta".to_string()));
        assert!(names.contains(&"response.function_call_arguments.done".to_string()));
        let done = frames
            .iter()
            .find(|f| f.event == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(done.data["arguments"], "{}");
        let completed = frames.last().unwrap();
        assert_eq!(
            completed.data["response"]["output"][0]["type"],
            "function_call"
        );
        assert_eq!(completed.data["response"]["output"][0]["name"], "f");
    }

    #[test]
    fn final_response_available_after_completed() {
        let mut e = ResponsesEmitter::new();
        e.on_event(&Created {
            response_id: "r".into(),
            model: "m".into(),
        });
        e.on_event(&TextDelta { text: "ok".into() });
        e.on_event(&Completed);
        let resp = e.final_response();
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["output"][0]["content"][0]["text"], "ok");
    }

    #[test]
    fn frames_carry_increasing_sequence_numbers() {
        let mut e = ResponsesEmitter::new();
        let mut frames = Vec::new();
        frames.extend(e.on_event(&Created {
            response_id: "r".into(),
            model: "m".into(),
        }));
        frames.extend(e.on_event(&TextDelta { text: "hi".into() }));
        frames.extend(e.on_event(&Completed));
        let seqs: Vec<u64> = frames
            .iter()
            .map(|f| f.data["sequence_number"].as_u64().unwrap())
            .collect();
        assert!(!seqs.is_empty());
        for (i, s) in seqs.iter().enumerate() {
            assert_eq!(*s, i as u64, "sequence numbers must be 0,1,2,... in order");
        }
    }
}
