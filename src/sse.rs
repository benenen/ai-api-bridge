//! Minimal Server-Sent Events decoder for upstream byte streams.
//!
//! Operates on raw bytes and only decodes UTF-8 on complete event blocks, so a
//! multibyte character split across two network chunks is never corrupted (an
//! event boundary `\n\n` is pure ASCII and can't fall inside a multibyte scalar).

#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SseItem {
    Data(String),
    Done,
}

impl SseDecoder {
    /// Feed a chunk of bytes; returns whatever complete events are now available.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseItem> {
        self.buf.extend_from_slice(chunk);
        let mut items = Vec::new();
        while let Some((pos, sep_len)) = find_separator(&self.buf) {
            let block = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
            self.buf.drain(..pos + sep_len);
            if let Some(item) = parse_event_block(&block) {
                items.push(item);
            }
        }
        items
    }
}

/// Find the first event separator (`\n\n` or `\r\n\r\n`), returning its start
/// offset and its length.
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_event_block(block: &str) -> Option<SseItem> {
    let mut data = String::new();
    for raw_line in block.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
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
        let mut items = d.push(b"data: {\"a\":1}\n\ndata: {\"b\"");
        items.extend(d.push(b":2}\n\n"));
        assert_eq!(items, vec![
            SseItem::Data("{\"a\":1}".into()),
            SseItem::Data("{\"b\":2}".into()),
        ]);
    }

    #[test]
    fn recognizes_done_and_crlf() {
        let mut d = SseDecoder::default();
        let items = d.push(b"data: [DONE]\r\n\r\n");
        assert_eq!(items, vec![SseItem::Done]);
    }

    #[test]
    fn ignores_comment_and_event_lines() {
        let mut d = SseDecoder::default();
        let items = d.push(b": ping\nevent: foo\ndata: {\"x\":true}\n\n");
        assert_eq!(items, vec![SseItem::Data("{\"x\":true}".into())]);
    }

    #[test]
    fn reassembles_multibyte_char_split_across_chunks() {
        let mut d = SseDecoder::default();
        let full = "data: {\"t\":\"你好\"}\n\n".as_bytes();
        // byte index 13 lands in the middle of the first multibyte char (你).
        let mut items = d.push(&full[..13]);
        assert!(items.is_empty());
        items.extend(d.push(&full[13..]));
        assert_eq!(items, vec![SseItem::Data("{\"t\":\"你好\"}".into())]);
    }
}
