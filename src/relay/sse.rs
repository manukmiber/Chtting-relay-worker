//! Server-sent-event plumbing for OpenAI-style streams.

use bytes::Bytes;

/// Incremental SSE parser: feed decoded text, get back complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, text: &str) -> Vec<SseEvent> {
        self.buf.push_str(text);
        let mut events = Vec::new();
        while let Some((start, end)) = find_separator(&self.buf) {
            let raw: String = self.buf[..start].to_string();
            self.buf.drain(..end);
            if let Some(parsed) = parse_event(&raw) {
                events.push(parsed);
            }
        }
        events
    }

    /// Anything left when the upstream closed without a trailing blank line.
    pub fn flush(&mut self) -> Vec<SseEvent> {
        let rest = std::mem::take(&mut self.buf);
        if rest.trim().is_empty() {
            return Vec::new();
        }
        parse_event(&rest).into_iter().collect()
    }
}

fn find_separator(buf: &str) -> Option<(usize, usize)> {
    let a = buf.find("\n\n");
    let b = buf.find("\r\n\r\n");
    match (a, b) {
        (None, None) => None,
        // Tolerate \r\n from proxies; take whichever separator comes first.
        (Some(a), Some(b)) if b < a => Some((b, b + 4)),
        (None, Some(b)) => Some((b, b + 4)),
        (Some(a), _) => Some((a, a + 2)),
    }
}

fn parse_event(raw: &str) -> Option<SseEvent> {
    let mut event = "message".to_string();
    let mut data: Vec<&str> = Vec::new();
    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(idx) => (
                &line[..idx],
                line[idx + 1..]
                    .strip_prefix(' ')
                    .unwrap_or(&line[idx + 1..]),
            ),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() && event == "message" {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

/// Frame a JSON payload as one SSE event.
pub fn format_sse(data: &serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {data}\n\n"))
}

pub fn format_sse_raw(data: &str) -> Bytes {
    Bytes::from(format!("data: {data}\n\n"))
}

pub const DONE: &str = "data: [DONE]\n\n";

/// Headers every streamed response carries.
pub const SSE_HEADERS: [(&str, &str); 4] = [
    ("content-type", "text/event-stream; charset=utf-8"),
    ("cache-control", "no-cache, no-transform"),
    ("connection", "keep-alive"),
    // Stops nginx and cloudflared from buffering the stream.
    ("x-accel-buffering", "no"),
];

/* -------------------------------------------------------- rewriting -- */

use crate::relay::transform::CompiledRules;

/// Applies text rewrites to a stream without letting a pattern that straddles
/// two chunks slip through.
///
/// Holding back a fixed tail is not enough on its own: a match can begin inside
/// the part about to be emitted and end inside the tail. So the cut point is
/// also pulled back behind any match that straddles it, leaving that text
/// buffered until the rest of it arrives.
pub struct StreamRewriter {
    rules: Option<CompiledRules>,
    lookbehind: usize,
    pending: String,
}

impl StreamRewriter {
    pub fn new(rules: Option<CompiledRules>, lookbehind: usize) -> Self {
        Self {
            rules,
            lookbehind: lookbehind.max(1),
            pending: String::new(),
        }
    }

    pub fn push(&mut self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let Some(rules) = &self.rules else {
            return text.to_string();
        };
        self.pending.push_str(text);
        let cut = safe_cut(&self.pending, rules, self.lookbehind);
        if cut == 0 {
            return String::new();
        }
        let safe: String = self.pending[..cut].to_string();
        self.pending.drain(..cut);
        rules.apply(&safe)
    }

    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let rest = std::mem::take(&mut self.pending);
        match &self.rules {
            Some(rules) => rules.apply(&rest),
            None => rest,
        }
    }
}

/// Largest prefix that cannot contain the start of a still-open match.
fn safe_cut(pending: &str, rules: &CompiledRules, lookbehind: usize) -> usize {
    let mut cut = pending.len().saturating_sub(lookbehind);
    if cut == 0 {
        return 0;
    }
    for re in rules.patterns() {
        let mut from = 0usize;
        while from <= pending.len() {
            let Ok(Some(m)) = re.find_from_pos(pending, from) else {
                break;
            };
            if m.end() == m.start() {
                // Zero-width match: step forward or loop forever.
                from = next_boundary(pending, m.end());
                if from == m.end() {
                    break;
                }
                continue;
            }
            if m.start() >= cut {
                break; // begins inside the withheld tail already
            }
            if m.end() > cut {
                cut = m.start(); // straddles the cut: keep it whole
            }
            from = m.end();
        }
    }
    // The cut must land on a character boundary or slicing panics.
    while cut > 0 && !pending.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

fn next_boundary(s: &str, mut idx: usize) -> usize {
    idx += 1;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx.min(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TextRule;
    use crate::relay::transform::compile_text_rules;

    fn rules(pattern: &str, replacement: &str) -> CompiledRules {
        compile_text_rules(&[TextRule {
            pattern: pattern.into(),
            flags: Some("gi".into()),
            replacement: replacement.into(),
            literal: false,
        }])
        .expect("rule compiles")
    }

    #[test]
    fn events_are_parsed_across_chunk_boundaries() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: {\"a\":").is_empty());
        let events = parser.push("1}\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn crlf_separators_from_proxies_are_tolerated() {
        let mut parser = SseParser::new();
        let events = parser.push("data: one\r\n\r\ndata: two\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
    }

    #[test]
    fn comments_and_keepalives_are_ignored() {
        let mut parser = SseParser::new();
        let events = parser.push(": keep-alive\n\ndata: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn a_match_split_across_two_chunks_is_still_caught() {
        let mut rewriter = StreamRewriter::new(Some(rules("DeepSeek", "Writer")), 32);
        let mut out = String::new();
        out.push_str(&rewriter.push("ask Deep"));
        out.push_str(&rewriter.push("Seek about it"));
        out.push_str(&rewriter.flush());
        assert_eq!(out, "ask Writer about it");
        assert!(!out.contains("DeepSeek"));
    }

    #[test]
    fn a_match_beginning_inside_the_emitted_prefix_is_held_back() {
        // The bug a fixed withheld tail cannot catch: the match starts well
        // before the tail and ends inside it.
        let mut rewriter = StreamRewriter::new(Some(rules("DeepSeek", "Writer")), 8);
        let mut out = String::new();
        out.push_str(&rewriter.push("padding padding padding DeepS"));
        out.push_str(&rewriter.push("eek now"));
        out.push_str(&rewriter.flush());
        assert_eq!(out, "padding padding padding Writer now");
    }

    #[test]
    fn multibyte_text_never_splits_mid_character() {
        let mut rewriter = StreamRewriter::new(Some(rules("测试", "test")), 4);
        let mut out = String::new();
        out.push_str(&rewriter.push("你好世界这是一个测"));
        out.push_str(&rewriter.push("试句子"));
        out.push_str(&rewriter.flush());
        assert_eq!(out, "你好世界这是一个test句子");
    }

    #[test]
    fn without_rules_the_stream_passes_straight_through() {
        let mut rewriter = StreamRewriter::new(None, 64);
        assert_eq!(rewriter.push("anything at all"), "anything at all");
        assert_eq!(rewriter.flush(), "");
    }
}
