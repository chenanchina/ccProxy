use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::error::AppError;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tokens (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  token TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  note TEXT,
  disabled INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
CREATE TABLE IF NOT EXISTS usage (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  token_id INTEGER,
  ts INTEGER NOT NULL,
  model TEXT,
  input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  reasoning_tokens INTEGER NOT NULL DEFAULT 0,
  stream INTEGER NOT NULL DEFAULT 0,
  path TEXT,
  status INTEGER,
  FOREIGN KEY(token_id) REFERENCES tokens(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_token ON usage(token_id);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage(ts);
";

pub struct Db {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let hex: String = (0..32)
        .map(|_| format!("{:x}", rng.gen_range(0..16u8)))
        .collect();
    format!("sk-ccp-{hex}")
}

fn db_err(e: rusqlite::Error) -> AppError {
    AppError::new(500, format!("Database error: {e}"), "database_error")
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// Returns the token id when the key matches an enabled token, and bumps last_used_at.
    pub fn verify_token(&self, key: &str) -> Option<i64> {
        let conn = self.conn.lock().ok()?;
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM tokens WHERE token = ?1 AND disabled = 0",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(id) = id {
            let _ = conn.execute(
                "UPDATE tokens SET last_used_at = ?1 WHERE id = ?2",
                params![now_ms(), id],
            );
        }
        id
    }

    pub fn has_tokens(&self) -> bool {
        let Ok(conn) = self.conn.lock() else {
            return false;
        };
        conn.query_row("SELECT COUNT(*) FROM tokens", [], |row| row.get::<_, i64>(0))
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    pub fn create_token(&self, name: &str, note: Option<&str>) -> Result<Value, AppError> {
        let conn = self.conn.lock().map_err(|_| {
            AppError::new(500, "Database lock poisoned", "database_error")
        })?;
        let token = generate_token();
        let created_at = now_ms();
        conn.execute(
            "INSERT INTO tokens (token, name, note, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![token, name, note, created_at],
        )
        .map_err(db_err)?;
        let id = conn.last_insert_rowid();
        Ok(json!({
            "id": id,
            "token": token,
            "name": name,
            "note": note,
            "disabled": false,
            "created_at": created_at,
            "last_used_at": Value::Null,
            "input_tokens": 0,
            "output_tokens": 0,
            "reasoning_tokens": 0,
            "requests": 0,
        }))
    }

    pub fn list_tokens(&self) -> Result<Vec<Value>, AppError> {
        let conn = self.conn.lock().map_err(|_| {
            AppError::new(500, "Database lock poisoned", "database_error")
        })?;
        let mut stmt = conn
            .prepare(
                "SELECT t.id, t.token, t.name, t.note, t.disabled, t.created_at, t.last_used_at,
                        COALESCE(SUM(u.input_tokens), 0),
                        COALESCE(SUM(u.output_tokens), 0),
                        COALESCE(SUM(u.reasoning_tokens), 0),
                        COUNT(u.id)
                 FROM tokens t LEFT JOIN usage u ON u.token_id = t.id
                 GROUP BY t.id
                 ORDER BY t.created_at DESC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "token": row.get::<_, String>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "note": row.get::<_, Option<String>>(3)?,
                    "disabled": row.get::<_, i64>(4)? != 0,
                    "created_at": row.get::<_, i64>(5)?,
                    "last_used_at": row.get::<_, Option<i64>>(6)?,
                    "input_tokens": row.get::<_, i64>(7)?,
                    "output_tokens": row.get::<_, i64>(8)?,
                    "reasoning_tokens": row.get::<_, i64>(9)?,
                    "requests": row.get::<_, i64>(10)?,
                }))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db_err)?);
        }
        Ok(out)
    }

    pub fn update_token(
        &self,
        id: i64,
        name: Option<&str>,
        note: Option<&str>,
        disabled: Option<bool>,
    ) -> Result<(), AppError> {
        let conn = self.conn.lock().map_err(|_| {
            AppError::new(500, "Database lock poisoned", "database_error")
        })?;
        if let Some(name) = name {
            conn.execute("UPDATE tokens SET name = ?1 WHERE id = ?2", params![name, id])
                .map_err(db_err)?;
        }
        if let Some(note) = note {
            conn.execute("UPDATE tokens SET note = ?1 WHERE id = ?2", params![note, id])
                .map_err(db_err)?;
        }
        if let Some(disabled) = disabled {
            conn.execute(
                "UPDATE tokens SET disabled = ?1 WHERE id = ?2",
                params![disabled as i64, id],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }

    pub fn delete_token(&self, id: i64) -> Result<(), AppError> {
        let conn = self.conn.lock().map_err(|_| {
            AppError::new(500, "Database lock poisoned", "database_error")
        })?;
        conn.execute("DELETE FROM tokens WHERE id = ?1", params![id])
            .map_err(db_err)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &self,
        token_id: Option<i64>,
        model: Option<&str>,
        input_tokens: i64,
        output_tokens: i64,
        reasoning_tokens: i64,
        stream: bool,
        path: &str,
        status: u16,
    ) {
        let Ok(conn) = self.conn.lock() else {
            return;
        };
        let _ = conn.execute(
            "INSERT INTO usage (token_id, ts, model, input_tokens, output_tokens, reasoning_tokens, stream, path, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                token_id,
                now_ms(),
                model,
                input_tokens,
                output_tokens,
                reasoning_tokens,
                stream as i64,
                path,
                status as i64
            ],
        );
    }

    pub fn list_usage(&self, token_id: Option<i64>, limit: i64) -> Result<Vec<Value>, AppError> {
        let conn = self.conn.lock().map_err(|_| {
            AppError::new(500, "Database lock poisoned", "database_error")
        })?;
        let limit = limit.clamp(1, 1000);
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<Value> {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "token_id": row.get::<_, Option<i64>>(1)?,
                "name": row.get::<_, Option<String>>(2)?,
                "ts": row.get::<_, i64>(3)?,
                "model": row.get::<_, Option<String>>(4)?,
                "input_tokens": row.get::<_, i64>(5)?,
                "output_tokens": row.get::<_, i64>(6)?,
                "reasoning_tokens": row.get::<_, i64>(7)?,
                "stream": row.get::<_, i64>(8)? != 0,
                "path": row.get::<_, Option<String>>(9)?,
                "status": row.get::<_, Option<i64>>(10)?,
            }))
        };
        let sql_base = "SELECT u.id, u.token_id, t.name, u.ts, u.model, u.input_tokens, u.output_tokens,
                               u.reasoning_tokens, u.stream, u.path, u.status
                        FROM usage u LEFT JOIN tokens t ON t.id = u.token_id";
        let mut out = Vec::new();
        if let Some(tid) = token_id {
            let mut stmt = conn
                .prepare(&format!(
                    "{sql_base} WHERE u.token_id = ?1 ORDER BY u.ts DESC LIMIT ?2"
                ))
                .map_err(db_err)?;
            let rows = stmt.query_map(params![tid, limit], map_row).map_err(db_err)?;
            for r in rows {
                out.push(r.map_err(db_err)?);
            }
        } else {
            let mut stmt = conn
                .prepare(&format!("{sql_base} ORDER BY u.ts DESC LIMIT ?1"))
                .map_err(db_err)?;
            let rows = stmt.query_map(params![limit], map_row).map_err(db_err)?;
            for r in rows {
                out.push(r.map_err(db_err)?);
            }
        }
        Ok(out)
    }
}
