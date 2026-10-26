//! Server-sent events, as both transports carry them: lines of `event:` and
//! `data:`, an event ending at a blank line, `data:` lines joined by newlines,
//! comments (`:`) ignored.

/// One event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    /// Its `event:` name; `message` when none was given.
    pub event: String,
    /// Its `data:` lines, joined.
    pub data: String,
}

/// Reads events out of a byte stream fed to it in arbitrary pieces.
#[derive(Debug, Default)]
pub(crate) struct Parser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl Parser {
    /// Feed the next bytes, returning the events they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() || self.event.is_some() {
                    events.push(Event {
                        event: self.event.take().unwrap_or_else(|| "message".to_owned()),
                        data: std::mem::take(&mut self.data).join("\n"),
                    });
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        events
    }

    /// The event left unfinished when the stream ended, if it has data.
    pub fn finish(&mut self) -> Option<Event> {
        let mut events = self.feed(b"\n\n");
        events.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_survive_being_split_anywhere() {
        let body =
            b": ping\r\nevent: endpoint\r\ndata: /messages?s=1\r\n\r\ndata: {\"a\":\ndata: 1}\n\n";
        for split in 1..body.len() {
            let mut parser = Parser::default();
            let mut events = parser.feed(&body[..split]);
            events.extend(parser.feed(&body[split..]));
            assert_eq!(
                events,
                vec![
                    Event {
                        event: "endpoint".into(),
                        data: "/messages?s=1".into()
                    },
                    Event {
                        event: "message".into(),
                        data: "{\"a\":\n1}".into()
                    },
                ],
                "split at {split}"
            );
        }
        let mut parser = Parser::default();
        assert!(parser.feed(b"data: tail").is_empty());
        assert_eq!(parser.finish().unwrap().data, "tail");
    }
}
