pub mod anthropic;
pub mod chat;
pub mod responses;

use serde_json::Value;

use crate::canonical::CanonicalEvent;

/// One SSE frame: an event name + a JSON payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub event: String,
    pub data: Value,
}

pub(crate) fn frame(event: &str, data: Value) -> SseFrame {
    SseFrame {
        event: event.to_string(),
        data,
    }
}

pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// A wire-format emitter: turns the canonical event stream into the SSE frames
/// of a specific client dialect (Responses, Anthropic Messages, ...).
pub trait CanonicalEmitter {
    fn emit(&mut self, ev: &CanonicalEvent) -> Vec<SseFrame>;
}
