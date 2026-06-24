use crate::error::AppError;
use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

/// A parsed SSE event. `data` holds the decoded JSON payload, or a JSON string
/// when the frame body was not valid JSON. `done` marks the `[DONE]` sentinel.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: Value,
    pub done: bool,
}

pub fn encode_sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {}\n\n", data)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Locate the earliest SSE frame separator. Returns (byte offset, separator length).
fn find_frame_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for sep in [b"\r\n\r\n".as_slice(), b"\n\n", b"\r\r"] {
        if let Some(idx) = find(buffer, sep) {
            match best {
                Some((b, _)) if b <= idx => {}
                _ => best = Some((idx, sep.len())),
            }
        }
    }
    best
}

fn parse_sse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = String::from("message");
    let mut data: Vec<String> = Vec::new();

    for line in frame.split(['\r', '\n']).filter(|l| !l.is_empty()) {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (
                &line[..i],
                line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..]),
            ),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => data.push(value.to_string()),
            _ => {}
        }
    }

    if data.is_empty() {
        return None;
    }

    let raw = data.join("\n");
    if raw == "[DONE]" {
        return Some(SseEvent {
            event,
            data: Value::Null,
            done: true,
        });
    }

    let value = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
    Some(SseEvent {
        event,
        data: value,
        done: false,
    })
}

/// Parse a byte stream (from reqwest) into a stream of SSE events.
pub fn parse_sse<S>(
    body: S,
    max_frame_bytes: usize,
) -> impl Stream<Item = Result<SseEvent, AppError>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    async_stream::try_stream! {
        futures_util::pin_mut!(body);
        let mut buffer: Vec<u8> = Vec::new();
        use futures_util::StreamExt;

        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(AppError::from)?;
            buffer.extend_from_slice(&chunk);
            if buffer.len() > max_frame_bytes {
                Err(AppError::new(502, "Upstream SSE frame is too large", "upstream_sse_error"))?;
            }

            while let Some((idx, len)) = find_frame_separator(&buffer) {
                let frame = String::from_utf8_lossy(&buffer[..idx]).into_owned();
                buffer.drain(..idx + len);
                if let Some(event) = parse_sse_frame(&frame) {
                    yield event;
                }
            }
        }

        let tail = String::from_utf8_lossy(&buffer);
        let tail = tail.trim();
        if !tail.is_empty() {
            if let Some(event) = parse_sse_frame(tail) {
                yield event;
            }
        }
    }
}
