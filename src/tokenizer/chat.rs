//! Message-level accounting.
//!
//! Counting raw message text is not enough: every backend wraps messages in a
//! chat template whose control tokens are billed too. Each family gets a
//! profile describing that overhead, and a route may pin a profile when a
//! backend does something unusual.

use serde::Serialize;
use serde_json::Value;

use super::registry::Encoder;
use crate::config::ImageDefaults;

#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub per_message: usize,
    pub per_name: usize,
    pub primer: usize,
    pub bos: usize,
}

pub const PROFILE_NAMES: [&str; 7] = [
    "openai", "chatml", "llama3", "deepseek", "mistral", "gemma", "raw",
];

pub fn profile(name: &str) -> Profile {
    match name {
        // <|im_start|>role\n content <|im_end|>\n
        "chatml" => Profile {
            per_message: 4,
            per_name: 1,
            primer: 3,
            bos: 0,
        },
        // <|start_header_id|>role<|end_header_id|>\n\n content <|eot_id|>
        "llama3" => Profile {
            per_message: 5,
            per_name: 1,
            primer: 4,
            bos: 1,
        },
        // <|User|> / <|Assistant|> markers, bos once
        "deepseek" => Profile {
            per_message: 2,
            per_name: 0,
            primer: 2,
            bos: 1,
        },
        // [INST] ... [/INST]
        "mistral" => Profile {
            per_message: 3,
            per_name: 0,
            primer: 2,
            bos: 1,
        },
        // <start_of_turn>role\n content <end_of_turn>\n
        "gemma" => Profile {
            per_message: 4,
            per_name: 0,
            primer: 3,
            bos: 1,
        },
        // plain concatenation, for completion-style backends
        "raw" => Profile {
            per_message: 0,
            per_name: 0,
            primer: 0,
            bos: 0,
        },
        // gpt-3.5/gpt-4/gpt-4o: <|start|>role<|message|>content<|end|>
        _ => Profile {
            per_message: 3,
            per_name: 1,
            primer: 3,
            bos: 0,
        },
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Breakdown {
    pub system: usize,
    pub user: usize,
    pub assistant: usize,
    pub tool: usize,
    pub images: usize,
    pub tools: usize,
    pub overhead: usize,
}

impl Breakdown {
    pub fn total(&self) -> usize {
        self.system
            + self.user
            + self.assistant
            + self.tool
            + self.images
            + self.tools
            + self.overhead
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestCount {
    pub total: usize,
    pub breakdown: Breakdown,
    pub exact: bool,
    pub tokenizer: String,
}

/// One countable piece of a message's content.
enum Part<'a> {
    Text(String),
    Image(&'a Value),
    Audio(&'a Value),
}

/// Flatten OpenAI content (string, or an array of parts) into countable pieces.
fn content_to_parts(content: &Value) -> Vec<Part<'_>> {
    match content {
        Value::Null => Vec::new(),
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => items
            .iter()
            .map(|p| match p {
                Value::String(s) => Part::Text(s.clone()),
                Value::Object(map) => match map.get("type").and_then(|v| v.as_str()) {
                    Some("text") | Some("input_text") => Part::Text(
                        map.get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("image_url") => Part::Image(map.get("image_url").unwrap_or(&Value::Null)),
                    Some("input_image") => Part::Image(p),
                    Some("input_audio") => {
                        Part::Audio(map.get("input_audio").unwrap_or(&Value::Null))
                    }
                    _ => Part::Text(p.to_string()),
                },
                other => Part::Text(other.to_string()),
            })
            .collect(),
        other => vec![Part::Text(other.to_string())],
    }
}

/// Flatten content down to plain text, for previews and rewrites.
pub fn flatten_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|p| match p {
                Value::String(s) => s.clone(),
                Value::Object(map) => map
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

/// Vision cost, following OpenAI's published tiling rule: `detail:"low"` is a
/// flat 85, high detail is 85 + 170 per 512px tile of the resized image.
pub fn image_tokens(image: &Value, defaults: &ImageDefaults) -> usize {
    let detail = image
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or(&defaults.default_detail);
    if detail == "low" {
        return 85;
    }
    let w = image
        .get("width")
        .and_then(|v| v.as_f64())
        .unwrap_or(f64::from(defaults.default_width));
    let h = image
        .get("height")
        .and_then(|v| v.as_f64())
        .unwrap_or(f64::from(defaults.default_height));

    let scale = (2048.0 / w.max(h)).min(1.0);
    let (mut sw, mut sh) = (w * scale, h * scale);
    let shortest = sw.min(sh);
    if shortest > 768.0 {
        let k = 768.0 / shortest;
        sw *= k;
        sh *= k;
    }
    let tiles = (sw / 512.0).ceil() * (sh / 512.0).ceil();
    85 + 170 * (tiles.max(0.0) as usize)
}

/// ~10 tokens per second of audio; base64 length is the only signal available
/// without decoding, so approximate from the payload size.
fn audio_tokens(audio: &Value) -> usize {
    let len = audio
        .get("data")
        .and_then(|v| v.as_str())
        .map_or(0, str::len);
    let bytes = len * 3 / 4;
    bytes.div_ceil(3200)
}

/// Tool definitions are injected into the prompt by the backend. Rendering
/// them into the pseudo-TypeScript shape OpenAI documents and counting that
/// tracks the real cost far better than counting the raw JSON.
pub fn render_tools(tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut lines = vec!["namespace functions {".to_string(), String::new()];
    for tool in tools {
        let f = tool.get("function").unwrap_or(tool);
        if let Some(desc) = f.get("description").and_then(|v| v.as_str()) {
            lines.push(format!("// {desc}"));
        }
        let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let params = f.get("parameters").cloned().unwrap_or(Value::Null);
        let props = params.get("properties").and_then(|v| v.as_object());
        let required: Vec<&str> = params
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        match props {
            Some(props) if !props.is_empty() => {
                lines.push(format!("type {name} = (_: {{"));
                for (key, spec) in props {
                    if let Some(desc) = spec.get("description").and_then(|v| v.as_str()) {
                        lines.push(format!("// {desc}"));
                    }
                    let opt = if required.contains(&key.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    lines.push(format!("{key}{opt}: {},", schema_type(spec)));
                }
                lines.push("}) => any;".into());
                lines.push(String::new());
            }
            _ => {
                lines.push(format!("type {name} = () => any;"));
                lines.push(String::new());
            }
        }
    }
    lines.push("} // namespace functions".into());
    lines.join("\n")
}

fn schema_type(spec: &Value) -> String {
    let Some(map) = spec.as_object() else {
        return "any".into();
    };
    if let Some(values) = map.get("enum").and_then(|v| v.as_array()) {
        return values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match map.get("type").and_then(|v| v.as_str()) {
        Some("array") => format!(
            "{}[]",
            schema_type(map.get("items").unwrap_or(&Value::Null))
        ),
        Some("object") => match map.get("properties").and_then(|v| v.as_object()) {
            Some(props) => {
                let inner = props
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", schema_type(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {inner} }}")
            }
            None => "object".into(),
        },
        Some(other) => other.to_string(),
        None => "any".into(),
    }
}

/// Count a whole chat request, with a breakdown of where the budget goes.
pub fn count_chat_request(
    body: &Value,
    encoder: &Encoder,
    profile_name: &str,
    images: &ImageDefaults,
) -> RequestCount {
    let p = profile(profile_name);
    let count = |s: &str| if s.is_empty() { 0 } else { encoder.count(s) };
    let mut b = Breakdown::default();

    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            b.overhead += p.per_message;
            if let Some(name) = msg.get("name").and_then(|v| v.as_str()) {
                b.overhead += p.per_name + count(name);
            }
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            b.overhead += count(role);

            let bucket = match role {
                "system" => 0,
                "assistant" => 1,
                "tool" | "function" => 2,
                _ => 3,
            };

            for part in content_to_parts(msg.get("content").unwrap_or(&Value::Null)) {
                match part {
                    Part::Text(text) => {
                        let n = count(&text);
                        match bucket {
                            0 => b.system += n,
                            1 => b.assistant += n,
                            2 => b.tool += n,
                            _ => b.user += n,
                        }
                    }
                    Part::Image(image) => b.images += image_tokens(image, images),
                    Part::Audio(audio) => {
                        let n = audio_tokens(audio);
                        match bucket {
                            0 => b.system += n,
                            1 => b.assistant += n,
                            2 => b.tool += n,
                            _ => b.user += n,
                        }
                    }
                }
            }

            // Assistant turns replaying tool calls still cost their serialised form.
            if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in calls {
                    let f = tc.get("function").unwrap_or(&Value::Null);
                    b.assistant += count(f.get("name").and_then(|v| v.as_str()).unwrap_or(""));
                    b.assistant += count(f.get("arguments").and_then(|v| v.as_str()).unwrap_or(""));
                    b.overhead += 3;
                }
            }
            if msg.get("tool_call_id").is_some() {
                b.overhead += 2;
            }
        }
    }

    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            b.tools += count(&render_tools(tools)) + 12;
            match body.get("tool_choice") {
                Some(Value::String(s)) if s == "auto" => {}
                Some(Value::Null) | None => {}
                Some(_) => b.tools += 4,
            }
        }
    } else if let Some(functions) = body.get("functions").and_then(|v| v.as_array()) {
        if !functions.is_empty() {
            let wrapped: Vec<Value> = functions
                .iter()
                .map(|f| serde_json::json!({ "function": f }))
                .collect();
            b.tools += count(&render_tools(&wrapped)) + 12;
        }
    }

    // Legacy completion-style bodies.
    match body.get("prompt") {
        Some(Value::String(s)) => b.user += count(s),
        Some(Value::Array(items)) => {
            for p in items {
                b.user += count(
                    &p.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| p.to_string()),
                );
            }
        }
        _ => {}
    }

    b.overhead += p.primer + p.bos;

    RequestCount {
        total: b.total(),
        breakdown: b,
        exact: encoder.exact(),
        tokenizer: encoder.name().to_string(),
    }
}

/// What the `system` turns in this body cost on their own.
///
/// Used to separate the prompt the caller wrote from the one the relay injects
/// on their behalf. It runs the same accounting as [`count_chat_request`] over
/// a body holding only the system turns — the same function, so the two can
/// never drift apart — and then removes the per-request primer, which belongs
/// to the request as a whole rather than to any one message.
pub fn count_system_messages(
    body: &Value,
    encoder: &Encoder,
    profile_name: &str,
    images: &ImageDefaults,
) -> usize {
    let system: Vec<Value> = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|messages| {
            messages
                .iter()
                .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if system.is_empty() {
        return 0;
    }
    let p = profile(profile_name);
    let only = serde_json::json!({ "messages": system });
    count_chat_request(&only, encoder, profile_name, images)
        .total
        .saturating_sub(p.primer + p.bos)
}

/// Whether two bodies agree everywhere the count looks, apart from system turns.
///
/// When they do, counting one gives the other away for free: the whole
/// difference between them is the difference between their system turns.
/// Injecting a system prompt always lands here, which is the common case and
/// the one worth saving a second pass over. A rewrite rule that edits the
/// caller's own text does not, and then there is nothing for it but to count.
pub fn same_but_system(a: &Value, b: &Value) -> bool {
    fn others(v: &Value) -> Vec<&Value> {
        v.get("messages")
            .and_then(|m| m.as_array())
            .map(|messages| {
                messages
                    .iter()
                    .filter(|m| m.get("role").and_then(|v| v.as_str()) != Some("system"))
                    .collect()
            })
            .unwrap_or_default()
    }
    others(a) == others(b)
        && ["tools", "functions", "prompt"]
            .into_iter()
            .all(|k| a.get(k) == b.get(k))
}

/// Count a completion: text, reasoning trace and tool calls.
pub fn count_completion(
    text: &str,
    encoder: &Encoder,
    reasoning: &str,
    tool_calls: &[Value],
) -> usize {
    let mut total = encoder.count(text);
    if !reasoning.is_empty() {
        total += encoder.count(reasoning);
    }
    for tc in tool_calls {
        let f = tc.get("function").unwrap_or(&Value::Null);
        total += encoder.count(f.get("name").and_then(|v| v.as_str()).unwrap_or(""));
        total += encoder.count(f.get("arguments").and_then(|v| v.as_str()).unwrap_or(""));
        total += 3;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::registry::builtin_encoder;

    fn enc() -> Encoder {
        // Any exact vocabulary works; the assertions are about the accounting.
        builtin_encoder("cl100k_base").expect("cl100k is built in")
    }

    #[test]
    fn the_whole_difference_between_two_bodies_is_their_system_turns() {
        let original = serde_json::json!({
            "messages": [
                {"role": "user", "content": "Halo, apa kabar hari ini?"},
                {"role": "assistant", "content": "Baik, terima kasih."},
            ]
        });
        let upstream = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You answer briefly and in the caller's language."},
                {"role": "user", "content": "Halo, apa kabar hari ini?"},
                {"role": "assistant", "content": "Baik, terima kasih."},
            ]
        });
        let (e, img) = (enc(), ImageDefaults::default());
        let theirs = count_chat_request(&original, &e, "openai", &img).total;
        let sent = count_chat_request(&upstream, &e, "openai", &img).total;
        let injected = count_system_messages(&upstream, &e, "openai", &img)
            - count_system_messages(&original, &e, "openai", &img);

        assert!(injected > 0, "the injected prompt should cost real tokens");
        assert!(same_but_system(&original, &upstream));
        // The saving behind `count_prompt`: the caller's body is counted in
        // full and the body on the wire falls out of it, exactly, without a
        // second walk over the whole conversation.
        assert_eq!(theirs + injected, sent);
    }

    #[test]
    fn a_rewritten_turn_is_not_the_same_body_and_has_to_be_counted() {
        let original = serde_json::json!({"messages": [{"role": "user", "content": "halo"}]});
        let injected = serde_json::json!({
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "halo"},
            ]
        });
        let rewritten = serde_json::json!({
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "halo halo halo"},
            ]
        });
        let tooled = serde_json::json!({
            "messages": [{"role": "user", "content": "halo"}],
            "tools": [{"type": "function", "function": {"name": "now"}}],
        });

        assert!(same_but_system(&original, &injected));
        assert!(!same_but_system(&original, &rewritten));
        assert!(!same_but_system(&original, &tooled));
    }

    #[test]
    fn overhead_is_charged_per_message_and_for_priming() {
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Halo"},
            ]
        });
        let openai = count_chat_request(&body, &enc(), "openai", &ImageDefaults::default());
        let raw = count_chat_request(&body, &enc(), "raw", &ImageDefaults::default());

        // `raw` contributes no template tokens, so the whole difference is the
        // openai profile's own: two messages at 3 each, plus a 3-token primer.
        let p = profile("openai");
        assert_eq!(
            openai.total - raw.total,
            2 * p.per_message + p.primer + p.bos
        );
        assert_eq!(openai.breakdown.system, raw.breakdown.system);
        assert_eq!(openai.breakdown.user, raw.breakdown.user);
    }

    #[test]
    fn image_tiling_follows_the_published_rule() {
        let d = ImageDefaults::default();
        assert_eq!(image_tokens(&serde_json::json!({"detail": "low"}), &d), 85);
        // a 1024x1024 upload resizes to 768x768 => 2x2 tiles
        assert_eq!(
            image_tokens(&serde_json::json!({"width": 1024, "height": 1024}), &d),
            85 + 170 * 4
        );
    }

    #[test]
    fn tools_render_as_the_pseudo_typescript_openai_documents() {
        let tools = vec![serde_json::json!({
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string", "description": "City name"},
                        "unit": {"enum": ["c", "f"]},
                    },
                    "required": ["city"],
                },
            }
        })];
        let rendered = render_tools(&tools);
        assert!(rendered.contains("type get_weather = (_: {"));
        assert!(rendered.contains("city: string,"));
        assert!(rendered.contains("unit?: \"c\" | \"f\","));
    }

    #[test]
    fn content_arrays_split_into_text_and_images() {
        let body = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image_url", "image_url": {"detail": "low"}},
                ],
            }]
        });
        let counted = count_chat_request(&body, &enc(), "openai", &ImageDefaults::default());
        assert_eq!(counted.breakdown.images, 85);
        assert!(counted.breakdown.user > 0);
    }
}
