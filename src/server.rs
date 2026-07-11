use crate::anthropic;
use crate::auth::CodexAuth;
use crate::config::{AuthMode, Config};
use crate::db::Db;
use crate::error::AppError;
use crate::models::list_models;
use crate::upstream::Upstream;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub upstream: Arc<Upstream>,
    pub auth: Option<Arc<CodexAuth>>,
    pub db: Arc<Db>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/auth/status", get(auth_status))
        .route("/auth/login", get(auth_login))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/device/start", get(auth_device_start))
        .route("/auth/device/poll", get(auth_device_poll))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/admin", get(dashboard))
        .route(
            "/admin/api/tokens",
            get(admin_list_tokens).post(admin_create_token),
        )
        .route(
            "/admin/api/tokens/{id}",
            patch(admin_update_token).delete(admin_delete_token),
        )
        .route("/admin/api/tokens/{id}/reset", post(admin_reset_token))
        .route("/admin/api/usage", get(admin_usage))
        .route("/admin/api/usage/summary", get(admin_usage_summary))
        .route("/admin/api/account", get(admin_account))
        .route("/admin/api/auth/status", get(admin_auth_status))
        .route("/admin/api/auth/login", post(admin_auth_login))
        .route("/admin/api/auth/device/start", post(admin_auth_device_start))
        .route("/admin/api/auth/device/poll", post(admin_auth_device_poll))
        .route("/admin/api/auth/import", post(admin_auth_import))
        .route("/admin/api/auth/refresh", post(admin_auth_refresh))
        .route("/admin/api/auth/logout", post(admin_auth_logout))
        .fallback(not_found)
        .layer(middleware::from_fn(cors))
        .layer(DefaultBodyLimit::max(state.config.max_body_bytes))
        .with_state(state)
}

// ---- middleware ----

fn set_cors(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,POST,OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type,x-api-key,authorization,anthropic-version"),
    );
}

async fn cors(req: axum::extract::Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        set_cors(res.headers_mut());
        return res;
    }
    let mut res = next.run(req).await;
    set_cors(res.headers_mut());
    res
}

// ---- local auth ----

fn require_local_auth(headers: &HeaderMap, proxy_api_key: &Option<String>) -> Result<(), AppError> {
    let Some(expected) = proxy_api_key else {
        return Ok(());
    };
    let x_api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if x_api_key == Some(expected.as_str()) || bearer == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(AppError::unauthorized("Invalid proxy API key"))
    }
}

fn require_local_auth_for_url(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    proxy_api_key: &Option<String>,
) -> Result<(), AppError> {
    let Some(expected) = proxy_api_key else {
        return Ok(());
    };
    if query.get("key").map(|s| s.as_str()) == Some(expected.as_str()) {
        return Ok(());
    }
    require_local_auth(headers, proxy_api_key)
}

fn header_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.to_string());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Authorizes a proxy client request. Returns the matched token id (None means the
/// legacy master key or fully open access). Per-token usage is attributed to this id.
fn authorize_client(headers: &HeaderMap, state: &AppState) -> Result<Option<i64>, AppError> {
    let presented = header_key(headers);
    if let Some(expected) = &state.config.proxy_api_key {
        if presented.as_deref() == Some(expected.as_str()) {
            return Ok(None);
        }
    }
    if let Some(key) = &presented {
        if let Some(id) = state.db.verify_token(key) {
            if state.db.token_over_limit(id) {
                return Err(AppError::new(
                    429,
                    "Token quota exhausted",
                    "rate_limit_error",
                ));
            }
            return Ok(Some(id));
        }
    }
    if state.config.proxy_api_key.is_none() && !state.db.has_tokens() {
        return Ok(None);
    }
    Err(AppError::unauthorized("Invalid proxy API key"))
}

fn require_admin(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    state: &AppState,
) -> Result<(), AppError> {
    let expected = state
        .config
        .admin_api_key
        .as_ref()
        .or(state.config.proxy_api_key.as_ref());
    let Some(expected) = expected else {
        return Err(AppError::new(
            403,
            "Admin API disabled: set ADMIN_API_KEY to manage tokens",
            "forbidden",
        ));
    };
    let presented = header_key(headers).or_else(|| query.get("key").cloned());
    if presented.as_deref() == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(AppError::unauthorized("Invalid admin API key"))
    }
}

fn log_trunc(tag: &str, v: &Value) {
    let s = v.to_string();
    let max = 4000;
    if s.len() > max {
        eprintln!("{tag} ({} bytes) {}…", s.len(), &s[..max]);
    } else {
        eprintln!("{tag} {s}");
    }
}

fn usage_from_response(response: &Value) -> (i64, i64, i64) {
    let u = response.get("usage");
    let input = u
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = u
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let reasoning = u
        .and_then(|u| u.get("output_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    (input, output, reasoning)
}

// ---- handlers ----

async fn health(State(state): State<AppState>) -> Response {
    let upstream = if state.config.auth_mode == AuthMode::Codex {
        &state.config.codex_api_base
    } else {
        &state.config.openai_api_base
    };
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "auth_mode": if state.config.auth_mode == AuthMode::Codex { "codex" } else { "api-key" },
        "upstream": upstream,
        "upstream_proxy": state.config.upstream_proxy_url,
    }))
    .into_response()
}

async fn auth_status_value(state: &AppState) -> Value {
    match &state.auth {
        Some(auth) => auth.status().await,
        None => json!({
            "authenticated": state.config.openai_api_key.is_some(),
            "auth_mode": "api-key",
            "account_id": null,
            "access_token_expires_at": null,
            "has_refresh_token": false,
            "last_refresh": null,
        }),
    }
}

async fn web_login_payload(state: &AppState) -> Result<Value, AppError> {
    let Some(auth) = &state.auth else {
        return Err(AppError::bad_request(
            "Web login is only available when OPENAI_AUTH_MODE=codex",
        ));
    };
    let redirect_uri = format!(
        "http://{}:{}/auth/callback",
        state.config.codex_oauth_redirect_host, state.config.port
    );
    let login = auth.start_login(redirect_uri.clone()).await;
    Ok(json!({
        "authorization_url": login.authorization_url,
        "expires_at": login.expires_at,
        "callback_url": redirect_uri,
    }))
}

async fn auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_local_auth_for_url(&headers, &query, &state.config.proxy_api_key) {
        return e.into_openai_response();
    }
    Json(auth_status_value(&state).await).into_response()
}

async fn auth_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_local_auth_for_url(&headers, &query, &state.config.proxy_api_key) {
        return e.into_openai_response();
    }
    match web_login_payload(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn auth_callback(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Web login is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };

    if let Some(err) = query.get("error") {
        let desc = query.get("error_description").unwrap_or(err);
        return AppError::new(400, desc.clone(), "authentication_error").into_openai_response();
    }

    let (Some(code), Some(st)) = (query.get("code"), query.get("state")) else {
        return AppError::bad_request("OAuth callback requires code and state")
            .into_openai_response();
    };

    match auth.finish_login(code, st).await {
        Ok(status) => {
            let mut body = json!({
                "ok": true,
                "message": "Codex OAuth login complete. You can close this tab.",
            });
            if let Some(obj) = status.as_object() {
                for (k, v) in obj {
                    body[k] = v.clone();
                }
            }
            Json(body).into_response()
        }
        Err(e) => e.into_openai_response(),
    }
}

async fn auth_device_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_local_auth_for_url(&headers, &query, &state.config.proxy_api_key) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Device login is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    match auth.start_device_login().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn auth_device_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_local_auth_for_url(&headers, &query, &state.config.proxy_api_key) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Device login is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    let (Some(device_auth_id), Some(user_code)) =
        (query.get("device_auth_id"), query.get("user_code"))
    else {
        return AppError::bad_request("device_auth_id and user_code are required")
            .into_openai_response();
    };
    match auth.poll_device_login(device_auth_id, user_code).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn models(State(state): State<AppState>) -> Response {
    let data = list_models(&state.upstream).await;
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn not_found(method: Method, uri: axum::http::Uri) -> Response {
    AppError::new(
        404,
        format!("No route for {method} {}", uri.path()),
        "not_found_error",
    )
    .into_openai_response()
}

// ---- proxy-handled usage command ----

fn usage_command_matches(text: &str, config: &Config) -> bool {
    let t = text.trim();
    t == config.usage_command || matches!(t.to_lowercase().as_str(), "/usage" | "ccusage" | "/用量")
}

fn last_text_blocks(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    let mut s = String::new();
    for b in arr {
        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
            s.push_str(t);
        }
    }
    Some(s)
}

/// Last user message text from an Anthropic `/v1/messages` request.
fn last_user_text_anthropic(request: &Value) -> Option<String> {
    let messages = request.get("messages")?.as_array()?;
    let last = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|v| v.as_str()) != Some("assistant"))?;
    last_text_blocks(last.get("content")?)
}

/// Last user message text from a Responses `/v1/responses` request.
fn last_user_text_responses(request: &Value) -> Option<String> {
    let input = request.get("input")?;
    if let Some(s) = input.as_str() {
        return Some(s.to_string());
    }
    let last = input
        .as_array()?
        .iter()
        .rev()
        .find(|it| it.get("role").and_then(|v| v.as_str()) == Some("user"))?;
    last_text_blocks(last.get("content")?)
}

async fn usage_report(state: &AppState, token_id: Option<i64>) -> String {
    let tokens = state.db.list_tokens().unwrap_or_default();
    let account = state
        .upstream
        .account_snapshot()
        .unwrap_or_else(|| json!({}));
    crate::usage::build_report(&tokens, token_id, &account)
}

/// A synthetic Responses API reply (output + optional SSE) carrying the text.
/// The streaming form mirrors the real event sequence Codex CLI expects.
fn responses_synthetic(model: &Value, text: &str, stream: bool) -> Response {
    let resp_id = format!("resp_{}", uuid_like());
    let msg_id = format!("msg_{}", uuid_like());
    let message_item = json!({
        "id": msg_id, "type": "message", "status": "completed", "role": "assistant",
        "content": [{ "type": "output_text", "annotations": [], "text": text }],
    });
    let final_response = json!({
        "id": resp_id, "object": "response", "status": "completed", "model": model,
        "output": [message_item.clone()],
        "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 },
    });
    if !stream {
        return Json(final_response).into_response();
    }
    let frames = vec![
        sse_event(
            "response.created",
            &json!({ "type": "response.created", "response": {
                "id": resp_id, "object": "response", "status": "in_progress", "model": model, "output": [] } }),
        ),
        sse_event(
            "response.output_item.added",
            &json!({ "type": "response.output_item.added", "output_index": 0,
                "item": { "id": msg_id, "type": "message", "status": "in_progress", "role": "assistant", "content": [] } }),
        ),
        sse_event(
            "response.content_part.added",
            &json!({ "type": "response.content_part.added", "output_index": 0, "content_index": 0,
                "item_id": msg_id, "part": { "type": "output_text", "annotations": [], "text": "" } }),
        ),
        sse_event(
            "response.output_text.delta",
            &json!({ "type": "response.output_text.delta", "output_index": 0, "content_index": 0,
                "item_id": msg_id, "delta": text }),
        ),
        sse_event(
            "response.output_text.done",
            &json!({ "type": "response.output_text.done", "output_index": 0, "content_index": 0,
                "item_id": msg_id, "text": text }),
        ),
        sse_event(
            "response.content_part.done",
            &json!({ "type": "response.content_part.done", "output_index": 0, "content_index": 0,
                "item_id": msg_id, "part": { "type": "output_text", "annotations": [], "text": text } }),
        ),
        sse_event(
            "response.output_item.done",
            &json!({ "type": "response.output_item.done", "output_index": 0, "item": message_item }),
        ),
        sse_event(
            "response.completed",
            &json!({ "type": "response.completed", "response": final_response }),
        ),
    ];
    let body = async_stream::stream! {
        for f in frames { yield Ok::<Bytes, std::io::Error>(Bytes::from(f)); }
    };
    sse_response(Body::from_stream(body))
}

fn sse_event(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:032x}")
}

// ---- /v1/messages (Anthropic) ----

async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let token_id = match authorize_client(&headers, &state) {
        Ok(id) => id,
        Err(e) => return e.into_anthropic_response(),
    };
    let request = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_anthropic_response(),
    };
    match messages_inner(state, request, token_id).await {
        Ok(r) => r,
        Err(e) => e.into_anthropic_response(),
    }
}

async fn messages_inner(
    state: AppState,
    request: Value,
    token_id: Option<i64>,
) -> Result<Response, AppError> {
    let stream = request
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let model = request.get("model").cloned().unwrap_or(Value::Null);

    // Proxy-handled usage command: answer locally without calling the upstream.
    if last_user_text_anthropic(&request)
        .map(|t| usage_command_matches(&t, &state.config))
        .unwrap_or(false)
    {
        let report = usage_report(&state, token_id).await;
        if stream {
            let frames = anthropic::synthetic_stream(&model, &report);
            let body = async_stream::stream! {
                for f in frames { yield Ok::<Bytes, std::io::Error>(Bytes::from(f)); }
            };
            return Ok(sse_response(Body::from_stream(body)));
        }
        return Ok(Json(anthropic::synthetic_message(&model, &report)).into_response());
    }

    let payload = anthropic::anthropic_to_responses(
        &request,
        &state.config.default_instructions,
        &state.config.model_map,
    )?;
    let emit_thinking = anthropic::thinking_enabled(&request);
    let model_str = model.as_str().map(|s| s.to_string());
    let log_up = state.config.log_upstream;
    if log_up {
        log_trunc("[ccproxy→codex payload]", &payload);
    }

    if !stream {
        let response = state.upstream.create_response(payload).await?;
        if log_up {
            log_trunc("[codex→ccproxy response]", &response);
        }
        let (input, output, reasoning) = usage_from_response(&response);
        state.db.record_usage(
            token_id,
            model_str.as_deref(),
            input,
            output,
            reasoning,
            false,
            "/v1/messages",
            200,
        );
        return Ok(Json(anthropic::responses_to_anthropic(&response, &request)).into_response());
    }

    let events = state.upstream.stream_response(payload).await?;
    let idle_ms = state.config.stream_idle_timeout_ms;
    let message_id = anthropic::new_message_id();
    let db = state.db.clone();

    let sse = async_stream::stream! {
        let mut sm = anthropic::StreamState::new(message_id.clone(), emit_thinking);
        yield Ok::<Bytes, std::io::Error>(Bytes::from(anthropic::message_start_frame(&message_id, &model)));

        futures_util::pin_mut!(events);
        loop {
            let next = if idle_ms > 0 {
                match tokio::time::timeout(Duration::from_millis(idle_ms), events.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        yield Ok(Bytes::from(anthropic::error_frame("upstream_timeout", "Upstream stream timed out")));
                        return;
                    }
                }
            } else {
                events.next().await
            };

            let Some(item) = next else { break };
            match item {
                Ok(event) => {
                    if log_up {
                        log_trunc("[codex→ccproxy event]", &event.data);
                    }
                    for frame in anthropic::map_stream_event(&event, &mut sm) {
                        yield Ok(Bytes::from(frame));
                    }
                    if sm.stream_errored {
                        return;
                    }
                }
                Err(e) => {
                    yield Ok(Bytes::from(anthropic::error_frame(e.anthropic_error_type(), &e.message)));
                    return;
                }
            }
        }

        for frame in anthropic::finish_open_blocks(&mut sm) {
            yield Ok(Bytes::from(frame));
        }
        yield Ok(Bytes::from(anthropic::message_delta_frame(&sm)));
        yield Ok(Bytes::from(anthropic::message_stop_frame()));
        db.record_usage(
            token_id,
            model_str.as_deref(),
            sm.input_tokens,
            sm.output_tokens,
            sm.reasoning_tokens,
            true,
            "/v1/messages",
            200,
        );
    };

    Ok(sse_response(Body::from_stream(sse)))
}

// ---- /v1/responses (native Codex/OpenAI passthrough) ----

async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let token_id = match authorize_client(&headers, &state) {
        Ok(id) => id,
        Err(e) => return e.into_openai_response(),
    };
    let request = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    match responses_inner(state, request, token_id).await {
        Ok(r) => r,
        Err(e) => e.into_openai_response(),
    }
}

async fn responses_inner(
    state: AppState,
    request: Value,
    token_id: Option<i64>,
) -> Result<Response, AppError> {
    let model_str = request
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let stream_flag = request
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Proxy-handled usage command: answer locally without calling the upstream.
    if last_user_text_responses(&request)
        .map(|t| usage_command_matches(&t, &state.config))
        .unwrap_or(false)
    {
        let report = usage_report(&state, token_id).await;
        let model = request.get("model").cloned().unwrap_or(Value::Null);
        return Ok(responses_synthetic(&model, &report, stream_flag));
    }

    let upstream = state.upstream.passthrough(request).await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));

    let db = state.db.clone();
    let status_code = status.as_u16();
    // Tee the upstream body through to the client while buffering it (capped) so
    // we can parse the final usage object and record per-token consumption.
    const USAGE_BUF_CAP: usize = 8 * 1024 * 1024;
    let body_stream = async_stream::stream! {
        let stream = upstream.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut buf: Vec<u8> = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if !truncated && buf.len() + bytes.len() <= USAGE_BUF_CAP {
                        buf.extend_from_slice(&bytes);
                    } else {
                        truncated = true;
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(e) => {
                    yield Err(std::io::Error::other(e));
                    break;
                }
            }
        }
        let (input, output, reasoning) = extract_usage(&buf);
        db.record_usage(
            token_id,
            model_str.as_deref(),
            input,
            output,
            reasoning,
            stream_flag,
            "/v1/responses",
            status_code,
        );
    };

    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(body_stream))
        .unwrap();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

/// Extracts (input, output, reasoning) token counts from a Responses API body,
/// handling both a single JSON object and a streamed SSE feed.
fn extract_usage(bytes: &[u8]) -> (i64, i64, i64) {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        let obj = v.get("response").unwrap_or(&v);
        return usage_from_response(obj);
    }
    let text = String::from_utf8_lossy(bytes);
    let mut last = (0, 0, 0);
    for line in text.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            let obj = v.get("response").unwrap_or(&v);
            if obj.get("usage").is_some() {
                last = usage_from_response(obj);
            }
        }
    }
    last
}

// ---- admin / token management ----

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

async fn dashboard(State(state): State<AppState>) -> Response {
    // When DASHBOARD_PATH is set, serve that file live so the admin page can be
    // tweaked without recompiling; otherwise fall back to the embedded copy.
    if let Some(path) = &state.config.dashboard_path {
        if let Ok(html) = tokio::fs::read_to_string(path).await {
            return Html(html).into_response();
        }
    }
    Html(DASHBOARD_HTML).into_response()
}

async fn admin_list_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    match state.db.list_tokens() {
        Ok(tokens) => Json(tokens).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_create_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    let name = req
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return AppError::bad_request("name is required").into_openai_response();
    }
    let note = req
        .get("note")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let limit = req.get("limit").and_then(|v| v.as_i64());
    let window = req.get("window_days").and_then(|v| v.as_i64());
    match state.db.create_token(name, note, limit, window) {
        Ok(token) => (StatusCode::CREATED, Json(token)).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_update_token(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    let name = req.get("name").and_then(|v| v.as_str());
    let note = req.get("note").and_then(|v| v.as_str());
    let disabled = req.get("disabled").and_then(|v| v.as_bool());
    // Tri-state: key absent = leave as-is, null = clear, number = set.
    let limit = req.get("limit").map(|v| v.as_i64());
    let window = req.get("window_days").map(|v| v.as_i64());
    match state
        .db
        .update_token(id, name, note, disabled, limit, window)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_reset_token(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    match state.db.reset_token_usage(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_delete_token(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    match state.db.delete_token(id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let token_id = query.get("token_id").and_then(|v| v.parse::<i64>().ok());
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200);
    match state.db.list_usage(token_id, limit) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_usage_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    // Absolute epoch-ms bounds; from=0 means from the beginning, to=0 means up to now.
    let from = query.get("from").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let to = query.get("to").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    match state.db.usage_summary(from, to) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let snapshot = state
        .upstream
        .account_snapshot()
        .unwrap_or_else(|| json!({ "captured_at": null, "headers": {} }));
    Json(snapshot).into_response()
}

async fn admin_auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    Json(auth_status_value(&state).await).into_response()
}

async fn admin_auth_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    match web_login_payload(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_auth_device_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Device login is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    match auth.start_device_login().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_auth_device_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Device login is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    let (Some(device_auth_id), Some(user_code)) = (
        req.get("device_auth_id").and_then(|v| v.as_str()),
        req.get("user_code").and_then(|v| v.as_str()),
    ) else {
        return AppError::bad_request("device_auth_id and user_code are required")
            .into_openai_response();
    };
    match auth.poll_device_login(device_auth_id, user_code).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_auth_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Auth import is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    let value = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return e.into_openai_response(),
    };
    match auth.import_auth(value).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_auth_refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Token refresh is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    match auth.refresh().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_openai_response(),
    }
}

async fn admin_auth_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&headers, &query, &state) {
        return e.into_openai_response();
    }
    let Some(auth) = &state.auth else {
        return AppError::bad_request("Logout is only available when OPENAI_AUTH_MODE=codex")
            .into_openai_response();
    };
    match auth.logout().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_openai_response(),
    }
}

// ---- helpers ----

fn parse_body(body: &Bytes) -> Result<Value, AppError> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body)
        .map_err(|e| AppError::bad_request(format!("Invalid JSON body: {e}")))
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header(header::CONNECTION, "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}
