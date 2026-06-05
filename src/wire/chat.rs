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
        Self { response_id, ..Default::default() }
    }

    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<CanonicalEvent> {
        if let Some(err) = chunk.get("error") {
            if !err.is_null() {
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
        }
        let mut events = Vec::new();
        if !self.created {
            self.model = chunk.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
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
                {
                    if !rc.is_empty() {
                        events.push(CanonicalEvent::ReasoningDelta { text: rc.to_string() });
                    }
                }
                if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        events.push(CanonicalEvent::TextDelta { text: c.to_string() });
                    }
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        if !self.started_tools.contains(&index) {
                            let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let name = tc.get("function").and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str()).unwrap_or("").to_string();
                            self.started_tools.insert(index);
                            events.push(CanonicalEvent::ToolCallStart { index, call_id, name });
                        }
                        if let Some(args) = tc.get("function").and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                        {
                            if !args.is_empty() {
                                events.push(CanonicalEvent::ToolCallArgsDelta { index, delta: args.to_string() });
                            }
                        }
                    }
                }
            }
            if choice.get("finish_reason").and_then(|v| v.as_str()).is_some() {
                let mut idxs: Vec<u32> = self.started_tools.iter().copied().collect();
                idxs.sort_unstable();
                for i in idxs {
                    events.push(CanonicalEvent::ToolCallDone { index: i });
                }
            }
        }

        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                events.push(usage_event(u));
            }
        }
        events
    }
}

/// Translate a full (non-streaming) Chat Completions response into canonical events.
pub fn completion_to_events(resp: &Value, response_id: &str) -> Vec<CanonicalEvent> {
    let mut events = Vec::new();
    let model = resp.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    events.push(CanonicalEvent::Created { response_id: response_id.to_string(), model });

    if let Some(msg) = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
    {
        if let Some(rc) = msg.get("reasoning_content").or_else(|| msg.get("reasoning")).and_then(|v| v.as_str()) {
            if !rc.is_empty() {
                events.push(CanonicalEvent::ReasoningDelta { text: rc.to_string() });
            }
        }
        if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
            if !c.is_empty() {
                events.push(CanonicalEvent::TextDelta { text: c.to_string() });
            }
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for (i, tc) in tcs.iter().enumerate() {
                let index = i as u32;
                let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                events.push(CanonicalEvent::ToolCallStart { index, call_id, name });
                if !args.is_empty() {
                    events.push(CanonicalEvent::ToolCallArgsDelta { index, delta: args });
                }
                events.push(CanonicalEvent::ToolCallDone { index });
            }
        }
    }

    if let Some(u) = resp.get("usage") {
        if !u.is_null() {
            events.push(usage_event(u));
        }
    }
    events.push(CanonicalEvent::Completed);
    events
}

fn usage_event(u: &Value) -> CanonicalEvent {
    CanonicalEvent::Usage {
        input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        output_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
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

    #[test]
    fn stream_parser_emits_text_then_usage() {
        let mut p = ChatStreamParser::new("resp_x".into());
        let mut evs = p.on_chunk(&json!({"model":"opencode/gpt-5.5",
            "choices":[{"delta":{"content":"Hel"}}]}));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{"content":"lo"}}]})));
        evs.extend(p.on_chunk(&json!({"choices":[{"delta":{},"finish_reason":"stop"}]})));
        evs.extend(p.on_chunk(&json!({"choices":[],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})));
        use CanonicalEvent::*;
        assert_eq!(evs[0], Created { response_id: "resp_x".into(), model: "opencode/gpt-5.5".into() });
        assert_eq!(evs[1], TextDelta { text: "Hel".into() });
        assert_eq!(evs[2], TextDelta { text: "lo".into() });
        assert_eq!(evs.last().unwrap(), &Usage { input_tokens: 3, output_tokens: 2, total_tokens: 5 });
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
        assert!(evs.contains(&ToolCallStart { index: 0, call_id: "call_1".into(), name: "get".into() }));
        assert!(evs.contains(&ToolCallArgsDelta { index: 0, delta: "{\"a\"".into() }));
        assert!(evs.contains(&ToolCallArgsDelta { index: 0, delta: ":1}".into() }));
        assert!(evs.contains(&ToolCallDone { index: 0 }));
    }

    #[test]
    fn stream_parser_surfaces_inband_error() {
        let mut p = ChatStreamParser::new("r".into());
        let evs = p.on_chunk(&json!({"error":{"message":"rate limited","code":429}}));
        use CanonicalEvent::*;
        assert_eq!(evs, vec![Error { message: "rate limited".into(), status: 429 }]);
    }

    #[test]
    fn completion_to_events_full_message() {
        let resp = json!({"model":"m","choices":[{"message":{"content":"hi",
            "tool_calls":[{"id":"c","function":{"name":"f","arguments":"{}"}}]}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        let evs = completion_to_events(&resp, "r1");
        use CanonicalEvent::*;
        assert_eq!(evs.first().unwrap(), &Created { response_id: "r1".into(), model: "m".into() });
        assert!(evs.contains(&TextDelta { text: "hi".into() }));
        assert!(evs.contains(&ToolCallStart { index: 0, call_id: "c".into(), name: "f".into() }));
        assert_eq!(evs.last().unwrap(), &Completed);
    }
}
