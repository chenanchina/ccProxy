use serde_json::Value;

/// Builds a markdown usage report shown when a client sends the usage command.
/// `token_id` is the caller's token (None = master key). `tokens` is the admin
/// token list; `account` is the cached Codex rate-limit snapshot.
pub fn build_report(tokens: &[Value], token_id: Option<i64>, account: &Value) -> String {
    let mut out = String::from("**你的用量**\n");

    match token_id.and_then(|id| {
        tokens
            .iter()
            .find(|t| t.get("id").and_then(|v| v.as_i64()) == Some(id))
    }) {
        Some(t) => {
            let g = |k: &str| t.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let used = g("used_tokens");
            let limit = t
                .get("token_limit")
                .and_then(|v| v.as_i64())
                .filter(|n| *n > 0);
            let window = t
                .get("quota_window_days")
                .and_then(|v| v.as_i64())
                .filter(|n| *n > 0);
            out.push_str(&format!("- token：{name}\n"));
            match limit {
                Some(l) => {
                    let remain = (l - used).max(0);
                    let cycle = window
                        .map(|d| format!("（近 {d} 天滚动）"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "- 已用 {} / {}，余量 {}{}\n",
                        fmt_num(used),
                        fmt_num(l),
                        fmt_num(remain),
                        cycle
                    ));
                }
                None => out.push_str(&format!("- 已用 {}（无限额）\n", fmt_num(used))),
            }
            out.push_str(&format!("- 历史请求数：{}\n", fmt_num(g("requests"))));
        }
        None => out.push_str("- 当前为主密钥（master），无个人限额\n"),
    }

    out.push_str("\n**账号套餐额度**\n");
    out.push_str(&account_section(account));
    out
}

fn account_section(account: &Value) -> String {
    let Some(h) = account
        .get("headers")
        .and_then(|v| v.as_object())
        .filter(|m| !m.is_empty())
    else {
        return "- 暂无数据（发起一次请求后再查）\n".to_string();
    };
    let mut out = String::new();
    let mut any = false;
    for (kind, fallback) in [("primary", "短期窗口"), ("secondary", "长期窗口")] {
        if let Some(pct) = h
            .get(&format!("x-codex-{kind}-used-percent"))
            .and_then(|v| v.as_str())
        {
            any = true;
            let label = window_label(
                h.get(&format!("x-codex-{kind}-window-minutes"))
                    .and_then(|v| v.as_str()),
                fallback,
            );
            let resets = h
                .get(&format!("x-codex-{kind}-reset-after-seconds"))
                .and_then(|v| v.as_str())
                .map(|s| format!("，约 {} 后重置", dur(s)))
                .unwrap_or_default();
            out.push_str(&format!("- {label}：已用 {pct}%{resets}\n"));
        }
    }
    if !any {
        for (k, v) in h {
            out.push_str(&format!("- {k}: {}\n", v.as_str().unwrap_or("")));
        }
    }
    out
}

fn fmt_num(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let len = s.len();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

fn dur(secs: &str) -> String {
    let s: i64 = secs.trim().parse().unwrap_or(0);
    if s < 60 {
        format!("{s}秒")
    } else if s < 3600 {
        format!("{}分钟", s / 60)
    } else if s < 86400 {
        format!("{}小时", s / 3600)
    } else {
        format!("{}天", s / 86400)
    }
}

fn window_label(min: Option<&str>, fallback: &str) -> String {
    match min.and_then(|m| m.trim().parse::<i64>().ok()) {
        Some(m) if m >= 1440 => format!("{} 天窗口", m / 1440),
        Some(m) if m >= 60 => format!("{} 小时窗口", m / 60),
        Some(m) => format!("{m} 分钟窗口"),
        None => fallback.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_shows_personal_quota_and_account() {
        let tokens = json!([
            { "id": 1, "name": "张三", "requests": 1234, "used_tokens": 56000,
              "input_tokens": 40000, "output_tokens": 16000, "reasoning_tokens": 0,
              "token_limit": 100000, "quota_window_days": 30, "disabled": false }
        ]);
        let account = json!({ "headers": {
            "x-codex-primary-used-percent": "42",
            "x-codex-primary-window-minutes": "300",
            "x-codex-primary-reset-after-seconds": "7200"
        }});
        let r = build_report(tokens.as_array().unwrap(), Some(1), &account);
        assert!(r.contains("张三"));
        assert!(r.contains("56,000 / 100,000"));
        assert!(r.contains("余量 44,000"));
        assert!(r.contains("5 小时窗口"));
        assert!(r.contains("已用 42%"));
    }

    #[test]
    fn report_handles_master_and_empty_account() {
        let r = build_report(&[], None, &json!({}));
        assert!(r.contains("master"));
        assert!(r.contains("暂无数据"));
    }
}
