//! Minimal Server-Sent Events decoder for upstream byte streams.

#[derive(Default)]
pub struct SseDecoder {
    buf: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SseItem {
    Data(String),
    Done,
}

impl SseDecoder {
    /// Feed a chunk of text; returns whatever complete events are now available.
    pub fn push(&mut self, chunk: &str) -> Vec<SseItem> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut items = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            if let Some(item) = parse_event_block(&block) {
                items.push(item);
            }
        }
        items
    }
}

fn parse_event_block(block: &str) -> Option<SseItem> {
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
        // event:, id:, and ":" comment lines are ignored
    }
    if data.is_empty() {
        return None;
    }
    if data.trim() == "[DONE]" {
        return Some(SseItem::Done);
    }
    Some(SseItem::Data(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut d = SseDecoder::default();
        let mut items = d.push("data: {\"a\":1}\n\ndata: {\"b\"");
        items.extend(d.push(":2}\n\n"));
        assert_eq!(items, vec![
            SseItem::Data("{\"a\":1}".into()),
            SseItem::Data("{\"b\":2}".into()),
        ]);
    }

    #[test]
    fn recognizes_done_and_crlf() {
        let mut d = SseDecoder::default();
        let items = d.push("data: [DONE]\r\n\r\n");
        assert_eq!(items, vec![SseItem::Done]);
    }

    #[test]
    fn ignores_comment_and_event_lines() {
        let mut d = SseDecoder::default();
        let items = d.push(": ping\nevent: foo\ndata: {\"x\":true}\n\n");
        assert_eq!(items, vec![SseItem::Data("{\"x\":true}".into())]);
    }
}
