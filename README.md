# ccProxy

本地代理服务：接受 Anthropic Messages API 请求，转成 OpenAI Responses API 请求。默认使用本机 Codex CLI 的 ChatGPT OAuth 登录态，也可以切换到 OpenAI API key。

同一套 Rust 代理服务同时用于 Linux 部署和 macOS 菜单栏 App。

## 启动

```bash
cargo run --release
```

默认监听 `http://127.0.0.1:48317`，核心接口：

```text
POST /v1/messages
GET  /health
GET  /v1/models
GET  /admin
```

## macOS App

```bash
./scripts/build-mac-app.sh
open dist/ccProxy.app
```

Mac App 会内置并启动同一个 Rust 代理二进制，不再依赖 Node。

## 配置

默认 `OPENAI_AUTH_MODE=codex`，读取 `~/.codex/auth.json`：

```bash
HOST=127.0.0.1 PORT=48317 cargo run --release
```

如果本机还没有 Codex 登录态，先启动服务，再打开登录链接：

```bash
curl -sS http://127.0.0.1:48317/auth/login
```

返回里的 `authorization_url` 复制到浏览器打开。网页登录完成后会回调：

```text
http://localhost:48317/auth/callback
```

代理会把换到的 token 写入 `~/.codex/auth.json`。检查状态：

```bash
curl -sS http://127.0.0.1:48317/auth/status
```

如果上游需要走本机代理：

```bash
UPSTREAM_PROXY_URL=http://127.0.0.1:6789 cargo run --release
```

代码会优先使用 `UPSTREAM_PROXY_URL`，否则读取 `https_proxy` / `http_proxy` / `all_proxy`，支持 `http://`、`https://` 和 `socks5://`。

切到 API key：

```bash
OPENAI_AUTH_MODE=api-key OPENAI_API_KEY=sk-... cargo run --release
```

## Token 管理

设置管理密码：

```bash
ADMIN_API_KEY=admin-secret cargo run --release
```

打开：

```text
http://127.0.0.1:48317/admin
```

管理后台可以给不同使用者创建独立 token，并记录每个 token 的请求数、输入 token、输出 token 和推理 token。

SQLite 默认写到：

```text
~/.ccproxy/ccproxy.db
```

可通过 `DB_PATH` 修改。

`PROXY_API_KEY` 是主密钥，客户端带它时可以绕过 per-user token 统计；普通使用者建议使用管理后台生成的 `sk-ccp-...` token。

## 示例

```bash
curl -sS http://127.0.0.1:48317/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'x-api-key: sk-ccp-...' \
  -d '{
    "model": "gpt-5.3-codex",
    "max_tokens": 200,
    "messages": [{"role": "user", "content": "Say hello in Chinese"}]
  }'
```

reasoning effort 可以放在模型名里：

```json
{
  "model": "gpt-5.5-high",
  "max_tokens": 200,
  "messages": [{"role": "user", "content": "Solve a hard bug"}]
}
```

也支持 `"gpt-5.5 high"`、`"gpt-5.5:high"`，都会转成上游 `model: "gpt-5.5"` 和 `reasoning.effort: "high"`。

流式：

```bash
curl -N http://127.0.0.1:48317/v1/messages \
  -H 'content-type: application/json' \
  -H 'x-api-key: sk-ccp-...' \
  -d '{
    "model": "gpt-5.3-codex",
    "max_tokens": 200,
    "stream": true,
    "messages": [{"role": "user", "content": "Count to 3"}]
  }'
```

## Linux systemd

参考 `systemd/ccproxy.service`，把编译后的 `ccproxy` 和 `.env` 放到 `/opt/ccproxy` 后启用服务。
