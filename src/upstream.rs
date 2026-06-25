use crate::auth::CodexAuth;
use crate::config::{AuthMode, Config};
use crate::error::AppError;
use crate::sse::{parse_sse, SseEvent};
use futures_util::StreamExt;
use rand::RngCore;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub struct Upstream {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub auth: Option<Arc<CodexAuth>>,
    /// Stable per-process session id sent to the Codex backend; lets ChatGPT's
    /// prompt cache stay warm across requests from this proxy instance.
    session_id: String,
}

pub struct Prepared {
    pub url: String,
    pub headers: HeaderMap,
}

fn random_uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

impl Upstream {
    pub fn new(config: Arc<Config>, http: reqwest::Client, auth: Option<Arc<CodexAuth>>) -> Self {
        Upstream {
            config,
            http,
            auth,
            session_id: random_uuid_v4(),
        }
    }

    fn is_codex(&self) -> bool {
        self.config.auth_mode == AuthMode::Codex
    }

    pub async fn prepare(&self, stream: bool) -> Result<Prepared, AppError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, HeaderValue::from_static("ccproxy/0.1.0"));
        if stream {
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        }
        for (k, v) in &self.config.extra_headers {
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                headers.insert(name, val);
            }
        }

        match self.config.auth_mode {
            AuthMode::ApiKey => {
                let key = self.config.openai_api_key.as_ref().ok_or_else(|| {
                    AppError::config("OPENAI_API_KEY is required when OPENAI_AUTH_MODE=api-key")
                })?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {key}"))
                        .map_err(|_| AppError::config("Invalid OPENAI_API_KEY"))?,
                );
                Ok(Prepared {
                    url: format!("{}/responses", self.config.openai_api_base.trim_end_matches('/')),
                    headers,
                })
            }
            AuthMode::Codex => {
                let auth = self
                    .auth
                    .as_ref()
                    .ok_or_else(|| AppError::config("Codex auth manager is not configured"))?;
                let bearer = auth.get_bearer().await?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", bearer.access_token))
                        .map_err(|_| AppError::config("Invalid access token"))?,
                );
                if let Some(account_id) = bearer.account_id {
                    if let Ok(val) = HeaderValue::from_str(&account_id) {
                        headers.insert(HeaderName::from_static("chatgpt-account-id"), val);
                    }
                }
                // Mimic the real Codex CLI so the ChatGPT backend treats us as a
                // first-class client (avoids unknown-originator throttling and
                // keeps prompt caching keyed to a stable session).
                headers.insert(HeaderName::from_static("originator"), HeaderValue::from_static("codex_cli_rs"));
                if let Ok(val) = HeaderValue::from_str(&self.session_id) {
                    headers.insert(HeaderName::from_static("session_id"), val);
                }
                if let Ok(val) = HeaderValue::from_str(&self.config.codex_client_version) {
                    headers.insert(HeaderName::from_static("version"), val);
                }
                if let Ok(val) = HeaderValue::from_str(&format!(
                    "codex_cli_rs/{} (ccproxy)",
                    self.config.codex_client_version
                )) {
                    headers.insert(USER_AGENT, val);
                }
                Ok(Prepared {
                    url: format!("{}/responses", self.config.codex_api_base.trim_end_matches('/')),
                    headers,
                })
            }
        }
    }

    fn shape_payload(&self, mut payload: Value) -> Value {
        if self.is_codex() {
            if let Some(obj) = payload.as_object_mut() {
                obj.remove("max_output_tokens");
                // With store:false the only way to carry reasoning across turns is
                // to ask the backend to return encrypted reasoning content.
                if obj.contains_key("reasoning") {
                    obj.insert(
                        "include".into(),
                        json!(["reasoning.encrypted_content"]),
                    );
                }
            }
        }
        payload
    }

    /// Non-streaming Responses API call. For Codex auth the upstream is always
    /// streamed and collected into a single response object.
    pub async fn create_response(&self, payload: Value) -> Result<Value, AppError> {
        let upstream_stream = self.is_codex();
        let prepared = self.prepare(upstream_stream).await?;
        let mut body = self.shape_payload(payload);
        body["stream"] = json!(upstream_stream);

        let resp = self
            .http
            .post(&prepared.url)
            .headers(prepared.headers)
            .timeout(Duration::from_millis(self.config.request_timeout_ms))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(upstream_error(resp).await);
        }

        if upstream_stream {
            let events = parse_sse(resp.bytes_stream(), self.config.sse_max_frame_bytes);
            collect_streamed_response(events).await
        } else {
            resp.json::<Value>()
                .await
                .map_err(|e| AppError::new(502, format!("Invalid upstream JSON: {e}"), "upstream_error"))
        }
    }

    /// Streaming Responses API call. Returns the parsed SSE event stream.
    pub async fn stream_response(
        &self,
        payload: Value,
    ) -> Result<impl futures_util::Stream<Item = Result<SseEvent, AppError>>, AppError> {
        let prepared = self.prepare(true).await?;
        let mut body = self.shape_payload(payload);
        body["stream"] = json!(true);

        let resp = self
            .http
            .post(&prepared.url)
            .headers(prepared.headers)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(upstream_error(resp).await);
        }

        Ok(parse_sse(resp.bytes_stream(), self.config.sse_max_frame_bytes))
    }

    /// Raw passthrough for the native Codex/OpenAI Responses path. Forwards the
    /// client payload verbatim (only injecting auth) and returns the upstream
    /// response so the caller can relay status and body unchanged.
    pub async fn passthrough(&self, payload: Value) -> Result<reqwest::Response, AppError> {
        let stream = payload
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let prepared = self.prepare(stream).await?;

        let resp = self
            .http
            .post(&prepared.url)
            .headers(prepared.headers)
            .json(&payload)
            .send()
            .await?;

        Ok(resp)
    }
}

async fn upstream_error(resp: reqwest::Response) -> AppError {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let message = if text.is_empty() {
        format!("Upstream returned HTTP {status}")
    } else {
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|j| {
                j.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .or_else(|| j.get("message").and_then(|v| v.as_str()))
                    .or_else(|| j.get("error_description").and_then(|v| v.as_str()))
                    .or_else(|| j.get("error").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
            })
            .unwrap_or(text)
    };
    AppError::upstream(status, message)
}

/// Collect a streamed Responses API SSE feed into a single response object,
/// mirroring the Node implementation's `collectStreamedResponse`.
async fn collect_streamed_response<S>(events: S) -> Result<Value, AppError>
where
    S: futures_util::Stream<Item = Result<SseEvent, AppError>>,
{
    futures_util::pin_mut!(events);

    let mut completed: Option<Value> = None;
    let mut id: Option<String> = None;
    let mut model: Option<String> = None;
    let mut text = String::new();
    let mut usage = Value::Object(Default::default());
    // Preserve insertion order of function calls.
    let mut fn_keys: Vec<String> = Vec::new();
    let mut fn_calls: std::collections::HashMap<String, Value> = std::collections::HashMap::new();

    let upsert = |keys: &mut Vec<String>,
                  calls: &mut std::collections::HashMap<String, Value>,
                  key: String,
                  value: Value| {
        if !calls.contains_key(&key) {
            keys.push(key.clone());
        }
        calls.insert(key, value);
    };

    while let Some(event) = events.next().await {
        let event = event?;
        if event.done {
            continue;
        }
        let data = &event.data;
        let typ = data
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or(&event.event)
            .to_string();

        match typ.as_str() {
            "response.created" => {
                if let Some(r) = data.get("response") {
                    id = r.get("id").and_then(|v| v.as_str()).map(String::from).or(id);
                    model = r.get("model").and_then(|v| v.as_str()).map(String::from).or(model);
                }
            }
            "response.output_text.delta" => {
                if let Some(d) = data.get("delta").and_then(|v| v.as_str()) {
                    text.push_str(d);
                }
            }
            "response.output_text.done" => {
                if text.is_empty() {
                    if let Some(t) = data.get("text").and_then(|v| v.as_str()) {
                        text = t.to_string();
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = data.get("item") {
                    if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                        let key = fn_key(data, item);
                        upsert(
                            &mut fn_keys,
                            &mut fn_calls,
                            key,
                            json!({
                                "type": "function_call",
                                "id": item.get("id").cloned().unwrap_or(Value::Null),
                                "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                                "name": item.get("name").cloned().unwrap_or(Value::Null),
                                "arguments": item.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            }),
                        );
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let key = output_key(data);
                let mut current = fn_calls.get(&key).cloned().unwrap_or_else(|| {
                    json!({
                        "type": "function_call",
                        "id": data.get("item_id").cloned().unwrap_or(Value::Null),
                        "call_id": data.get("call_id").cloned().unwrap_or(Value::Null),
                        "name": data.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": "",
                    })
                });
                let prev = current.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                let delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                current["arguments"] = json!(format!("{prev}{delta}"));
                upsert(&mut fn_keys, &mut fn_calls, key, current);
            }
            "response.function_call_arguments.done" => {
                let key = output_key(data);
                let mut current = fn_calls.get(&key).cloned().unwrap_or_else(|| {
                    json!({
                        "type": "function_call",
                        "id": data.get("item_id").cloned().unwrap_or(Value::Null),
                        "call_id": data.get("call_id").cloned().unwrap_or(Value::Null),
                        "name": data.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": "",
                    })
                });
                let existing = current.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                if existing.is_empty() {
                    if let Some(a) = data.get("arguments").and_then(|v| v.as_str()) {
                        current["arguments"] = json!(a);
                    }
                }
                upsert(&mut fn_keys, &mut fn_calls, key, current);
            }
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                        let key = fn_key(data, item);
                        let current = fn_calls.get(&key).cloned().unwrap_or(json!({ "type": "function_call" }));
                        let args = current
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .or_else(|| item.get("arguments").and_then(|v| v.as_str()).map(String::from))
                            .unwrap_or_default();
                        upsert(
                            &mut fn_keys,
                            &mut fn_calls,
                            key,
                            json!({
                                "type": "function_call",
                                "id": item.get("id").cloned().or_else(|| current.get("id").cloned()).unwrap_or(Value::Null),
                                "call_id": item.get("call_id").cloned().or_else(|| current.get("call_id").cloned()).unwrap_or(Value::Null),
                                "name": item.get("name").cloned().or_else(|| current.get("name").cloned()).unwrap_or(Value::Null),
                                "arguments": args,
                            }),
                        );
                    }
                }
            }
            "response.completed" | "response.done" => {
                if data.get("response").is_some() || data.get("id").is_some() {
                    let resp = data.get("response").cloned().unwrap_or_else(|| data.clone());
                    if let Some(u) = resp.get("usage") {
                        usage = u.clone();
                    }
                    id = resp.get("id").and_then(|v| v.as_str()).map(String::from).or(id);
                    model = resp.get("model").and_then(|v| v.as_str()).map(String::from).or(model);
                    completed = Some(resp);
                }
            }
            _ => {}
        }
    }

    let function_items: Vec<Value> = fn_keys.iter().filter_map(|k| fn_calls.get(k).cloned()).collect();

    if let Some(mut resp) = completed {
        if !has_output(&resp) {
            let mut output = text_output_items(&text);
            output.extend(function_items);
            resp["output"] = Value::Array(output);
        } else {
            if !has_text_output(&resp) && !text.is_empty() {
                let mut output = text_output_items(&text);
                if let Some(arr) = resp.get("output").and_then(|v| v.as_array()) {
                    output.extend(arr.iter().cloned());
                }
                resp["output"] = Value::Array(output);
            }
            if !has_function_output(&resp) && !function_items.is_empty() {
                let mut output = resp
                    .get("output")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                output.extend(function_items);
                resp["output"] = Value::Array(output);
            }
        }
        return Ok(resp);
    }

    let mut output = text_output_items(&text);
    output.extend(function_items);
    Ok(json!({
        "id": id,
        "model": model,
        "output": output,
        "usage": usage,
    }))
}

fn output_key(data: &Value) -> String {
    if let Some(i) = data.get("output_index").and_then(|v| v.as_i64()) {
        return i.to_string();
    }
    data.get("item_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default()
}

fn fn_key(data: &Value, item: &Value) -> String {
    if let Some(i) = data.get("output_index").and_then(|v| v.as_i64()) {
        return i.to_string();
    }
    item.get("id")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("call_id").and_then(|v| v.as_str()))
        .map(String::from)
        .unwrap_or_default()
}

fn has_output(resp: &Value) -> bool {
    resp.get("output").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false)
}

fn has_text_output(resp: &Value) -> bool {
    resp.get("output").and_then(|v| v.as_array()).map(|items| {
        items.iter().any(|item| {
            item.get("content").and_then(|c| c.as_array()).map(|content| {
                content.iter().any(|c| {
                    matches!(c.get("type").and_then(|v| v.as_str()), Some("output_text") | Some("text"))
                })
            }).unwrap_or(false)
        })
    }).unwrap_or(false)
}

fn has_function_output(resp: &Value) -> bool {
    resp.get("output").and_then(|v| v.as_array()).map(|items| {
        items.iter().any(|item| item.get("type").and_then(|v| v.as_str()) == Some("function_call"))
    }).unwrap_or(false)
}

fn text_output_items(text: &str) -> Vec<Value> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }],
    })]
}
