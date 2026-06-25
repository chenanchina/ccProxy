use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Codex,
    ApiKey,
}

/// Maps Anthropic-family model names sent by clients (e.g. Claude Code) onto the
/// upstream gpt models that Codex actually serves.
#[derive(Clone, Debug)]
pub struct ModelMap {
    pub opus: String,
    pub sonnet: String,
    pub haiku: String,
}

impl Default for ModelMap {
    fn default() -> Self {
        ModelMap {
            opus: "gpt-5.5".to_string(),
            sonnet: "gpt-5.5".to_string(),
            haiku: "gpt-5.4-mini".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub proxy_api_key: Option<String>,
    pub admin_api_key: Option<String>,
    pub db_path: PathBuf,
    pub auth_mode: AuthMode,
    pub openai_api_key: Option<String>,
    pub openai_api_base: String,
    pub codex_api_base: String,
    pub codex_authorize_url: String,
    pub codex_token_url: String,
    pub codex_client_version: String,
    pub codex_auth_file: PathBuf,
    pub codex_client_id: String,
    pub codex_oauth_scope: String,
    pub codex_oauth_redirect_host: String,
    pub codex_device_usercode_url: String,
    pub codex_device_token_url: String,
    pub codex_device_verification_url: String,
    pub codex_device_redirect_uri: String,
    pub model_map: ModelMap,
    pub dashboard_path: Option<PathBuf>,
    pub default_instructions: String,
    pub upstream_proxy_url: Option<String>,
    pub extra_headers: HashMap<String, String>,
    pub request_timeout_ms: u64,
    pub stream_idle_timeout_ms: u64,
    pub sse_max_frame_bytes: usize,
    pub model_list_timeout_ms: u64,
    pub codex_auth_timeout_ms: u64,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(value)
}

fn get(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).filter(|v| !v.is_empty()).cloned()
}

fn get_or(env: &HashMap<String, String>, key: &str, fallback: &str) -> String {
    get(env, key).unwrap_or_else(|| fallback.to_string())
}

fn parse_u64(env: &HashMap<String, String>, key: &str, fallback: u64) -> u64 {
    get(env, key)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(fallback)
}

fn pick_proxy_url(env: &HashMap<String, String>) -> Option<String> {
    for key in [
        "UPSTREAM_PROXY_URL",
        "https_proxy",
        "HTTPS_PROXY",
        "http_proxy",
        "HTTP_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        if let Some(v) = get(env, key) {
            return Some(v);
        }
    }
    None
}

fn parse_extra_headers(env: &HashMap<String, String>) -> HashMap<String, String> {
    let Some(raw) = get(env, "OPENAI_EXTRA_HEADERS") else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, serde_json::Value>>(&raw) {
        Ok(map) => map
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                serde_json::Value::Null => None,
                other => Some((k, other.to_string())),
            })
            .collect(),
        Err(e) => {
            eprintln!("OPENAI_EXTRA_HEADERS must be valid JSON: {e}");
            HashMap::new()
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_map(&env)
    }

    pub fn from_map(env: &HashMap<String, String>) -> Self {
        let chatgpt_base_url = get_or(env, "CHATGPT_BASE_URL", "https://chatgpt.com/backend-api");
        let auth_mode = match get_or(env, "OPENAI_AUTH_MODE", "codex").as_str() {
            "api-key" => AuthMode::ApiKey,
            _ => AuthMode::Codex,
        };

        Config {
            host: get_or(env, "HOST", "127.0.0.1"),
            port: get(env, "PORT")
                .and_then(|v| v.trim().parse::<u16>().ok())
                .unwrap_or(48317),
            proxy_api_key: get(env, "PROXY_API_KEY"),
            admin_api_key: get(env, "ADMIN_API_KEY"),
            db_path: expand_home(&get_or(env, "DB_PATH", "~/.ccproxy/ccproxy.db")),
            auth_mode,
            openai_api_key: get(env, "OPENAI_API_KEY"),
            openai_api_base: get_or(env, "OPENAI_API_BASE", "https://api.openai.com/v1"),
            codex_api_base: get(env, "CODEX_API_BASE")
                .unwrap_or_else(|| format!("{chatgpt_base_url}/codex")),
            codex_authorize_url: get_or(
                env,
                "CODEX_AUTHORIZE_URL",
                "https://auth.openai.com/oauth/authorize",
            ),
            codex_token_url: get_or(
                env,
                "CODEX_TOKEN_URL",
                "https://auth.openai.com/oauth/token",
            ),
            codex_client_version: get_or(env, "CODEX_CLIENT_VERSION", "0.128.0"),
            codex_auth_file: expand_home(&get_or(env, "CODEX_AUTH_FILE", "~/.codex/auth.json")),
            codex_client_id: get_or(env, "CODEX_CLIENT_ID", "app_EMoamEEZ73f0CkXaXp7hrann"),
            codex_oauth_scope: get_or(
                env,
                "CODEX_OAUTH_SCOPE",
                "openid profile email offline_access api.connectors.read api.connectors.invoke",
            ),
            codex_oauth_redirect_host: get_or(env, "CODEX_OAUTH_REDIRECT_HOST", "localhost"),
            codex_device_usercode_url: get_or(
                env,
                "CODEX_DEVICE_USERCODE_URL",
                "https://auth.openai.com/api/accounts/deviceauth/usercode",
            ),
            codex_device_token_url: get_or(
                env,
                "CODEX_DEVICE_TOKEN_URL",
                "https://auth.openai.com/api/accounts/deviceauth/token",
            ),
            codex_device_verification_url: get_or(
                env,
                "CODEX_DEVICE_VERIFICATION_URL",
                "https://auth.openai.com/codex/device",
            ),
            codex_device_redirect_uri: get_or(
                env,
                "CODEX_DEVICE_REDIRECT_URI",
                "https://auth.openai.com/deviceauth/callback",
            ),
            model_map: ModelMap {
                opus: get_or(env, "ANTHROPIC_DEFAULT_OPUS_MODEL", "gpt-5.5"),
                sonnet: get_or(env, "ANTHROPIC_DEFAULT_SONNET_MODEL", "gpt-5.5"),
                haiku: get_or(env, "ANTHROPIC_DEFAULT_HAIKU_MODEL", "gpt-5.4-mini"),
            },
            dashboard_path: get(env, "DASHBOARD_PATH").map(|p| expand_home(&p)),
            default_instructions: get_or(
                env,
                "DEFAULT_INSTRUCTIONS",
                "You are a helpful assistant.",
            ),
            upstream_proxy_url: pick_proxy_url(env),
            extra_headers: parse_extra_headers(env),
            request_timeout_ms: parse_u64(env, "REQUEST_TIMEOUT_MS", 600_000),
            stream_idle_timeout_ms: parse_u64(env, "STREAM_IDLE_TIMEOUT_MS", 300_000),
            sse_max_frame_bytes: parse_u64(env, "SSE_MAX_FRAME_BYTES", 1024 * 1024) as usize,
            model_list_timeout_ms: parse_u64(env, "MODEL_LIST_TIMEOUT_MS", 10_000),
            codex_auth_timeout_ms: parse_u64(env, "CODEX_AUTH_TIMEOUT_MS", 30_000),
        }
    }
}
