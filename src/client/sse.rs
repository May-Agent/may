//! Incremental parser of `text/event-stream` frames.

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEvent {
    /// The `event:` name; `message` when the frame names none.
    pub kind: String,
    /// The `data:` lines joined by newlines.
    pub data: String,
    /// The `id:` field, when the frame has one.
    pub id: Option<String>,
}

/// Buffers stream bytes and yields complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    /// Appends received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete event, if the buffer holds one. Frames without
    /// data (comments, keep-alives) are skipped.
    pub fn next_event(&mut self) -> Option<ServerEvent> {
        loop {
            let end = find_frame_end(&self.buffer)?;
            let frame: Vec<u8> = self.buffer.drain(..end.frame).collect();
            self.buffer.drain(..end.separator);
            let text = String::from_utf8_lossy(&frame);
            let mut kind = None;
            let mut id = None;
            let mut data: Vec<&str> = Vec::new();
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    kind = Some(value.trim_start().to_string());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value));
                } else if let Some(value) = line.strip_prefix("id:") {
                    id = Some(value.trim_start().to_string());
                }
            }
            if !data.is_empty() || kind.is_some() {
                return Some(ServerEvent {
                    kind: kind.unwrap_or_else(|| "message".to_string()),
                    data: data.join("\n"),
                    id,
                });
            }
        }
    }
}

struct FrameEnd {
    frame: usize,
    separator: usize,
}

fn find_frame_end(buffer: &[u8]) -> Option<FrameEnd> {
    let mut index = 0;
    while index + 1 < buffer.len() {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some(FrameEnd {
                frame: index,
                separator: 2,
            });
        }
        if index + 3 < buffer.len() && &buffer[index..index + 4] == b"\r\n\r\n" {
            return Some(FrameEnd {
                frame: index,
                separator: 4,
            });
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{ServerEvent, SseParser};

    #[test]
    fn events_split_across_chunks_are_reassembled() {
        let mut parser = SseParser::default();
        parser.push(b"data:{\"a\":1}\nev");
        assert_eq!(parser.next_event(), None);
        parser.push(b"ent:session_turn\n\n:keepalive\n\ndata: x\n\n");
        assert_eq!(
            parser.next_event(),
            Some(ServerEvent {
                kind: "session_turn".to_string(),
                data: "{\"a\":1}".to_string(),
                id: None,
            })
        );
        assert_eq!(
            parser.next_event(),
            Some(ServerEvent {
                kind: "message".to_string(),
                data: "x".to_string(),
                id: None,
            })
        );
        assert_eq!(parser.next_event(), None);
    }

    #[test]
    fn a_frame_keeps_its_id() {
        let mut parser = SseParser::default();
        parser.push(b"event: session_agent\nid: 17\ndata: {}\n\n");
        assert_eq!(
            parser.next_event().and_then(|event| event.id).as_deref(),
            Some("17")
        );
    }
}
