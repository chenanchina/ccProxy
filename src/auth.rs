use crate::config::Config;
use crate::error::AppError;
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub struct Bearer {
    pub access_token: String,
    pub account_id: Option<String>,
}

struct PendingLogin {
    code_verifier: String,
    redirect_uri: String,
    created_at_ms: u128,
}

pub struct LoginStart {
    pub authorization_url: String,
    pub expires_at: String,
}

pub struct CodexAuth {
    config: Arc<Config>,
    http: reqwest::Client,
    refresh_lock: Mutex<()>,
    pending: Mutex<HashMap<String, PendingLogin>>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn iso8601(ms: u128) -> String {
    // Minimal RFC3339 formatter (UTC) without pulling in chrono.
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        y, m, d, hour, minute, second
    )
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256_base64url(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    base64url(&hasher.finalize())
}

fn random_base64url(byte_len: usize) -> String {
    let mut bytes = vec![0u8; byte_len];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64url(&bytes)
}

fn decode_jwt_exp(token: &str) -> Option<i64> {
    let part = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(part))
        .ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    payload.get("exp").and_then(|v| v.as_i64())
}

fn token_expires_soon(token: &str, skew_seconds: i64) -> bool {
    match decode_jwt_exp(token) {
        Some(exp) => exp * 1000 <= now_ms() as i64 + skew_seconds * 1000,
        None => false,
    }
}

async fn read_auth_file(path: &Path) -> Result<Value, AppError> {
    let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
        AppError::new(
            500,
            format!("Unable to read Codex auth file at {}: {e}", path.display()),
            "configuration_error",
        )
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        AppError::new(
            500,
            format!("Unable to parse Codex auth file at {}: {e}", path.display()),
            "configuration_error",
        )
    })
}

async fn write_auth_file(path: &Path, auth: &Value) -> Result<(), AppError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".auth.json.{}.{}.tmp",
        std::process::id(),
        now_ms()
    ));
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(auth).unwrap_or_default()
    );

    tokio::fs::write(&tmp, body)
        .await
        .map_err(|e| AppError::config(format!("Unable to write Codex auth file: {e}")))?;
    set_mode_600(&tmp).await;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| AppError::config(format!("Unable to replace Codex auth file: {e}")))?;
    set_mode_600(path).await;
    Ok(())
}

#[cfg(unix)]
async fn set_mode_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
}

#[cfg(not(unix))]
async fn set_mode_600(_path: &Path) {}

fn token_summary(tokens: &Value) -> Value {
    let access = tokens.get("access_token").and_then(|v| v.as_str());
    let expires_at = access
        .and_then(decode_jwt_exp)
        .map(|exp| iso8601((exp as u128) * 1000));
    json!({
        "authenticated": access.is_some(),
        "account_id": tokens.get("account_id").cloned().unwrap_or(Value::Null),
        "access_token_expires_at": expires_at,
        "has_refresh_token": tokens.get("refresh_token").and_then(|v| v.as_str()).is_some(),
    })
}

impl CodexAuth {
    pub fn new(config: Arc<Config>, http: reqwest::Client) -> Self {
        CodexAuth {
            config,
            http,
            refresh_lock: Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_bearer(&self) -> Result<Bearer, AppError> {
        let auth = read_auth_file(&self.config.codex_auth_file).await?;
        let access = auth
            .get("tokens")
            .and_then(|t| t.get("access_token"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::config("Codex auth file does not contain tokens.access_token")
            })?;

        if !token_expires_soon(access, 90) {
            return Ok(Bearer {
                access_token: access.to_string(),
                account_id: account_id_of(&auth),
            });
        }

        let _guard = self.refresh_lock.lock().await;
        // Re-read after acquiring the lock: another task may have refreshed.
        let auth = read_auth_file(&self.config.codex_auth_file).await?;
        let access = auth
            .get("tokens")
            .and_then(|t| t.get("access_token"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::config("Codex auth file does not contain tokens.access_token")
            })?;
        if !token_expires_soon(access, 90) {
            return Ok(Bearer {
                access_token: access.to_string(),
                account_id: account_id_of(&auth),
            });
        }

        self.refresh_now(auth).await
    }

    async fn refresh_now(&self, auth: Value) -> Result<Bearer, AppError> {
        let refresh_token = auth
            .get("tokens")
            .and_then(|t| t.get("refresh_token"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::config("Codex auth file does not contain tokens.refresh_token")
            })?
            .to_string();

        let form = [
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("client_id", self.config.codex_client_id.clone()),
        ];

        let json = self.post_token(&form, "Codex OAuth refresh failed").await?;

        let mut updated = auth.clone();
        let tokens = updated
            .get_mut("tokens")
            .and_then(|t| t.as_object_mut())
            .ok_or_else(|| AppError::config("Codex auth file has no tokens object"))?;

        for (src, dst) in [
            ("id_token", "id_token"),
            ("access_token", "access_token"),
            ("refresh_token", "refresh_token"),
        ] {
            if let Some(v) = json.get(src).filter(|v| !v.is_null()) {
                tokens.insert(dst.to_string(), v.clone());
            }
        }
        if let Some(v) = json
            .get("account_id")
            .or_else(|| json.get("chatgpt_account_id"))
            .filter(|v| !v.is_null())
        {
            tokens.insert("account_id".to_string(), v.clone());
        }

        if updated
            .get("auth_mode")
            .map(|v| v.is_null())
            .unwrap_or(true)
        {
            updated["auth_mode"] = json!("chatgpt");
        }
        updated["last_refresh"] = json!(iso8601(now_ms()));

        write_auth_file(&self.config.codex_auth_file, &updated).await?;

        Ok(Bearer {
            access_token: updated["tokens"]["access_token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            account_id: account_id_of(&updated),
        })
    }

    async fn post_token(&self, form: &[(&str, String)], context: &str) -> Result<Value, AppError> {
        let resp = self
            .http
            .post(&self.config.codex_token_url)
            .timeout(Duration::from_millis(self.config.codex_auth_timeout_ms))
            .form(form)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AppError::new(
                        504,
                        format!("{context}: request timed out"),
                        "upstream_timeout",
                    )
                } else {
                    AppError::from(e)
                }
            })?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));

        if !status.is_success() {
            let message = json
                .get("error_description")
                .or_else(|| json.get("error"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
            return Err(AppError::new(
                status.as_u16(),
                format!("{context}: {message}"),
                "authentication_error",
            ));
        }

        Ok(json)
    }

    pub async fn status(&self) -> Value {
        match read_auth_file(&self.config.codex_auth_file).await {
            Ok(auth) => {
                let mut summary = token_summary(auth.get("tokens").unwrap_or(&Value::Null));
                summary["auth_mode"] = auth.get("auth_mode").cloned().unwrap_or(Value::Null);
                summary["last_refresh"] = auth.get("last_refresh").cloned().unwrap_or(Value::Null);
                summary
            }
            Err(_) => json!({
                "authenticated": false,
                "auth_mode": null,
                "account_id": null,
                "access_token_expires_at": null,
                "has_refresh_token": false,
                "last_refresh": null,
            }),
        }
    }

    pub async fn start_login(&self, redirect_uri: String) -> LoginStart {
        let state = random_base64url(24);
        let code_verifier = random_base64url(64);
        let code_challenge = sha256_base64url(&code_verifier);
        let created_at = now_ms();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                state.clone(),
                PendingLogin {
                    code_verifier,
                    redirect_uri: redirect_uri.clone(),
                    created_at_ms: created_at,
                },
            );
            let cutoff = created_at.saturating_sub(10 * 60 * 1000);
            pending.retain(|_, p| p.created_at_ms >= cutoff);
        }

        let mut url = url::Url::parse(&self.config.codex_authorize_url).unwrap();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code");
            q.append_pair("client_id", &self.config.codex_client_id);
            q.append_pair("redirect_uri", &redirect_uri);
            q.append_pair("state", &state);
            q.append_pair("code_challenge", &code_challenge);
            q.append_pair("code_challenge_method", "S256");
            if !self.config.codex_oauth_scope.is_empty() {
                q.append_pair("scope", &self.config.codex_oauth_scope);
            }
            q.append_pair("id_token_add_organizations", "true");
            q.append_pair("codex_cli_simplified_flow", "true");
            q.append_pair("originator", "codex_cli_rs");
        }

        LoginStart {
            authorization_url: url.to_string(),
            expires_at: iso8601(created_at + 10 * 60 * 1000),
        }
    }

    pub async fn finish_login(&self, code: &str, state: &str) -> Result<Value, AppError> {
        let pending = {
            let mut map = self.pending.lock().await;
            map.remove(state)
        }
        .ok_or_else(|| {
            AppError::bad_request("OAuth state is missing or expired. Start login again.")
        })?;

        let form = [
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", pending.redirect_uri),
            ("client_id", self.config.codex_client_id.clone()),
            ("code_verifier", pending.code_verifier),
        ];

        let json = self
            .post_token(&form, "Codex OAuth token exchange failed")
            .await?;

        let auth = build_chatgpt_auth(&json);
        write_auth_file(&self.config.codex_auth_file, &auth).await?;
        Ok(token_summary(&auth["tokens"]))
    }

    /// Posts a form and returns the raw status + parsed body without treating a
    /// non-2xx response as an error, so callers can branch on pending/expired.
    async fn post_form_raw(
        &self,
        url: &str,
        form: &[(&str, String)],
    ) -> Result<(u16, Value), AppError> {
        let resp = self
            .http
            .post(url)
            .timeout(Duration::from_millis(self.config.codex_auth_timeout_ms))
            .form(form)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AppError::new(504, "Device auth request timed out", "upstream_timeout")
                } else {
                    AppError::from(e)
                }
            })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
        Ok((status, json))
    }

    /// Starts the headless device-code login. Returns the user code and the URL
    /// the user should open; no localhost callback is required.
    pub async fn start_device_login(&self) -> Result<Value, AppError> {
        let form = [("client_id", self.config.codex_client_id.clone())];
        let (status, json) = self
            .post_form_raw(&self.config.codex_device_usercode_url, &form)
            .await?;
        if !(200..300).contains(&status) {
            let message = json
                .get("error_description")
                .or_else(|| json.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to start device login");
            return Err(AppError::new(
                status,
                message.to_string(),
                "authentication_error",
            ));
        }

        let device_auth_id = json.get("device_auth_id").cloned().unwrap_or(Value::Null);
        let user_code = json.get("user_code").cloned().unwrap_or(Value::Null);
        let interval = json.get("interval").and_then(|v| v.as_i64()).unwrap_or(5);
        let expires_in = json
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(900);

        Ok(json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
            "verification_url": self.config.codex_device_verification_url,
            "interval": interval,
            "expires_in": expires_in,
        }))
    }

    /// Polls the device-code endpoint. While the user has not yet approved, returns
    /// `{ "status": "pending" }`; on success writes the auth file and returns the
    /// token summary with `status: "complete"`.
    pub async fn poll_device_login(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<Value, AppError> {
        let form = [
            ("device_auth_id", device_auth_id.to_string()),
            ("user_code", user_code.to_string()),
        ];
        let (status, json) = self
            .post_form_raw(&self.config.codex_device_token_url, &form)
            .await?;

        if status == 403 || status == 404 {
            return Ok(json!({ "status": "pending" }));
        }
        if status == 410 {
            return Err(AppError::new(
                410,
                "Device code expired. Start the device login again.",
                "authentication_error",
            ));
        }
        if !(200..300).contains(&status) {
            let message = json
                .get("error_description")
                .or_else(|| json.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("Device login failed");
            return Err(AppError::new(
                status,
                message.to_string(),
                "authentication_error",
            ));
        }

        let Some(code) = json.get("authorization_code").and_then(|v| v.as_str()) else {
            return Ok(json!({ "status": "pending" }));
        };
        let code_verifier = json
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::config("Device token response missing code_verifier"))?;

        let form = [
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            (
                "redirect_uri",
                self.config.codex_device_redirect_uri.clone(),
            ),
            ("client_id", self.config.codex_client_id.clone()),
            ("code_verifier", code_verifier.to_string()),
        ];
        let tokens = self
            .post_token(&form, "Device code exchange failed")
            .await?;

        let auth = build_chatgpt_auth(&tokens);
        write_auth_file(&self.config.codex_auth_file, &auth).await?;
        let mut summary = token_summary(&auth["tokens"]);
        summary["status"] = json!("complete");
        Ok(summary)
    }
}

fn build_chatgpt_auth(json: &Value) -> Value {
    json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": json.get("id_token").cloned().unwrap_or(Value::Null),
            "access_token": json.get("access_token").cloned().unwrap_or(Value::Null),
            "refresh_token": json.get("refresh_token").cloned().unwrap_or(Value::Null),
            "account_id": json.get("account_id")
                .or_else(|| json.get("chatgpt_account_id"))
                .cloned()
                .unwrap_or(Value::Null),
        },
        "last_refresh": iso8601(now_ms()),
    })
}

fn account_id_of(auth: &Value) -> Option<String> {
    auth.get("tokens")
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
