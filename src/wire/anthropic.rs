//! Anthropic Messages API wire format — inbound (server) side.
//!
//! Lets an Anthropic-Messages client (e.g. Claude Code via `ANTHROPIC_BASE_URL`)
//! drive the bridge: parse `/v1/messages` requests into the canonical request,
//! and serialize the canonical event stream back into Anthropic Messages SSE
//! (`message_start` / `content_block_*` / `message_delta` / `message_stop`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::canonical::*;
use crate::error::BridgeError;
use crate::wire::{CanonicalEmitter, SseFrame, frame};

// DeepSeek (and other Chat Completions upstreams) do not verify Anthropic
// thinking-block signatures, but a non-empty signature keeps clients from
// discarding the thinking block on the next turn — which is what carries the
// reasoning back to the model. This is an opaque placeholder, not a real one.
const THINKING_SIGNATURE: &str = "bridge-unsigned";

// ---------------------------------------------------------------------------
// Request: Anthropic Messages -> CanonicalRequest
// ---------------------------------------------------------------------------

pub fn parse_request(body: &Value) -> Result<CanonicalRequest, BridgeError> {
    let obj = body
        .as_object()
        .ok_or_else(|| BridgeError::BadRequest("body must be a JSON object".into()))?;

    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::BadRequest("missing `model`".into()))?
        .to_string();

    let system = parse_system(obj.get("system"));

    let mut messages = Vec::new();
    if let Some(arr) = obj.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            parse_message(role, m.get("content"), &mut messages);
        }
    }

    let tools = parse_tools(obj.get("tools"));
    let tool_choice = parse_tool_choice(obj.get("tool_choice"));
    let temperature = obj
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    let top_p = obj.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32);
    let max_output_tokens = obj
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    // `thinking: { type: "enabled" }` -> ask the upstream to think.
    let reasoning_effort = match obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
    {
        Some("enabled") => Some(ReasoningEffort::High),
        _ => None,
    };
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
        parallel_tool_calls: None,
        stream,
    })
}

fn parse_system(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let mut out = String::new();
            for b in arr {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

fn parse_message(role: &str, content: Option<&Value>, messages: &mut Vec<Message>) {
    match role {
        "user" => match content {
            Some(Value::String(s)) => messages.push(Message::User(s.clone())),
            Some(Value::Array(blocks)) => {
                let mut text = String::new();
                for b in blocks {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                text.push_str(t);
                            }
                        }
                        // A tool result closes out a tool call; emit it as a Tool message.
                        Some("tool_result") => {
                            messages.push(Message::Tool {
                                call_id: b
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                output: block_text(b.get("content")),
                            });
                        }
                        // images and other block types are deferred
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    messages.push(Message::User(text));
                }
            }
            _ => {}
        },
        "assistant" => {
            let mut text: Option<String> = None;
            let mut reasoning_content: Option<String> = None;
            let mut tool_calls = Vec::new();
            match content {
                Some(Value::String(s)) => text = Some(s.clone()),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                let t = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                text.get_or_insert_with(String::new).push_str(t);
                            }
                            Some("thinking") => {
                                let t = b.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                                if !t.is_empty() {
                                    reasoning_content = Some(t.to_string());
                                }
                            }
                            Some("tool_use") => {
                                tool_calls.push(ToolCall {
                                    call_id: b
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    name: b
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    arguments: b
                                        .get("input")
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| "{}".to_string()),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            // A turn that produced only reasoning still needs non-null content
            // when echoed to a Chat Completions upstream (content or tool_calls
            // must be set), so give it an empty string rather than null.
            let text = if text.is_none() && tool_calls.is_empty() && reasoning_content.is_some() {
                Some(String::new())
            } else {
                text
            };
            messages.push(Message::Assistant {
                text,
                reasoning_content,
                tool_calls,
            });
        }
        _ => {}
    }
}

/// Extract text from a tool_result/content value (string, or array of blocks).
fn block_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for b in blocks {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
            out
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn parse_tools(v: Option<&Value>) -> Vec<ToolDef> {
    let Some(Value::Array(arr)) = v else {
        return vec![];
    };
    arr.iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(|x| x.as_str())?.to_string();
            Some(ToolDef {
                name,
                description: t
                    .get("description")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
                parameters: t.get("input_schema").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn parse_tool_choice(v: Option<&Value>) -> ToolChoice {
    match v.and_then(|t| t.get("type")).and_then(|v| v.as_str()) {
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => v
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Function(n.to_string()))
            .unwrap_or(ToolChoice::Auto),
        _ => ToolChoice::Auto,
    }
}

// ---------------------------------------------------------------------------
// Response: CanonicalEvent stream -> Anthropic Messages SSE
// ---------------------------------------------------------------------------

struct ToolBlock {
    index: u32,
    id: String,
    name: String,
    args: String,
}

/// Stateful translator: canonical events -> Anthropic Messages SSE frames.
#[derive(Default)]
pub struct AnthropicEmitter {
    message_id: String,
    model: String,
    started: bool,
    next_index: u32,
    thinking_index: Option<u32>,
    thinking_text: String,
    text_index: Option<u32>,
    text_text: String,
    tools: BTreeMap<u32, ToolBlock>,
    any_tool: bool,
    usage: Option<(u32, u32, u32, u32)>,
    final_blocks: Vec<Value>,
}

impl CanonicalEmitter for AnthropicEmitter {
    fn emit(&mut self, ev: &CanonicalEvent) -> Vec<SseFrame> {
        self.on_event(ev)
    }
}

impl AnthropicEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_event(&mut self, ev: &CanonicalEvent) -> Vec<SseFrame> {
        let mut f = Vec::new();
        match ev {
            CanonicalEvent::Created { response_id, model } => {
                self.message_id = response_id.clone();
                self.model = model.clone();
                self.start(&mut f);
            }
            CanonicalEvent::ReasoningDelta { text } => {
                self.start(&mut f);
                self.ensure_thinking_open(&mut f);
                self.thinking_text.push_str(text);
                let index = self.thinking_index.unwrap();
                f.push(frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "thinking_delta", "thinking": text}
                    }),
                ));
            }
            CanonicalEvent::TextDelta { text } => {
                self.start(&mut f);
                self.close_thinking(&mut f);
                self.ensure_text_open(&mut f);
                self.text_text.push_str(text);
                let index = self.text_index.unwrap();
                f.push(frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
            }
            CanonicalEvent::ToolCallStart {
                index,
                call_id,
                name,
            } => {
                self.start(&mut f);
                self.close_thinking(&mut f);
                self.close_text(&mut f);
                let block_index = self.alloc_index();
                self.any_tool = true;
                f.push(frame(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": block_index,
                        "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}
                    }),
                ));
                self.tools.insert(
                    *index,
                    ToolBlock {
                        index: block_index,
                        id: call_id.clone(),
                        name: name.clone(),
                        args: String::new(),
                    },
                );
            }
            CanonicalEvent::ToolCallArgsDelta { index, delta } => {
                if let Some(tb) = self.tools.get_mut(index) {
                    tb.args.push_str(delta);
                    f.push(frame(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": tb.index,
                            "delta": {"type": "input_json_delta", "partial_json": delta}
                        }),
                    ));
                }
            }
            CanonicalEvent::ToolCallDone { index } => {
                if let Some(tb) = self.tools.remove(index) {
                    self.finish_tool(&mut f, tb);
                }
            }
            CanonicalEvent::Usage {
                input_tokens,
                output_tokens,
                total_tokens,
                cached_input_tokens,
            } => {
                self.usage = Some((
                    *input_tokens,
                    *output_tokens,
                    *total_tokens,
                    *cached_input_tokens,
                ));
            }
            CanonicalEvent::Completed => {
                self.close_thinking(&mut f);
                self.close_text(&mut f);
                self.close_open_tools(&mut f);
                let (uncached, o, cache_read) = self.anthropic_usage();
                let stop_reason = if self.any_tool {
                    "tool_use"
                } else {
                    "end_turn"
                };
                f.push(frame(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": {"input_tokens": uncached, "output_tokens": o,
                                  "cache_read_input_tokens": cache_read}
                    }),
                ));
                f.push(frame("message_stop", json!({"type": "message_stop"})));
            }
            CanonicalEvent::Error { message, status } => {
                f.push(frame(
                    "error",
                    json!({
                        "type": "error",
                        "error": {"type": "api_error", "message": message, "code": status}
                    }),
                ));
            }
        }
        f
    }

    /// The full non-streaming Messages object (valid after `Completed`).
    pub fn final_message(&self) -> Value {
        let (uncached, o, cache_read) = self.anthropic_usage();
        let stop_reason = if self.any_tool {
            "tool_use"
        } else {
            "end_turn"
        };
        json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": self.final_blocks,
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "usage": {"input_tokens": uncached, "output_tokens": o,
                      "cache_read_input_tokens": cache_read}
        })
    }

    /// `(input_tokens, output_tokens, cache_read_input_tokens)` in Anthropic
    /// terms: upstream `prompt_tokens` is cache-hit + cache-miss, so the
    /// Anthropic `input_tokens` is the non-cached remainder and the cached
    /// portion is reported separately as `cache_read_input_tokens`.
    fn anthropic_usage(&self) -> (u32, u32, u32) {
        let (input, output, _total, cached) = self.usage.unwrap_or((0, 0, 0, 0));
        (input.saturating_sub(cached), output, cached)
    }

    fn start(&mut self, f: &mut Vec<SseFrame>) {
        if self.started {
            return;
        }
        self.started = true;
        f.push(frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id, "type": "message", "role": "assistant",
                    "model": self.model, "content": [],
                    "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        ));
    }

    fn alloc_index(&mut self) -> u32 {
        let i = self.next_index;
        self.next_index += 1;
        i
    }

    fn ensure_thinking_open(&mut self, f: &mut Vec<SseFrame>) {
        if self.thinking_index.is_some() {
            return;
        }
        let index = self.alloc_index();
        f.push(frame(
            "content_block_start",
            json!({
                "type": "content_block_start", "index": index,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        ));
        self.thinking_index = Some(index);
    }

    fn close_thinking(&mut self, f: &mut Vec<SseFrame>) {
        if let Some(index) = self.thinking_index.take() {
            f.push(frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "signature_delta", "signature": THINKING_SIGNATURE}
                }),
            ));
            f.push(frame(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
            self.final_blocks.push(json!({
                "type": "thinking", "thinking": self.thinking_text, "signature": THINKING_SIGNATURE
            }));
        }
    }

    fn ensure_text_open(&mut self, f: &mut Vec<SseFrame>) {
        if self.text_index.is_some() {
            return;
        }
        let index = self.alloc_index();
        f.push(frame(
            "content_block_start",
            json!({
                "type": "content_block_start", "index": index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        self.text_index = Some(index);
    }

    fn close_text(&mut self, f: &mut Vec<SseFrame>) {
        if let Some(index) = self.text_index.take() {
            f.push(frame(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
            self.final_blocks
                .push(json!({"type": "text", "text": self.text_text}));
        }
    }

    fn finish_tool(&mut self, f: &mut Vec<SseFrame>, tb: ToolBlock) {
        let input: Value = serde_json::from_str(&tb.args).unwrap_or_else(|_| json!({}));
        f.push(frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": tb.index}),
        ));
        self.final_blocks.push(json!({
            "type": "tool_use", "id": tb.id, "name": tb.name, "input": input
        }));
    }

    fn close_open_tools(&mut self, f: &mut Vec<SseFrame>) {
        let open: Vec<ToolBlock> = std::mem::take(&mut self.tools).into_values().collect();
        for tb in open {
            self.finish_tool(f, tb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::CanonicalEvent::*;

    fn names(frames: &[SseFrame]) -> Vec<String> {
        frames.iter().map(|f| f.event.clone()).collect()
    }

    #[test]
    fn parses_system_tools_and_user_text() {
        let body = json!({
            "model": "claude-x",
            "max_tokens": 256,
            "system": "be terse",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{"name": "get_weather", "description": "w", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
            "stream": true
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.model, "claude-x");
        assert_eq!(req.system.as_deref(), Some("be terse"));
        assert_eq!(req.messages, vec![Message::User("hello".into())]);
        assert_eq!(req.max_output_tokens, Some(256));
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "get_weather");
        assert_eq!(req.tool_choice, ToolChoice::Auto);
        assert!(req.stream);
    }

    #[test]
    fn parses_assistant_thinking_tool_use_and_tool_result() {
        let body = json!({
            "model": "claude-x",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "let me check", "signature": "s"},
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "tu_1", "name": "get_weather", "input": {"city": "SF"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "sunny"}
                ]}
            ]
        });
        let req = parse_request(&body).unwrap();
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0], Message::User("weather?".into()));
        match &req.messages[1] {
            Message::Assistant {
                text,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(text.as_deref(), Some("Checking."));
                assert_eq!(reasoning_content.as_deref(), Some("let me check"));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].call_id, "tu_1");
                assert_eq!(tool_calls[0].name, "get_weather");
                assert!(tool_calls[0].arguments.contains("\"city\""));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        assert_eq!(
            req.messages[2],
            Message::Tool {
                call_id: "tu_1".into(),
                output: "sunny".into()
            }
        );
    }

    #[test]
    fn tool_choice_any_and_tool() {
        let any =
            parse_request(&json!({"model": "m", "messages": [], "tool_choice": {"type": "any"}}))
                .unwrap();
        assert_eq!(any.tool_choice, ToolChoice::Required);
        let pick = parse_request(
            &json!({"model": "m", "messages": [], "tool_choice": {"type": "tool", "name": "f"}}),
        )
        .unwrap();
        assert_eq!(pick.tool_choice, ToolChoice::Function("f".into()));
    }

    #[test]
    fn assistant_with_only_thinking_gets_empty_text() {
        let body = json!({"model": "m", "messages": [
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": "s"}]}
        ]});
        let req = parse_request(&body).unwrap();
        match &req.messages[0] {
            Message::Assistant {
                text,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(text.as_deref(), Some(""));
                assert_eq!(reasoning_content.as_deref(), Some("hmm"));
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn emits_text_message_sequence() {
        let mut e = AnthropicEmitter::new();
        let mut f = Vec::new();
        f.extend(e.on_event(&Created {
            response_id: "msg_1".into(),
            model: "m".into(),
        }));
        f.extend(e.on_event(&TextDelta { text: "Hi".into() }));
        f.extend(e.on_event(&Usage {
            input_tokens: 3,
            output_tokens: 1,
            total_tokens: 4,
            cached_input_tokens: 2,
        }));
        f.extend(e.on_event(&Completed));
        let n = names(&f);
        assert_eq!(n.first().unwrap(), "message_start");
        assert!(n.contains(&"content_block_start".to_string()));
        assert!(n.contains(&"content_block_delta".to_string()));
        assert!(n.contains(&"content_block_stop".to_string()));
        assert_eq!(n[n.len() - 2], "message_delta");
        assert_eq!(n.last().unwrap(), "message_stop");
        let delta = f
            .iter()
            .find(|fr| fr.data["delta"]["type"] == "text_delta")
            .unwrap();
        assert_eq!(delta.data["delta"]["text"], "Hi");
        let md = f.iter().find(|fr| fr.event == "message_delta").unwrap();
        assert_eq!(md.data["delta"]["stop_reason"], "end_turn");
        assert_eq!(md.data["usage"]["output_tokens"], 1);
        // Anthropic input_tokens excludes the cache-read portion (3 - 2 = 1).
        assert_eq!(md.data["usage"]["input_tokens"], 1);
        assert_eq!(md.data["usage"]["cache_read_input_tokens"], 2);
    }

    #[test]
    fn emits_tool_use_sequence() {
        let mut e = AnthropicEmitter::new();
        let mut f = Vec::new();
        f.extend(e.on_event(&Created {
            response_id: "msg_1".into(),
            model: "m".into(),
        }));
        f.extend(e.on_event(&ToolCallStart {
            index: 0,
            call_id: "tu_1".into(),
            name: "f".into(),
        }));
        f.extend(e.on_event(&ToolCallArgsDelta {
            index: 0,
            delta: "{\"a\":1}".into(),
        }));
        f.extend(e.on_event(&ToolCallDone { index: 0 }));
        f.extend(e.on_event(&Completed));
        let start = f
            .iter()
            .find(|fr| fr.data["content_block"]["type"] == "tool_use")
            .unwrap();
        assert_eq!(start.data["content_block"]["id"], "tu_1");
        assert_eq!(start.data["content_block"]["name"], "f");
        let jd = f
            .iter()
            .find(|fr| fr.data["delta"]["type"] == "input_json_delta")
            .unwrap();
        assert_eq!(jd.data["delta"]["partial_json"], "{\"a\":1}");
        let md = f.iter().find(|fr| fr.event == "message_delta").unwrap();
        assert_eq!(md.data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn final_message_assembles_blocks() {
        // Mirrors non-streaming: reasoning + text + a tool call.
        let mut e = AnthropicEmitter::new();
        for ev in [
            Created {
                response_id: "msg_1".into(),
                model: "m".into(),
            },
            ReasoningDelta {
                text: "think".into(),
            },
            TextDelta {
                text: "answer".into(),
            },
            ToolCallStart {
                index: 0,
                call_id: "tu_1".into(),
                name: "f".into(),
            },
            ToolCallArgsDelta {
                index: 0,
                delta: "{}".into(),
            },
            ToolCallDone { index: 0 },
            Usage {
                input_tokens: 5,
                output_tokens: 2,
                total_tokens: 7,
                cached_input_tokens: 2,
            },
            Completed,
        ] {
            e.on_event(&ev);
        }
        let msg = e.final_message();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"][0]["type"], "thinking");
        assert_eq!(msg["content"][0]["thinking"], "think");
        assert_eq!(msg["content"][1]["type"], "text");
        assert_eq!(msg["content"][1]["text"], "answer");
        assert_eq!(msg["content"][2]["type"], "tool_use");
        assert_eq!(msg["content"][2]["name"], "f");
        assert_eq!(msg["stop_reason"], "tool_use");
        // input_tokens (5) minus cache-read (2) = 3; cached reported separately.
        assert_eq!(msg["usage"]["input_tokens"], 3);
        assert_eq!(msg["usage"]["cache_read_input_tokens"], 2);
    }
}
