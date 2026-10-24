//! Server-sent events: parsing, writing and stream accumulation.
//!
//! The parser is deliberately tolerant: comment lines, unknown fields, CRLF and
//! LF terminators, and multi-line data fields are all handled, matching what
//! real gateways emit. It is exposed twice: [`SseReader`] over any [`BufRead`]
//! (the same shape as Go's `SSEReader`) and [`SseParser`], which accepts
//! arbitrary chunks so the async proxy can feed a body stream without wrapping
//! it in a blocking reader.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use super::translate::ProtocolError;
use super::types::{
    AnthropicBlock, AnthropicError, AnthropicResponse, AnthropicUsage, ChatMessage, ChatResponse,
    Choice, Content, ContentPart, StreamChunk, ToolCall, Usage,
};

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One server-sent event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field; empty for OpenAI-style streams.
    pub event: String,
    /// The concatenated `data:` payload.
    pub data: Vec<u8>,
    /// The exact bytes of the event including its terminator, so the gateway
    /// can forward it verbatim without re-encoding.
    pub raw: Vec<u8>,
}

impl SseEvent {
    /// Builds an event carrying only a data payload.
    pub fn data(data: impl Into<Vec<u8>>) -> Self {
        SseEvent {
            event: String::new(),
            data: data.into(),
            raw: Vec::new(),
        }
    }

    /// Builds a named event carrying a data payload.
    pub fn named(event: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        SseEvent {
            event: event.into(),
            data: data.into(),
            raw: Vec::new(),
        }
    }

    /// Reports whether the event is the OpenAI `[DONE]` sentinel.
    pub fn is_done(&self) -> bool {
        trim_space(&self.data) == b"[DONE]"
    }
}

/// Trims the whitespace Go's `bytes.TrimSpace` removes around SSE payloads.
///
/// Go also strips exotic Unicode spaces; SSE payloads are JSON or the `[DONE]`
/// sentinel, so ASCII whitespace is the entire practical set.
fn trim_space(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end && (b[start] as char).is_ascii_whitespace() {
        start += 1;
    }
    while end > start && (b[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }
    &b[start..end]
}

/// Trims trailing `\r` and `\n` bytes (Go's `bytes.TrimRight(line, "\r\n")`).
fn trim_line_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\r' || line[end - 1] == b'\n') {
        end -= 1;
    }
    &line[..end]
}

// ---------------------------------------------------------------------------
// Incremental parser
// ---------------------------------------------------------------------------

/// The event currently being assembled.
#[derive(Debug, Default)]
struct EventBuilder {
    raw: Vec<u8>,
    data: Vec<u8>,
    event: String,
    saw_field: bool,
}

/// Splits a byte stream into SSE events, tolerating every dialect detail the Go
/// implementation tolerates.
///
/// Bytes may be pushed in arbitrary chunks: an event split across two pushes
/// produces exactly the same result as one that arrived whole.
#[derive(Debug, Default)]
pub struct SseParser {
    pending: Vec<u8>,
    eof: bool,
    cur: Option<EventBuilder>,
}

impl SseParser {
    /// Creates an empty parser.
    pub fn new() -> Self {
        SseParser::default()
    }

    /// Feeds another chunk of the stream.
    pub fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    /// Signals that no more bytes will arrive.
    pub fn finish(&mut self) {
        self.eof = true;
    }

    /// Reports whether the parser has consumed a finished stream.
    pub fn is_finished(&self) -> bool {
        self.eof && self.pending.is_empty()
    }

    /// Returns the next complete event, or `None` when more bytes are needed
    /// (or the stream ended).
    pub fn next_event(&mut self) -> Option<SseEvent> {
        loop {
            if let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=idx).collect();
                if let Some(ev) = self.consume_line(&line) {
                    return Some(ev);
                }
                continue;
            }
            if !self.eof {
                return None;
            }
            // A stream may end without a trailing newline: process the residue
            // as a final unterminated line, exactly as a bufio read returning
            // (partial, io.EOF) would.
            if !self.pending.is_empty() {
                let line: Vec<u8> = std::mem::take(&mut self.pending);
                if let Some(ev) = self.consume_line(&line) {
                    return Some(ev);
                }

                // A trailing comment block never sets a field, so nothing is
                // owed; fall through to the EOF check.
                let builder = self.cur.take().unwrap_or_default();
                if builder.saw_field {
                    return Some(SseEvent {
                        event: builder.event,
                        data: builder.data,
                        raw: builder.raw,
                    });
                }
                return None;
            }
            // EOF with the pending block finished.
            let builder = self.cur.take().unwrap_or_default();
            if builder.saw_field {
                return Some(SseEvent {
                    event: builder.event,
                    data: builder.data,
                    raw: builder.raw,
                });
            }
            return None;
        }
    }

    /// Consumes one line, returning a completed event when the line terminates
    /// a block.
    fn consume_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        let builder = self.cur.get_or_insert_with(EventBuilder::default);
        builder.raw.extend_from_slice(line);
        let trimmed = trim_line_end(line);
        if trimmed.is_empty() {
            // A blank line terminates the event.
            if !builder.saw_field {
                // Keep-alive blank line: skip it without emitting anything.
                builder.raw.clear();
                // A blank line at EOF while nothing is pending ends the stream.
                if self.eof && self.pending.is_empty() {
                    *builder = EventBuilder::default();
                }
                return None;
            }
            let done = self.cur.take().unwrap_or_default();
            return Some(SseEvent {
                event: done.event,
                data: done.data,
                raw: done.raw,
            });
        }
        if trimmed[0] == b':' {
            // Comment / heartbeat (": ping"). Forwarded verbatim in raw, but it
            // is NOT a payload field: a comment-only block must not terminate
            // the stream or emit an empty event.
            return None;
        }
        let (name, mut value) = match trimmed.iter().position(|&b| b == b':') {
            Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
            None => (trimmed, &b""[..]),
        };
        if let Some(rest) = value.strip_prefix(b" ") {
            value = rest;
        }
        match name {
            b"event" => builder.event = String::from_utf8_lossy(value).into_owned(),
            b"data" => {
                if !builder.data.is_empty() {
                    builder.data.push(b'\n');
                }
                builder.data.extend_from_slice(value);
            }
            // `id:`, `retry:` and vendor extensions are preserved in raw.
            _ => {}
        }
        builder.saw_field = true;
        None
    }
}

// ---------------------------------------------------------------------------
// Reader / writer
// ---------------------------------------------------------------------------

/// Reads server-sent events one at a time from any buffered reader.
///
/// It is the blocking counterpart of [`SseParser`] and mirrors Go's `SSEReader`
/// including its EOF behaviour: a final event without a trailing blank line is
/// still delivered.
pub struct SseReader<R: BufRead> {
    inner: R,
    parser: SseParser,
    failed: bool,
}

impl<R: BufRead> SseReader<R> {
    /// Wraps a reader.
    pub fn new(inner: R) -> Self {
        SseReader {
            inner,
            parser: SseParser::new(),
            failed: false,
        }
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Returns the next event, or `Ok(None)` at end of stream.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        if self.failed {
            return Ok(None);
        }
        loop {
            if let Some(ev) = self.parser.next_event() {
                return Ok(Some(ev));
            }
            if self.parser.is_finished() {
                return Ok(None);
            }
            let mut chunk = [0u8; 32 * 1024];
            match self.inner.read(&mut chunk) {
                Ok(0) => self.parser.finish(),
                Ok(n) => self.parser.push(&chunk[..n]),
                Err(e) => {
                    self.failed = true;
                    return Err(e);
                }
            }
        }
    }
}

impl<R: BufRead> Iterator for SseReader<R> {
    type Item = io::Result<SseEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_event() {
            Ok(Some(ev)) => Some(Ok(ev)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Serialises events and flushes after each one.
pub struct SseWriter<W: Write> {
    w: W,
}

impl<W: Write> SseWriter<W> {
    /// Wraps a writer.
    pub fn new(w: W) -> Self {
        SseWriter { w }
    }

    /// Writes one event and flushes it downstream.
    ///
    /// An event that carries its original bytes is forwarded verbatim; a
    /// synthesised event is rendered in the SSE wire form.
    pub fn write_event(&mut self, ev: &SseEvent) -> io::Result<()> {
        if !ev.raw.is_empty() {
            self.w.write_all(&ev.raw)?;
        } else {
            if !ev.event.is_empty() {
                writeln!(self.w, "event: {}", ev.event)?;
            }
            // Go's bytes.Split always yields at least one element, so empty
            // data still produces a "data: " line.
            for line in ev.data.split(|&b| b == b'\n') {
                self.w.write_all(b"data: ")?;
                self.w.write_all(line)?;
                self.w.write_all(b"\n")?;
            }
            self.w.write_all(b"\n")?;
        }
        self.flush()
    }

    /// Forwards exact bytes and flushes.
    pub fn write_raw(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.flush()
    }

    /// Flushes the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    /// Returns the inner writer.
    pub fn into_inner(self) -> W {
        self.w
    }
}

// ---------------------------------------------------------------------------
// Stream accumulation
// ---------------------------------------------------------------------------

/// Reassembles a streaming response into a single logical response, which is
/// what the audit and fingerprinting layers need.
///
/// It never mutates the bytes it observes: the proxy forwards each event as it
/// arrives and feeds a copy here.
#[derive(Debug)]
pub struct StreamAccumulator {
    /// The concatenated assistant text.
    pub text: String,
    /// The concatenated chain-of-thought, when present.
    pub reasoning: String,
    /// Maps a tool-call index to its accumulated call.
    pub tool_calls: BTreeMap<i64, ToolCall>,
    /// Preserves first-seen ordering of tool-call indices.
    pub tool_order: Vec<i64>,
    /// The usage block, usually on the final chunk.
    pub usage: Usage,
    /// Reports whether any chunk carried usage.
    pub has_usage: bool,
    /// The model reported by the upstream.
    pub model: String,
    /// The response/completion id.
    pub id: String,
    /// OpenAI's backend identity field.
    pub system_fingerprint: String,
    /// The last non-empty finish reason seen.
    pub finish_reason: String,
    /// Records that the `[DONE]` sentinel arrived. It is the only reliable
    /// signal that a chat-style stream ended deliberately, because a
    /// finish_reason can precede a further usage chunk.
    pub saw_done: bool,
    /// Counts events consumed.
    pub events: i64,
    /// Counts raw SSE bytes consumed.
    pub bytes: i64,
    /// Records a malformed chunk without aborting the stream.
    pub err: Option<ProtocolError>,
    /// `"openai"` or `"anthropic"`, selected at construction.
    provider: String,
    /// Accumulates Anthropic content blocks by index.
    blocks: BTreeMap<i64, AnthropicBlock>,
    /// Preserves first-seen block indices.
    block_order: Vec<i64>,
}

impl Default for StreamAccumulator {
    fn default() -> Self {
        StreamAccumulator::new("openai")
    }
}

impl StreamAccumulator {
    /// Creates an accumulator for the given provider ("openai" or "anthropic").
    pub fn new(provider: &str) -> Self {
        let provider = if provider.is_empty() {
            "openai".to_string()
        } else {
            provider.to_string()
        };
        StreamAccumulator {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: BTreeMap::new(),
            tool_order: Vec::new(),
            usage: Usage::default(),
            has_usage: false,
            model: String::new(),
            id: String::new(),
            system_fingerprint: String::new(),
            finish_reason: String::new(),
            saw_done: false,
            events: 0,
            bytes: 0,
            err: None,
            provider,
            blocks: BTreeMap::new(),
            block_order: Vec::new(),
        }
    }

    /// Returns the protocol this accumulator was created for.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Feeds one event into the accumulator.
    pub fn observe(&mut self, ev: &SseEvent) {
        self.events += 1;
        self.bytes += ev.raw.len() as i64;
        let data = trim_space(&ev.data);
        if data == b"[DONE]" {
            self.saw_done = true;
            return;
        }
        if data.is_empty() {
            return;
        }
        if self.provider == "anthropic" || !ev.event.is_empty() {
            self.observe_anthropic(ev, data);
            return;
        }
        self.observe_openai(data);
    }

    fn observe_openai(&mut self, data: &[u8]) {
        let ch: StreamChunk = match serde_json::from_slice(data) {
            Ok(ch) => ch,
            Err(e) => {
                if self.err.is_none() {
                    self.err = Some(ProtocolError::Message(format!("stream: chunk parse: {e}")));
                }
                return;
            }
        };
        if !ch.id.is_empty() {
            self.id = ch.id;
        }
        if !ch.model.is_empty() {
            self.model = ch.model;
        }
        if !ch.system_fingerprint.is_empty() {
            self.system_fingerprint = ch.system_fingerprint;
        }
        if let Some(usage) = ch.usage {
            self.usage = usage;
            self.has_usage = true;
        }
        for c in ch.choices {
            let text = c.delta.content.plain();
            if !text.is_empty() {
                self.text.push_str(&text);
            }
            if !c.delta.reasoning_content.is_empty() {
                self.reasoning.push_str(&c.delta.reasoning_content);
            }
            if let Some(reason) = &c.finish_reason {
                if !reason.is_empty() {
                    self.finish_reason = reason.clone();
                }
            }
            for (i, tc) in c.delta.tool_calls.into_iter().enumerate() {
                let idx = tc.index.unwrap_or(i as i64);
                let acc = self.tool_calls.entry(idx).or_insert_with(|| {
                    self.tool_order.push(idx);
                    ToolCall {
                        index: Some(idx),
                        ..ToolCall::default()
                    }
                });
                if !tc.id.is_empty() {
                    acc.id = tc.id;
                }
                if !tc.r#type.is_empty() {
                    acc.r#type = tc.r#type;
                }
                if !tc.function.name.is_empty() {
                    acc.function.name.push_str(&tc.function.name);
                }
                if !tc.function.arguments.is_empty() {
                    acc.function.arguments.push_str(&tc.function.arguments);
                }
            }
        }
    }

    fn observe_anthropic(&mut self, ev: &SseEvent, data: &[u8]) {
        let se: AnthropicStreamEvent = match serde_json::from_slice(data) {
            Ok(se) => se,
            Err(e) => {
                if self.err.is_none() {
                    self.err = Some(ProtocolError::Message(format!(
                        "stream: anthropic chunk parse: {e}"
                    )));
                }
                return;
            }
        };
        let _ = ev;
        match se.r#type.as_str() {
            "message_start" => {
                if let Some(raw) = &se.message {
                    if let Ok(msg) = raw.parse::<AnthropicResponse>() {
                        self.id = msg.id;
                        self.model = msg.model;
                        if let Some(usage) = &msg.usage {
                            self.usage = usage.to_openai();
                            self.has_usage = true;
                        }
                    }
                }
            }
            "content_block_start" => {
                if let Some(raw) = &se.content_block {
                    if let Ok(blk) = raw.parse::<AnthropicBlock>() {
                        self.blocks.insert(se.index, blk);
                        self.block_order.push(se.index);
                    }
                }
            }
            "content_block_delta" => {
                let Some(raw) = &se.delta else { return };
                let Ok(d) = raw.parse::<ContentBlockDelta>() else {
                    return;
                };
                match d.r#type.as_str() {
                    "text_delta" => self.text.push_str(&d.text),
                    "thinking_delta" => self.reasoning.push_str(&d.thinking),
                    "input_json_delta" => {
                        let entry = self.blocks.entry(se.index).or_insert_with(|| {
                            self.block_order.push(se.index);
                            AnthropicBlock {
                                r#type: "tool_use".to_string(),
                                ..AnthropicBlock::default()
                            }
                        });
                        let mut input = entry
                            .input
                            .as_ref()
                            .map(|raw| raw.as_str().to_string())
                            .unwrap_or_default();
                        input.push_str(&d.partial_json);
                        entry.input = Some(super::types::RawJson::new(input));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(raw) = &se.delta {
                    if let Ok(d) = raw.parse::<MessageDelta>() {
                        if !d.stop_reason.is_empty() {
                            self.finish_reason = d.stop_reason;
                        }
                    }
                }
                if let Some(usage) = &se.usage {
                    let out = usage.output_tokens;
                    self.usage.completion_tokens = out;
                    self.usage.total_tokens = self.usage.prompt_tokens + out;
                    self.has_usage = true;
                }
            }
            _ => {}
        }
    }

    /// Returns accumulated tool calls ordered by index.
    pub fn tool_calls_in_order(&self) -> Vec<ToolCall> {
        self.tool_order
            .iter()
            .filter_map(|idx| self.tool_calls.get(idx).cloned())
            .collect()
    }

    /// Returns accumulated content blocks ordered by index.
    pub fn anthropic_blocks_in_order(&self) -> Vec<AnthropicBlock> {
        self.block_order
            .iter()
            .filter_map(|idx| self.blocks.get(idx).cloned())
            .collect()
    }

    /// Renders the accumulated stream as a non-streaming response so that
    /// downstream audit rules can run unchanged.
    pub fn as_response(&self) -> ChatResponse {
        let mut msg = ChatMessage {
            role: "assistant".to_string(),
            content: Content::from_parts(vec![ContentPart {
                r#type: "text".to_string(),
                text: self.text.clone(),
                ..ContentPart::default()
            }]),
            reasoning_content: self.reasoning.clone(),
            ..ChatMessage::default()
        };
        if !self.tool_calls.is_empty() {
            msg.tool_calls = self.tool_calls_in_order();
        }
        let mut resp = ChatResponse {
            id: self.id.clone(),
            object: "chat.completion".to_string(),
            model: self.model.clone(),
            system_fingerprint: self.system_fingerprint.clone(),
            choices: vec![Choice {
                index: 0,
                message: msg,
                finish_reason: self.finish_reason.clone(),
                logprobs: None,
            }],
            ..ChatResponse::default()
        };
        if self.has_usage {
            resp.usage = Some(self.usage.clone());
        }
        resp
    }

    /// Renders the accumulated stream as a Messages response.
    pub fn as_anthropic_response(&self) -> AnthropicResponse {
        let mut content = self.anthropic_blocks_in_order();
        let mut resp = AnthropicResponse {
            id: self.id.clone(),
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            model: self.model.clone(),
            content: Vec::new(),
            stop_reason: self.finish_reason.clone(),
            ..AnthropicResponse::default()
        };
        if !self.text.is_empty() {
            content.insert(
                0,
                AnthropicBlock {
                    r#type: "text".to_string(),
                    text: self.text.clone(),
                    ..AnthropicBlock::default()
                },
            );
        }
        resp.content = content;
        if self.has_usage {
            resp.usage = Some(AnthropicUsage {
                input_tokens: self.usage.prompt_tokens,
                output_tokens: self.usage.completion_tokens,
                ..AnthropicUsage::default()
            });
        }
        resp
    }
}

/// The Messages API streaming envelope.
///
/// `error` is parsed for parity with Go, which also models it without ever
/// reading it in the accumulator.
#[allow(dead_code)]
#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicStreamEvent {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    index: i64,
    #[serde(default)]
    message: Option<super::types::RawJson>,
    #[serde(default)]
    content_block: Option<super::types::RawJson>,
    #[serde(default)]
    delta: Option<super::types::RawJson>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    error: Option<AnthropicError>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ContentBlockDelta {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thinking: String,
    #[serde(default, rename = "partial_json")]
    partial_json: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct MessageDelta {
    #[serde(default)]
    stop_reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_all(stream: &str) -> Vec<SseEvent> {
        let mut r = SseReader::new(Cursor::new(stream.as_bytes().to_vec()));
        let mut out = Vec::new();
        while let Some(ev) = r.next_event().expect("read") {
            out.push(ev);
        }
        out
    }

    /// Translated from `TestSSEReaderBasic`.
    #[test]
    fn test_sse_reader_basic() {
        let stream = "data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n";
        let got: Vec<String> = read_all(stream)
            .into_iter()
            .map(|ev| String::from_utf8(ev.data).unwrap())
            .collect();
        assert_eq!(got, vec![r#"{"a":1}"#, r#"{"b":2}"#, "[DONE]"]);
    }

    /// Translated from `TestSSEReaderHandlesCRLFAndComments`.
    #[test]
    fn test_sse_reader_handles_crlf_and_comments() {
        let stream = ": ping\r\n\r\ndata: hello\r\n\r\ndata: [DONE]\r\n\r\n";
        let got: Vec<String> = read_all(stream)
            .into_iter()
            .map(|ev| String::from_utf8(ev.data).unwrap())
            .collect();
        assert_eq!(got, vec!["hello", "[DONE]"]);
    }

    /// Translated from `TestSSEReaderMultiLineData`.
    #[test]
    fn test_sse_reader_multi_line_data() {
        let events = read_all("data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, b"line1\nline2");
    }

    /// Translated from `TestSSEReaderPreservesRawBytes`.
    #[test]
    fn test_sse_reader_preserves_raw_bytes() {
        let stream = "data: {\"x\":1}\n\n";
        let events = read_all(stream);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].raw, stream.as_bytes());
    }

    /// Translated from `TestSSEReaderEOFWithoutTrailingBlank`.
    #[test]
    fn test_sse_reader_eof_without_trailing_blank() {
        let events = read_all("data: {\"x\":1}");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, br#"{"x":1}"#);
    }

    // -----------------------------------------------------------------
    // Extra coverage: chunk boundaries, keep-alives, field handling.
    // -----------------------------------------------------------------

    #[test]
    fn test_parser_is_indifferent_to_chunk_boundaries() {
        let stream = "event: message_start\r\ndata: {\"type\":\"message_start\"}\r\n\r\ndata: [DONE]\r\n\r\n";
        let whole = read_all(stream);
        for split in 1..stream.len() {
            let mut parser = SseParser::new();
            parser.push(&stream.as_bytes()[..split]);
            parser.push(&stream.as_bytes()[split..]);
            parser.finish();
            let mut chunked = Vec::new();
            while let Some(ev) = parser.next_event() {
                chunked.push(ev);
            }
            assert_eq!(chunked, whole, "split at byte {split}");
        }
    }

    #[test]
    fn test_parser_handles_one_byte_at_a_time() {
        let stream = "data: a\ndata: b\n\nevent: x\ndata: c\n\n";
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for byte in stream.as_bytes() {
            parser.push(&[*byte]);
            if let Some(ev) = parser.next_event() {
                out.push(ev);
            }
        }
        parser.finish();
        while let Some(ev) = parser.next_event() {
            out.push(ev);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, b"a\nb");
        assert_eq!(out[0].event, "");
        assert_eq!(out[1].data, b"c");
        assert_eq!(out[1].event, "x");
    }

    #[test]
    fn test_comment_only_blocks_never_emit_events() {
        let events = read_all(": keep-alive\n\n: another\n\n");
        assert!(events.is_empty());
        // A comment before a payload stays in that payload's raw bytes.
        let events = read_all(": ping\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].raw, b": ping\ndata: x\n\n");
        assert_eq!(events[0].data, b"x");
    }

    #[test]
    fn test_unknown_and_empty_fields_are_preserved_in_raw() {
        let stream = "id: 42\nretry: 3000\ndata: x\n\n";
        let events = read_all(stream);
        assert_eq!(events[0].raw, stream.as_bytes());
        assert_eq!(events[0].data, b"x");

        // A field line without a colon still counts as a field, like Go.
        let events = read_all("ping\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, b"");
        assert_eq!(events[0].raw, b"ping\n\n");
    }

    #[test]
    fn test_event_field_with_no_data_is_reported() {
        let events = read_all("event: ping\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "ping");
        assert!(events[0].data.is_empty());
    }

    #[test]
    fn test_parser_eof_with_newline_but_no_blank_line() {
        // A stream that ends right after the newline still owes its event.
        let events = read_all("data: x\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, b"x");
        assert_eq!(events[0].raw, b"data: x\n");
    }

    #[test]
    fn test_parser_carries_multibyte_utf8_across_chunks() {
        let stream = "data: 你好\n\n";
        let mut parser = SseParser::new();
        // Split inside the first multi-byte character.
        parser.push(&stream.as_bytes()[..7]);
        assert!(parser.next_event().is_none());
        parser.push(&stream.as_bytes()[7..]);
        let ev = parser.next_event().expect("event after the split");
        assert_eq!(String::from_utf8(ev.data).unwrap(), "你好");
        assert!(parser.next_event().is_none());
    }

    #[test]
    fn test_is_done_accepts_surrounding_whitespace() {
        assert!(SseEvent::data("[DONE]").is_done());
        assert!(SseEvent::data(" [DONE]\n").is_done());
        assert!(SseEvent::data("[DONE] ").is_done());
        assert!(!SseEvent::data("{}").is_done());
    }

    #[test]
    fn test_writer_round_trips_through_the_reader() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut w = SseWriter::new(&mut sink);
            w.write_event(&SseEvent::named("message_start", r#"{"a":1}"#))
                .unwrap();
            w.write_event(&SseEvent::data("[DONE]")).unwrap();
        }
        let mut r = SseReader::new(Cursor::new(sink));
        let first = r.next_event().unwrap().unwrap();
        assert_eq!(first.event, "message_start");
        assert_eq!(first.data, br#"{"a":1}"#);
        assert_eq!(first.raw, b"event: message_start\ndata: {\"a\":1}\n\n");
        let second = r.next_event().unwrap().unwrap();
        assert!(second.is_done());
        assert!(r.next_event().unwrap().is_none());
    }

    #[test]
    fn test_writer_forwards_raw_bytes_verbatim() {
        let mut sink: Vec<u8> = Vec::new();
        let mut w = SseWriter::new(&mut sink);
        let ev = SseEvent {
            event: String::new(),
            data: b"ignored".to_vec(),
            raw: b"data:   spaced\r\n\r\n".to_vec(),
        };
        w.write_event(&ev).unwrap();
        assert_eq!(sink, b"data:   spaced\r\n\r\n");
    }

    #[test]
    fn test_writer_emits_a_data_line_for_empty_payloads() {
        let mut sink: Vec<u8> = Vec::new();
        let mut w = SseWriter::new(&mut sink);
        w.write_event(&SseEvent::data("")).unwrap();
        assert_eq!(sink, b"data: \n\n");
    }

    #[test]
    fn test_writer_splits_multi_line_payloads() {
        let mut sink: Vec<u8> = Vec::new();
        let mut w = SseWriter::new(&mut sink);
        w.write_event(&SseEvent::data("a\nb")).unwrap();
        assert_eq!(sink, b"data: a\ndata: b\n\n");
    }

    // -----------------------------------------------------------------
    // Accumulator
    // -----------------------------------------------------------------

    /// Translated from `TestStreamAccumulatorOpenAI`.
    #[test]
    fn test_stream_accumulator_openai() {
        let chunks = [
            r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
            r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"lo "}}]}"#,
            r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"world"},"finish_reason":"stop"}]}"#,
            r#"{"id":"c1","model":"gpt-4o","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
        ];
        let mut acc = StreamAccumulator::new("openai");
        for c in chunks {
            acc.observe(&SseEvent::data(c));
        }
        assert_eq!(acc.text, "Hello world");
        assert_eq!(acc.model, "gpt-4o");
        assert!(acc.has_usage);
        assert_eq!(acc.usage.completion_tokens, 3);
        assert_eq!(acc.finish_reason, "stop");
        assert_eq!(
            acc.as_response().choices[0].message.content.plain(),
            "Hello world"
        );
    }

    /// Translated from `TestStreamAccumulatorToolCalls`.
    #[test]
    fn test_stream_accumulator_tool_calls() {
        let chunks = [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_","arguments":"{\"ci"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"weather","arguments":"ty\":\"SH\"}"}}]}}]}"#,
        ];
        let mut acc = StreamAccumulator::new("openai");
        for c in chunks {
            acc.observe(&SseEvent::data(c));
        }
        let tcs = acc.tool_calls_in_order();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "get_weather");
        assert_eq!(tcs[0].function.arguments, r#"{"city":"SH"}"#);
        assert_eq!(tcs[0].id, "call_1");
        assert_eq!(tcs[0].r#type, "function");
    }

    /// Translated from `TestStreamAccumulatorAnthropic`.
    #[test]
    fn test_stream_accumulator_anthropic() {
        let events = [
            SseEvent::named(
                "message_start",
                r#"{"type":"message_start","message":{"id":"m1","model":"claude-3-5-sonnet","usage":{"input_tokens":12,"output_tokens":0}}}"#,
            ),
            SseEvent::named(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            SseEvent::named(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi "}}"#,
            ),
            SseEvent::named(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}"#,
            ),
            SseEvent::named(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            ),
        ];
        let mut acc = StreamAccumulator::new("anthropic");
        for ev in &events {
            acc.observe(ev);
        }
        assert_eq!(acc.text, "Hi there");
        assert_eq!(acc.model, "claude-3-5-sonnet");
        assert_eq!(acc.usage.prompt_tokens, 12);
        assert_eq!(acc.usage.completion_tokens, 2);
        assert_eq!(acc.usage.total_tokens, 14);
        assert_eq!(acc.finish_reason, "end_turn");
        assert_eq!(acc.provider(), "anthropic");
    }

    #[test]
    fn test_accumulator_counts_events_bytes_and_done() {
        let mut acc = StreamAccumulator::new("");
        assert_eq!(acc.provider(), "openai");
        acc.observe(&SseEvent {
            event: String::new(),
            data: b"[DONE]".to_vec(),
            raw: b"data: [DONE]\n\n".to_vec(),
        });
        assert!(acc.saw_done);
        assert_eq!(acc.events, 1);
        assert_eq!(acc.bytes, 14);
    }

    #[test]
    fn test_accumulator_records_malformed_chunks_without_failing() {
        let mut acc = StreamAccumulator::new("openai");
        acc.observe(&SseEvent::data("{not json"));
        let first = acc.err.as_ref().expect("parse error recorded").to_string();
        assert!(first.starts_with("stream: chunk parse:"), "{first}");
        // A second bad chunk must not overwrite the first error.
        acc.observe(&SseEvent::data("{bad again"));
        assert_eq!(acc.err.as_ref().unwrap().to_string(), first);
    }

    #[test]
    fn test_accumulator_accumulates_anthropic_tool_input_deltas() {
        let events = [
            SseEvent::named(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}"#,
            ),
            SseEvent::named(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            ),
            SseEvent::named(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
            ),
        ];
        let mut acc = StreamAccumulator::new("anthropic");
        for ev in &events {
            acc.observe(ev);
        }
        let blocks = acc.anthropic_blocks_in_order();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].r#type, "tool_use");
        assert_eq!(blocks[0].name, "f");
        // Go appends the deltas onto the input the start block carried, so a
        // start block that already holds "{}" ends up with "{}" + the deltas.
        // The gateway only reads this field for auditing, never as JSON.
        assert_eq!(blocks[0].input.as_ref().unwrap().as_str(), r#"{}{"a":1}"#);
        let resp = acc.as_anthropic_response();
        assert_eq!(resp.r#type, "message");
        assert_eq!(resp.role, "assistant");
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.stop_reason, "");
    }

    #[test]
    fn test_as_anthropic_response_prepends_text_before_tool_blocks() {
        let mut acc = StreamAccumulator::new("anthropic");
        acc.observe(&SseEvent::named(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"f"}}"#,
        ));
        acc.observe(&SseEvent::named(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        ));
        let resp = acc.as_anthropic_response();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.content[0].r#type, "text");
        assert_eq!(resp.content[0].text, "hello");
        assert_eq!(resp.content[1].r#type, "tool_use");
    }

    #[test]
    fn test_as_response_keeps_reasoning_and_fingerprint() {
        let mut acc = StreamAccumulator::new("openai");
        acc.observe(&SseEvent::data(
            r#"{"choices":[{"index":0,"delta":{"content":"x","reasoning_content":"why"},"finish_reason":"length"}],"system_fingerprint":"fp_1"}"#,
        ));
        let resp = acc.as_response();
        assert_eq!(resp.choices[0].message.reasoning_content, "why");
        assert_eq!(resp.choices[0].finish_reason, "length");
        assert_eq!(resp.system_fingerprint, "fp_1");
        assert_eq!(resp.object, "chat.completion");
        assert!(resp.usage.is_none());
    }
}
