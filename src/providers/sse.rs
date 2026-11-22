//! HTTP SSE framing. Network chunks need not align with lines or UTF-8 scalars.

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    skip_lf: bool,
    seen_line: bool,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, &'static str> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    if let Some(event) = self.finish_line()? {
                        events.push(event);
                    }
                    self.skip_lf = byte == b'\r';
                }
                byte => {
                    self.line.push(byte);
                    if self.line.len() + self.data.len() > MAX_FRAME_BYTES {
                        return Err("SSE frame exceeds the 16 MiB limit");
                    }
                }
            }
        }
        Ok(events)
    }

    fn finish_line(&mut self) -> Result<Option<String>, &'static str> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes).map_err(|_| "Invalid UTF-8 in SSE frame")?;
        // SSE permits one BOM at the stream start; line buffering also covers
        // arbitrary splits inside its three UTF-8 bytes.
        let line = if self.seen_line {
            line
        } else {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        };
        self.seen_line = true;
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                return Ok(Some(std::mem::take(&mut self.data)));
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            self.has_data = true;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::Decoder;

    #[test]
    fn framing_handles_every_chunk_boundary_and_line_ending() {
        for ending in ["\n", "\r\n", "\r"] {
            let stream =
                format!(": heartbeat{ending}data: {{\"text\":\"工具🙂\"}}{ending}{ending}");
            let mut decoder = Decoder::default();
            let mut events = Vec::new();
            for byte in stream.bytes() {
                events.extend(decoder.push(&[byte]).unwrap());
            }
            assert_eq!(events, ["{\"text\":\"工具🙂\"}"]);
        }
    }

    #[test]
    fn one_leading_bom_is_ignored_across_every_chunk_boundary() {
        let stream = "\u{feff}data: first\n\ndata: \u{feff}inside\n\n";
        for split in 0..=stream.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.push(&stream.as_bytes()[..split]).unwrap();
            events.extend(decoder.push(&stream.as_bytes()[split..]).unwrap());
            assert_eq!(events, ["first", "\u{feff}inside"]);
        }
    }

    #[test]
    fn multiline_data_and_incomplete_events() {
        let mut decoder = Decoder::default();
        assert!(decoder.push(b"data: one\ndata: two\n").unwrap().is_empty());
        assert_eq!(decoder.push(b"\n").unwrap(), ["one\ntwo"]);
        assert!(decoder.push(b"data: partial").unwrap().is_empty());
    }
}
