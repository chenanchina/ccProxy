use crate::config::ModelMap;
use crate::error::AppError;
use crate::sse::{encode_sse, SseEvent};
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

/// Normalizes a reasoning effort string to one of the upstream-supported levels,
/// accepting common aliases (`max` -> `xhigh`). Returns None when unrecognized.
fn normalize_effort(s: &str) -> Option<String> {
    match s.trim().to_lowercase().as_str() {
        "low" => Some("low".to_string()),
        "medium" => Some("medium".to_string()),
        "high" => Some("high".to_string()),
        "xhigh" | "max" => Some("xhigh".to_string()),
        _ => None,
    }
}

fn is_effort(s: &str) -> bool {
    normalize_effort(s).is_some()
}

pub fn new_message_id() -> String {
    gen_id("msg_")
}

fn gen_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

pub struct ParsedModel {
    pub id: String,
    pub reasoning_effort: Option<String>,
}

pub fn parse_model_and_reasoning(model: &str) -> ParsedModel {
    let value = model.trim();

    // spaced: "id high"
    if let Some(pos) = value.rfind(char::is_whitespace) {
        let tail = &value[pos + 1..];
        let head = value[..pos].trim_end();
        if is_effort(tail) && !head.is_empty() {
            return ParsedModel {
                id: head.to_string(),
                reasoning_effort: normalize_effort(tail),
            };
        }
    }

    // separated: "id/high" or "id:high"
    if let Some(pos) = value.rfind(['/', ':']) {
        let tail = &value[pos + 1..];
        let head = value[..pos].trim();
        if is_effort(tail) && !head.is_empty() {
            return ParsedModel {
                id: head.to_string(),
                reasoning_effort: normalize_effort(tail),
            };
        }
    }

    // dashed: "id-high"
    if let Some(pos) = value.rfind('-') {
        let tail = &value[pos + 1..];
        let head = value[..pos].trim();
        if is_effort(tail) && !head.is_empty() {
            return ParsedModel {
                id: head.to_string(),
                reasoning_effort: normalize_effort(tail),
            };
        }
    }

    ParsedModel {
        id: value.to_string(),
        reasoning_effort: None,
    }
}

fn validate(request: &Value) -> Result<(), AppError> {
    if !request.is_object() {
        return Err(AppError::bad_request("Request body must be a JSON object"));
    }
    let model = request.get("model").and_then(|v| v.as_str());
    let Some(model) = model else {
        return Err(AppError::bad_request("model is required"));
    };
    let _ = parse_model_and_reasoning(model);
    if let Some(effort) = request.get("reasoning_effort").and_then(|v| v.as_str()) {
        if normalize_effort(effort).is_none() {
            return Err(AppError::bad_request("Unsupported reasoning_effort"));
        }
    }
    let max_tokens = request.get("max_tokens").and_then(|v| v.as_f64());
    match max_tokens {
        Some(n) if n.is_finite() && n > 0.0 => {}
        _ => {
            return Err(AppError::bad_request(
                "max_tokens must be a positive number",
            ))
        }
    }
    if !request
        .get("messages")
        .map(|v| v.is_array())
        .unwrap_or(false)
    {
        return Err(AppError::bad_request("messages must be an array"));
    }
    Ok(())
}

pub fn anthropic_to_responses(
    request: &Value,
    default_instructions: &str,
    model_map: &ModelMap,
) -> Result<Value, AppError> {
    validate(request)?;
    let model = parse_model_and_reasoning(request.get("model").and_then(|v| v.as_str()).unwrap());

    let mut payload = Map::new();
    payload.insert("model".into(), json!(map_model_id(&model.id, model_map)));
    payload.insert(
        "input".into(),
        Value::Array(messages_to_input(
            request.get("messages").and_then(|v| v.as_array()).unwrap(),
        )),
    );
    if let Some(mt) = request.get("max_tokens") {
        payload.insert("max_output_tokens".into(), mt.clone());
    }
    payload.insert("store".into(), json!(false));

    let reasoning_effort = request
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .and_then(normalize_effort)
        .or(model.reasoning_effort)
        .or_else(|| thinking_to_effort(request));
    if let Some(effort) = reasoning_effort {
        payload.insert("reasoning".into(), json!({ "effort": effort }));
    }

    let instructions = system_to_text(request.get("system"));
    let instructions = instructions.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        if default_instructions.is_empty() {
            "You are a helpful assistant.".to_string()
        } else {
            default_instructions.to_string()
        }
    });
    payload.insert("instructions".into(), json!(instructions));

    for key in ["temperature", "top_p"] {
        if let Some(v) = request.get(key).filter(|v| !v.is_null()) {
            payload.insert(key.into(), v.clone());
        }
    }
    if let Some(stop) = request.get("stop_sequences").filter(|v| !v.is_null()) {
        payload.insert("stop".into(), stop.clone());
    }

    if let Some(tools) = request.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            payload.insert(
                "tools".into(),
                Value::Array(tools.iter().map(tool_to_response_tool).collect()),
            );
        }
    }

    if let Some(choice) = tool_choice_to_responses(request.get("tool_choice")) {
        payload.insert("tool_choice".into(), choice);
    }

    Ok(Value::Object(payload))
}

fn messages_to_input(messages: &[Value]) -> Vec<Value> {
    messages.iter().flat_map(message_to_input_items).collect()
}

fn message_to_input_items(message: &Value) -> Vec<Value> {
    let role = if message.get("role").and_then(|v| v.as_str()) == Some("assistant") {
        "assistant"
    } else {
        "user"
    };
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };

    let mut items: Vec<Value> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();

    let flush = |items: &mut Vec<Value>, text_parts: &mut Vec<String>| {
        if text_parts.is_empty() {
            return;
        }
        let content: Vec<Value> = text_parts
            .iter()
            .map(|t| json!({ "type": text_type, "text": t }))
            .collect();
        items.push(json!({ "role": role, "content": content }));
        text_parts.clear();
    };

    let content = message.get("content");
    let blocks = content.and_then(|v| v.as_array());

    let Some(blocks) = blocks else {
        // string or other scalar content
        text_parts = content_to_text_parts(content.unwrap_or(&Value::Null));
        flush(&mut items, &mut text_parts);
        if items.is_empty() {
            return vec![json!({ "role": role, "content": [{ "type": text_type, "text": "" }] })];
        }
        return items;
    };

    for block in blocks {
        let btype = block.get("type").and_then(|v| v.as_str());
        if block.is_string() || btype == Some("text") || btype == Some("image") {
            text_parts.extend(content_to_text_parts(block));
            continue;
        }

        if role == "assistant" && matches!(btype, Some("thinking") | Some("redacted_thinking")) {
            if let Some(item) = thinking_block_to_reasoning(block) {
                flush(&mut items, &mut text_parts);
                items.push(item);
            }
            continue;
        }

        if role == "assistant" && btype == Some("tool_use") {
            flush(&mut items, &mut text_parts);
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            items.push(json!({
                "type": "function_call",
                "call_id": block.get("id").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| gen_id("call_")),
                "name": block.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
            }));
            continue;
        }

        if role == "user" && btype == Some("tool_result") {
            flush(&mut items, &mut text_parts);
            items.push(json!({
                "type": "function_call_output",
                "call_id": block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or(""),
                "output": tool_result_content_to_text(block.get("content")),
            }));
            continue;
        }

        text_parts.extend(content_to_text_parts(block));
    }

    flush(&mut items, &mut text_parts);

    if items.is_empty() {
        return vec![json!({ "role": role, "content": [{ "type": text_type, "text": "" }] })];
    }
    items
}

fn content_to_text_parts(content: &Value) -> Vec<String> {
    if let Some(s) = content.as_str() {
        return vec![s.to_string()];
    }
    let Some(arr) = content.as_array() else {
        if content.is_null() {
            return vec![String::new()];
        }
        return vec![content.to_string()];
    };

    arr.iter()
        .flat_map(|block| {
            if let Some(s) = block.as_str() {
                return vec![s.to_string()];
            }
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => vec![block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()],
                Some("tool_result") => vec![format!(
                    "Tool result {}:\n{}",
                    block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    tool_result_content_to_text(block.get("content"))
                )],
                Some("tool_use") => vec![format!(
                    "Tool use {} {}:\n{}",
                    block.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    block.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    serde_json::to_string(
                        &block.get("input").cloned().unwrap_or_else(|| json!({}))
                    )
                    .unwrap_or_default()
                )],
                Some("image") => vec!["[image omitted by local proxy]".to_string()],
                other => vec![format!(
                    "[unsupported {} block omitted by local proxy]",
                    other.unwrap_or("content")
                )],
            }
        })
        .collect()
}

fn tool_result_content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        Some(v) if v.is_array() => content_to_text_parts(v).join("\n"),
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}

fn system_to_text(system: Option<&Value>) -> Option<String> {
    let system = system?;
    if system.is_null() {
        return None;
    }
    if let Some(s) = system.as_str() {
        return Some(s.to_string());
    }
    if system.is_array() {
        return Some(content_to_text_parts(system).join("\n"));
    }
    Some(system.to_string())
}

fn tool_to_response_tool(tool: &Value) -> Value {
    json!({
        "type": "function",
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "description": tool.get("description").and_then(|v| v.as_str()).unwrap_or(""),
        "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        "strict": false,
    })
}

fn tool_choice_to_responses(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    if let Some(s) = choice.as_str() {
        return match s {
            "auto" => Some(json!("auto")),
            "none" => Some(json!("none")),
            _ => None,
        };
    }
    if !choice.is_object() {
        return None;
    }
    match choice.get("type").and_then(|v| v.as_str()) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("tool") => Some(
            json!({ "type": "function", "name": choice.get("name").cloned().unwrap_or(Value::Null) }),
        ),
        _ => None,
    }
}

/// Maps an Anthropic-family model name (claude / sonnet / opus / haiku, with an
/// optional `[1m]`-style context suffix) onto the configured upstream gpt model.
/// Names that already look like a gpt/other model pass through unchanged.
fn map_model_id(id: &str, map: &ModelMap) -> String {
    let lower = id.to_lowercase();
    let base = lower.split('[').next().unwrap_or(&lower).trim();
    let is_anthropic = base.starts_with("claude")
        || base.contains("haiku")
        || base.contains("sonnet")
        || base.contains("opus");
    if !is_anthropic {
        return id.to_string();
    }
    if base.contains("haiku") {
        map.haiku.clone()
    } else if base.contains("opus") {
        map.opus.clone()
    } else {
        map.sonnet.clone()
    }
}

pub fn thinking_enabled(request: &Value) -> bool {
    request
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        == Some("enabled")
}

/// Translates an Anthropic `thinking` request block into an upstream reasoning
/// effort, scaling by the requested budget when present.
fn thinking_to_effort(request: &Value) -> Option<String> {
    if !thinking_enabled(request) {
        return None;
    }
    let budget = request
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|v| v.as_i64());
    Some(
        match budget {
            Some(b) if b >= 24_000 => "high",
            Some(b) if b >= 4_000 => "medium",
            Some(_) => "low",
            None => "medium",
        }
        .to_string(),
    )
}

/// Opaque reference packed into a thinking block's signature so a later turn can
/// reconstruct the upstream reasoning item (id + encrypted content) verbatim.
fn encode_reasoning_ref(id: Option<&str>, enc: Option<&str>) -> String {
    let payload = json!({ "id": id, "enc": enc });
    base64::engine::general_purpose::STANDARD.encode(payload.to_string())
}

fn decode_reasoning_ref(sig: &str) -> Option<(Option<String>, Option<String>)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(sig).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let id = v.get("id").and_then(|v| v.as_str()).map(String::from);
    let enc = v.get("enc").and_then(|v| v.as_str()).map(String::from);
    if id.is_none() && enc.is_none() {
        return None;
    }
    Some((id, enc))
}

/// Rebuilds an upstream reasoning input item from an assistant thinking block the
/// client echoed back. Returns None when the block carries no recoverable state.
fn thinking_block_to_reasoning(block: &Value) -> Option<Value> {
    let btype = block.get("type").and_then(|v| v.as_str());
    let (id, enc, summary_text) = match btype {
        Some("thinking") => {
            let sig = block.get("signature").and_then(|v| v.as_str())?;
            let (id, enc) = decode_reasoning_ref(sig)?;
            let text = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
            (id, enc, text.to_string())
        }
        Some("redacted_thinking") => {
            let data = block.get("data").and_then(|v| v.as_str())?;
            let (id, enc) = decode_reasoning_ref(data)?;
            (id, enc, String::new())
        }
        _ => return None,
    };
    enc.as_ref()?;
    let mut item = Map::new();
    item.insert("type".into(), json!("reasoning"));
    if let Some(id) = id {
        item.insert("id".into(), json!(id));
    }
    item.insert("encrypted_content".into(), json!(enc));
    if !summary_text.is_empty() {
        item.insert(
            "summary".into(),
            json!([{ "type": "summary_text", "text": summary_text }]),
        );
    } else {
        item.insert("summary".into(), json!([]));
    }
    Some(Value::Object(item))
}

/// Converts an upstream reasoning output item into an Anthropic thinking block,
/// stashing the encrypted content in the signature for the next turn.
fn reasoning_item_to_thinking(item: &Value) -> Option<Value> {
    let enc = item.get("encrypted_content").and_then(|v| v.as_str())?;
    let id = item.get("id").and_then(|v| v.as_str());
    let text = item
        .get("summary")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    Some(json!({
        "type": "thinking",
        "thinking": text,
        "signature": encode_reasoning_ref(id, Some(enc)),
    }))
}

pub fn responses_to_anthropic(response: &Value, original: &Value) -> Value {
    let emit_thinking = thinking_enabled(original);
    let content = response_output_to_blocks(response, emit_thinking);
    let stop_reason = infer_stop_reason(response, &content);
    let usage = response.get("usage");

    json!({
        "id": response.get("id").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| gen_id("msg_")),
        "type": "message",
        "role": "assistant",
        "model": response.get("model").cloned().or_else(|| original.get("model").cloned()).unwrap_or(Value::Null),
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": if stop_reason == "stop_sequence" { first_stop_sequence(original) } else { Value::Null },
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0),
            "output_tokens": usage.and_then(|u| u.get("output_tokens")).and_then(|v| v.as_i64()).unwrap_or(0),
        },
    })
}

fn response_output_to_blocks(response: &Value, emit_thinking: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(output) = response.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if emit_thinking && item.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
                if let Some(block) = reasoning_item_to_thinking(item) {
                    blocks.push(block);
                }
            }
            if item.get("type").and_then(|v| v.as_str()) == Some("message") {
                if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                    for c in content {
                        if matches!(
                            c.get("type").and_then(|v| v.as_str()),
                            Some("output_text") | Some("text")
                        ) {
                            blocks.push(json!({
                                "type": "text",
                                "text": c.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                            }));
                        }
                    }
                }
            }
            if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": item.get("call_id").and_then(|v| v.as_str())
                        .or_else(|| item.get("id").and_then(|v| v.as_str()))
                        .map(String::from)
                        .unwrap_or_else(|| gen_id("toolu_")),
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "input": parse_json_object(item.get("arguments")),
                }));
            }
        }
    }

    if blocks.is_empty() {
        if let Some(text) = response.get("output_text").and_then(|v| v.as_str()) {
            blocks.push(json!({ "type": "text", "text": text }));
        }
    }

    blocks
}

fn parse_json_object(raw: Option<&Value>) -> Value {
    match raw {
        Some(v) if v.is_object() => v.clone(),
        Some(v) if v.is_string() => {
            let s = v.as_str().unwrap();
            if s.is_empty() {
                return json!({});
            }
            serde_json::from_str::<Value>(s)
                .ok()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| json!({}))
        }
        _ => json!({}),
    }
}

fn infer_stop_reason(response: &Value, content: &[Value]) -> String {
    if content
        .iter()
        .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
    {
        return "tool_use".to_string();
    }
    if response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(|v| v.as_str())
        == Some("max_output_tokens")
    {
        return "max_tokens".to_string();
    }
    if response.get("status").and_then(|v| v.as_str()) == Some("incomplete") {
        return "max_tokens".to_string();
    }
    "end_turn".to_string()
}

fn first_stop_sequence(request: &Value) -> Value {
    request
        .get("stop_sequences")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null)
}

// ---- Streaming state machine ----

pub struct StreamState {
    pub message_id: String,
    pub next_index: usize,
    pub text_blocks: HashMap<String, usize>,
    pub function_blocks: HashMap<String, usize>,
    pub function_argument_deltas: HashSet<String>,
    pub output_to_block: Vec<(String, usize)>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub stop_reason: String,
    pub stream_errored: bool,
    pub emit_thinking: bool,
}

impl StreamState {
    pub fn new(message_id: String, emit_thinking: bool) -> Self {
        StreamState {
            message_id,
            next_index: 0,
            text_blocks: HashMap::new(),
            function_blocks: HashMap::new(),
            function_argument_deltas: HashSet::new(),
            output_to_block: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            stop_reason: "end_turn".to_string(),
            stream_errored: false,
            emit_thinking,
        }
    }
}

fn num_or_str(data: &Value, key: &str) -> Option<String> {
    match data.get(key) {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn text_key(data: &Value) -> String {
    if let Some(k) = num_or_str(data, "output_index") {
        return k;
    }
    let item = num_or_str(data, "item_id").unwrap_or_else(|| "text".to_string());
    let content_index = data
        .get("content_index")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    format!("{item}:{content_index}")
}

fn fn_key(data: &Value) -> String {
    num_or_str(data, "output_index")
        .or_else(|| num_or_str(data, "item_id"))
        .unwrap_or_default()
}

fn start_text_block(state: &mut StreamState, key: &str) -> Option<String> {
    if state.text_blocks.contains_key(key) {
        return None;
    }
    let index = state.next_index;
    state.next_index += 1;
    state.text_blocks.insert(key.to_string(), index);
    state.output_to_block.push((key.to_string(), index));
    Some(encode_sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" },
        }),
    ))
}

fn start_function_block(state: &mut StreamState, key: &str, item: &Value) -> Option<String> {
    if state.function_blocks.contains_key(key) {
        return None;
    }
    let index = state.next_index;
    state.next_index += 1;
    state.function_blocks.insert(key.to_string(), index);
    state.output_to_block.push((key.to_string(), index));
    let id = item
        .get("call_id")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("id").and_then(|v| v.as_str()))
        .map(String::from)
        .unwrap_or_else(|| gen_id("toolu_"));
    Some(encode_sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "tool_use",
                "id": id,
                "name": item.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "input": {},
            },
        }),
    ))
}

fn stop_block(state: &mut StreamState, key: &str) -> Option<String> {
    let pos = state.output_to_block.iter().position(|(k, _)| k == key)?;
    let (_, index) = state.output_to_block.remove(pos);
    Some(encode_sse(
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": index }),
    ))
}

pub fn map_stream_event(event: &SseEvent, state: &mut StreamState) -> Vec<String> {
    if event.done {
        return Vec::new();
    }
    let data = &event.data;
    let typ = data
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or(&event.event)
        .to_string();
    let mut frames: Vec<String> = Vec::new();

    match typ.as_str() {
        "response.created" => {
            if let Some(id) = data
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
            {
                state.message_id = id.to_string();
            }
        }
        "response.completed" | "response.done" => {
            let response = data
                .get("response")
                .cloned()
                .unwrap_or_else(|| data.clone());
            if let Some(usage) = response.get("usage") {
                if let Some(i) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                    state.input_tokens = i;
                }
                if let Some(o) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                    state.output_tokens = o;
                }
                if let Some(r) = usage
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_i64())
                {
                    state.reasoning_tokens = r;
                }
            }
            let inferred = infer_stop_reason(&response, &[]);
            if !(state.stop_reason == "tool_use" && inferred == "end_turn") {
                state.stop_reason = inferred;
            }
        }
        "response.output_item.added"
            if data
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(|v| v.as_str())
                == Some("function_call") =>
        {
            let key = fn_key(data);
            let item = data.get("item").cloned().unwrap_or(Value::Null);
            push_opt(&mut frames, start_function_block(state, &key, &item));
        }
        "response.output_text.delta" => {
            let key = text_key(data);
            push_opt(&mut frames, start_text_block(state, &key));
            if let Some(&index) = state.text_blocks.get(&key) {
                frames.push(encode_sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": data.get("delta").and_then(|v| v.as_str()).unwrap_or("") },
                    }),
                ));
            }
        }
        "response.function_call_arguments.delta" => {
            let key = fn_key(data);
            let item = data.get("item").cloned().unwrap_or_else(|| {
                json!({
                    "id": data.get("item_id").cloned().unwrap_or(Value::Null),
                    "name": data.get("name").cloned().unwrap_or(Value::Null),
                })
            });
            push_opt(&mut frames, start_function_block(state, &key, &item));
            state.function_argument_deltas.insert(key.clone());
            if let Some(&index) = state.function_blocks.get(&key) {
                frames.push(encode_sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": data.get("delta").and_then(|v| v.as_str()).unwrap_or("") },
                    }),
                ));
            }
        }
        "response.function_call_arguments.done" => {
            let key = fn_key(data);
            let item = data.get("item").cloned().unwrap_or_else(|| {
                json!({
                    "id": data.get("item_id").cloned().unwrap_or(Value::Null),
                    "call_id": data.get("call_id").cloned().unwrap_or(Value::Null),
                    "name": data.get("name").cloned().unwrap_or(Value::Null),
                })
            });
            push_opt(&mut frames, start_function_block(state, &key, &item));
            let args = data.get("arguments").and_then(|v| v.as_str());
            if let (Some(args), false) = (args, state.function_argument_deltas.contains(&key)) {
                if let Some(&index) = state.function_blocks.get(&key) {
                    frames.push(encode_sse(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": { "type": "input_json_delta", "partial_json": args },
                        }),
                    ));
                }
            }
            push_opt(&mut frames, stop_block(state, &key));
            state.stop_reason = "tool_use".to_string();
        }
        "response.output_item.done" => {
            let item = data.get("item");
            if state.emit_thinking
                && item.and_then(|i| i.get("type")).and_then(|v| v.as_str()) == Some("reasoning")
            {
                if let Some(block) = item.and_then(reasoning_item_to_thinking) {
                    let index = state.next_index;
                    state.next_index += 1;
                    frames.push(encode_sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": { "type": "thinking", "thinking": "", "signature": "" },
                        }),
                    ));
                    if let Some(text) = block
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        frames.push(encode_sse(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": { "type": "thinking_delta", "thinking": text },
                            }),
                        ));
                    }
                    frames.push(encode_sse(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": { "type": "signature_delta", "signature": block.get("signature").and_then(|v| v.as_str()).unwrap_or("") },
                        }),
                    ));
                    frames.push(encode_sse(
                        "content_block_stop",
                        &json!({ "type": "content_block_stop", "index": index }),
                    ));
                }
            }
            if item.and_then(|i| i.get("type")).and_then(|v| v.as_str()) == Some("function_call") {
                let key = fn_key(data);
                let item = item.cloned().unwrap_or(Value::Null);
                push_opt(&mut frames, start_function_block(state, &key, &item));
                let args = item.get("arguments").and_then(|v| v.as_str());
                if let (Some(args), false) = (args, state.function_argument_deltas.contains(&key)) {
                    if let Some(&index) = state.function_blocks.get(&key) {
                        frames.push(encode_sse(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": { "type": "input_json_delta", "partial_json": args },
                            }),
                        ));
                    }
                }
                push_opt(&mut frames, stop_block(state, &key));
                state.stop_reason = "tool_use".to_string();
            }
        }
        "response.output_text.done" => {
            let key = text_key(data);
            push_opt(&mut frames, stop_block(state, &key));
        }
        "error" => {
            frames.push(encode_sse(
                "error",
                &json!({
                    "type": "error",
                    "error": {
                        "type": data.get("error").and_then(|e| e.get("type")).and_then(|v| v.as_str()).unwrap_or("api_error"),
                        "message": data.get("message").and_then(|v| v.as_str())
                            .or_else(|| data.get("error").and_then(|e| e.get("message")).and_then(|v| v.as_str()))
                            .unwrap_or("Upstream stream error"),
                    },
                }),
            ));
            state.stream_errored = true;
        }
        _ => {}
    }

    frames
}

fn push_opt(frames: &mut Vec<String>, frame: Option<String>) {
    if let Some(f) = frame {
        frames.push(f);
    }
}

pub fn message_start_frame(message_id: &str, model: &Value) -> String {
    encode_sse(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            },
        }),
    )
}

pub fn finish_open_blocks(state: &mut StreamState) -> Vec<String> {
    let mut frames = Vec::new();
    let remaining: Vec<(String, usize)> = state.output_to_block.drain(..).collect();
    for (_, index) in remaining {
        frames.push(encode_sse(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": index }),
        ));
    }
    frames
}

pub fn message_delta_frame(state: &StreamState) -> String {
    encode_sse(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": state.stop_reason, "stop_sequence": null },
            "usage": { "output_tokens": state.output_tokens },
        }),
    )
}

pub fn message_stop_frame() -> String {
    encode_sse("message_stop", &json!({ "type": "message_stop" }))
}

pub fn error_frame(kind: &str, message: &str) -> String {
    encode_sse(
        "error",
        &json!({
            "type": "error",
            "error": { "type": kind, "message": message },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reasoning_effort_variants() {
        for input in [
            "gpt-5.5-high",
            "gpt-5.5 high",
            "gpt-5.5:high",
            "gpt-5.5/high",
        ] {
            let p = parse_model_and_reasoning(input);
            assert_eq!(p.id, "gpt-5.5", "input={input}");
            assert_eq!(p.reasoning_effort.as_deref(), Some("high"), "input={input}");
        }
    }

    #[test]
    fn keeps_model_without_effort() {
        let p = parse_model_and_reasoning("gpt-5.3-codex");
        assert_eq!(p.id, "gpt-5.3-codex");
        assert_eq!(p.reasoning_effort, None);

        let p = parse_model_and_reasoning("gpt-5.4-mini");
        assert_eq!(p.id, "gpt-5.4-mini");
        assert_eq!(p.reasoning_effort, None);
    }

    #[test]
    fn builds_responses_payload_with_tools_and_system() {
        let req = json!({
            "model": "gpt-5.5-high",
            "max_tokens": 200,
            "system": "be terse",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "call_1", "name": "lookup", "input": { "q": "x" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_1", "content": "ok" }
                ]}
            ],
            "tools": [
                { "name": "lookup", "description": "d", "input_schema": { "type": "object" } }
            ],
            "tool_choice": { "type": "any" }
        });
        let payload = anthropic_to_responses(&req, "default", &ModelMap::default()).unwrap();

        assert_eq!(payload["model"], "gpt-5.5");
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["instructions"], "be terse");
        assert_eq!(payload["max_output_tokens"], 200);
        assert_eq!(payload["tool_choice"], "required");
        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["tools"][0]["name"], "lookup");

        let input = payload["input"].as_array().unwrap();
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "ok");
    }

    #[test]
    fn converts_response_to_anthropic_with_tool_use() {
        let response = json!({
            "id": "resp_1",
            "model": "gpt-5.5",
            "output": [
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hello" }] },
                { "type": "function_call", "call_id": "call_9", "name": "run", "arguments": "{\"a\":1}" }
            ],
            "usage": { "input_tokens": 7, "output_tokens": 3 }
        });
        let original = json!({ "model": "gpt-5.5-high" });
        let msg = responses_to_anthropic(&response, &original);

        assert_eq!(msg["id"], "resp_1");
        assert_eq!(msg["stop_reason"], "tool_use");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "hello");
        assert_eq!(msg["content"][1]["type"], "tool_use");
        assert_eq!(msg["content"][1]["id"], "call_9");
        assert_eq!(msg["content"][1]["input"]["a"], 1);
        assert_eq!(msg["usage"]["input_tokens"], 7);
    }

    #[test]
    fn maps_claude_models_and_passes_through_gpt() {
        let map = ModelMap::default();
        let mk = |model: &str| {
            let req = json!({ "model": model, "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
            anthropic_to_responses(&req, "d", &map).unwrap()["model"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(mk("claude-sonnet-4-5"), "gpt-5.5");
        assert_eq!(mk("claude-opus-4-1"), "gpt-5.5");
        assert_eq!(mk("claude-3-5-haiku-20241022"), "gpt-5.4-mini");
        assert_eq!(mk("claude-sonnet-4-5[1m]"), "gpt-5.5");
        assert_eq!(mk("gpt-5.3-codex"), "gpt-5.3-codex");
    }

    #[test]
    fn thinking_block_enables_reasoning() {
        let req = json!({
            "model": "gpt-5.5",
            "max_tokens": 10,
            "thinking": { "type": "enabled", "budget_tokens": 30000 },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let payload = anthropic_to_responses(&req, "d", &ModelMap::default()).unwrap();
        assert_eq!(payload["reasoning"]["effort"], "high");
    }

    #[test]
    fn reasoning_round_trips_through_thinking_block() {
        let response = json!({
            "id": "resp_1",
            "model": "gpt-5.5",
            "output": [
                { "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC",
                  "summary": [{ "type": "summary_text", "text": "thought" }] },
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hi" }] }
            ],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let original = json!({ "model": "claude-sonnet-4-5", "thinking": { "type": "enabled" } });
        let msg = responses_to_anthropic(&response, &original);
        assert_eq!(msg["content"][0]["type"], "thinking");
        assert_eq!(msg["content"][0]["thinking"], "thought");
        let sig = msg["content"][0]["signature"].as_str().unwrap();

        // Feed the emitted thinking block back; the reasoning item must reappear.
        let req = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 10,
            "messages": [{
                "role": "assistant",
                "content": [{ "type": "thinking", "thinking": "thought", "signature": sig }]
            }]
        });
        let payload = anthropic_to_responses(&req, "d", &ModelMap::default()).unwrap();
        let item = &payload["input"][0];
        assert_eq!(item["type"], "reasoning");
        assert_eq!(item["id"], "rs_1");
        assert_eq!(item["encrypted_content"], "ENC");
    }

    #[test]
    fn streams_text_then_tool_use() {
        let mut sm = StreamState::new("msg_test".into(), false);
        let events = [
            json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "Hi" }),
            json!({ "type": "response.output_text.done", "output_index": 0 }),
            json!({ "type": "response.output_item.added", "output_index": 1, "item": { "type": "function_call", "id": "fc1", "call_id": "call_1", "name": "go" } }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"x\":" }),
            json!({ "type": "response.function_call_arguments.done", "output_index": 1, "arguments": "{\"x\":1}" }),
            json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 2 } } }),
        ];
        let mut all = String::new();
        for e in events {
            let ev = SseEvent {
                event: "message".into(),
                data: e,
                done: false,
            };
            for f in map_stream_event(&ev, &mut sm) {
                all.push_str(&f);
            }
        }
        assert!(all.contains("\"type\":\"text_delta\""));
        assert!(all.contains("Hi"));
        assert!(all.contains("\"type\":\"input_json_delta\""));
        assert_eq!(sm.stop_reason, "tool_use");
        assert_eq!(sm.output_tokens, 2);
        // text block (0) opened, function block (1) opened
        assert_eq!(sm.next_index, 2);
    }
}
