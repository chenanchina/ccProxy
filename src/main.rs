mod anthropic;
mod auth;
mod config;
mod db;
mod error;
mod models;
mod server;
mod sse;
mod upstream;

use std::sync::Arc;
use std::time::Duration;

use auth::CodexAuth;
use config::{AuthMode, Config};
use db::Db;
use upstream::Upstream;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let config = Arc::new(Config::from_env());

    let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(30));
    if let Some(proxy) = &config.upstream_proxy_url {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => builder = builder.proxy(p),
            Err(e) => {
                eprintln!("Invalid UPSTREAM_PROXY_URL {proxy}: {e}");
                std::process::exit(1);
            }
        }
    }
    let http = builder.build().expect("failed to build HTTP client");

    let codex_auth = if config.auth_mode == AuthMode::Codex {
        Some(Arc::new(CodexAuth::new(config.clone(), http.clone())))
    } else {
        None
    };

    let db = match Db::open(&config.db_path) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!("Failed to open database {}: {e}", config.db_path.display());
            std::process::exit(1);
        }
    };

    let upstream = Arc::new(Upstream::new(config.clone(), http, codex_auth.clone()));

    // Periodically prune old usage rows so the database does not grow unbounded.
    if config.usage_retention_days > 0 {
        let db = db.clone();
        let retention_ms = config.usage_retention_days as i64 * 86_400_000;
        tokio::spawn(async move {
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let removed = db.purge_usage_older_than(now - retention_ms);
                if removed > 0 {
                    println!("usage retention: pruned {removed} old records");
                }
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
            }
        });
    }

    let state = server::AppState {
        config: config.clone(),
        upstream,
        auth: codex_auth,
        db,
    };

    let app = server::router(state);

    let addr = format!("{}:{}", config.host, config.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };

    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    println!("ccProxy listening on http://{local}");
    println!(
        "auth mode: {}",
        if config.auth_mode == AuthMode::Codex {
            "codex"
        } else {
            "api-key"
        }
    );
    if let Some(proxy) = &config.upstream_proxy_url {
        println!("upstream proxy: {proxy}");
    }
    println!("token db: {}", config.db_path.display());
    if config.admin_api_key.is_some() || config.proxy_api_key.is_some() {
        println!("admin dashboard: http://{local}/admin (login with ADMIN_API_KEY)");
    } else {
        println!("admin dashboard disabled: set ADMIN_API_KEY to enable token management");
    }

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
