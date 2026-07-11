use crate::config::{AuthMode, Config};
use crate::upstream::Upstream;
use serde_json::{json, Value};
use std::time::Duration;

const REASONING_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

fn static_models() -> Vec<Value> {
    [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
        "gpt-5.2",
    ]
    .iter()
    .map(|id| json!({ "id": id, "object": "model", "owned_by": "openai" }))
    .collect()
}

pub async fn list_models(upstream: &Upstream) -> Vec<Value> {
    let config = &upstream.config;

    if config.auth_mode == AuthMode::Codex {
        if let Some(live) = list_codex_models(upstream).await {
            if !live.is_empty() {
                return live;
            }
        }
    }

    if let Some(cached) = list_cached_models(config).await {
        if !cached.is_empty() {
            return cached;
        }
    }

    static_models()
}

async fn list_codex_models(upstream: &Upstream) -> Option<Vec<Value>> {
    let auth = upstream.auth.as_ref()?;
    let bearer = auth.get_bearer().await.ok()?;
    let config = &upstream.config;

    let mut url = url::Url::parse(&format!(
        "{}/models",
        config.codex_api_base.trim_end_matches('/')
    ))
    .ok()?;
    url.query_pairs_mut()
        .append_pair("client_version", &upstream.codex_client_version());

    let mut req = upstream
        .http
        .get(url)
        .timeout(Duration::from_millis(config.model_list_timeout_ms))
        .header("authorization", format!("Bearer {}", bearer.access_token));
    if let Some(account_id) = bearer.account_id {
        req = req.header("chatgpt-account-id", account_id);
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return Some(Vec::new());
    }
    let json: Value = resp.json().await.ok()?;
    Some(normalize_models(json.get("models")))
}

async fn list_cached_models(config: &Config) -> Option<Vec<Value>> {
    let path = config
        .codex_auth_file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("models_cache.json");
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    let models = json.get("models").or(Some(&json));
    Some(normalize_models(models))
}

fn normalize_models(models: Option<&Value>) -> Vec<Value> {
    let Some(arr) = models.and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let normalized: Vec<Value> = arr
        .iter()
        .filter(|m| m.get("slug").is_some() || m.get("id").is_some())
        .filter(|m| m.get("supported_in_api").and_then(|v| v.as_bool()) != Some(false))
        .filter(|m| m.get("visibility").and_then(|v| v.as_str()) != Some("hide"))
        .map(|m| {
            let id = m
                .get("slug")
                .or_else(|| m.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let levels = m
                .get("supported_reasoning_levels")
                .and_then(|v| v.as_array())
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|l| {
                            l.get("effort")
                                .and_then(|v| v.as_str())
                                .or_else(|| l.as_str())
                                .map(String::from)
                        })
                        .collect::<Vec<_>>()
                });

            let mut model = json!({
                "id": id,
                "object": "model",
                "owned_by": "openai",
                "display_name": m.get("display_name").cloned().unwrap_or(Value::Null),
                "description": m.get("description").cloned().unwrap_or(Value::Null),
                "context_window": m.get("context_window").cloned().unwrap_or(Value::Null),
                "default_reasoning_level": m.get("default_reasoning_level").cloned().unwrap_or(Value::Null),
            });
            if let Some(levels) = levels {
                model["supported_reasoning_levels"] = json!(levels);
            }
            model
        })
        .collect();

    with_reasoning_aliases(normalized)
}

fn with_reasoning_aliases(models: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for model in models {
        let id = model
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let display = model
            .get("display_name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| id.clone());
        let efforts: Vec<String> = model
            .get("supported_reasoning_levels")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| REASONING_EFFORTS.iter().map(|s| s.to_string()).collect());

        out.push(model.clone());
        for effort in efforts {
            let mut alias = model.clone();
            alias["id"] = json!(format!("{id}-{effort}"));
            alias["display_name"] = json!(format!("{display} {effort}"));
            alias["reasoning_effort"] = json!(effort);
            alias["canonical_model"] = json!(id);
            out.push(alias);
        }
    }
    out
}
