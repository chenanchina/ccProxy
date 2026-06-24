# ccProxy

把 **Anthropic Messages API** 的请求实时转换成 **OpenAI Responses API**,默认复用本机 Codex CLI 的 ChatGPT OAuth 登录态,也可切换到 OpenAI API key。

一套 Rust 服务同时用于 **Linux 部署** 和 **macOS 菜单栏 App**——让 Claude Code、以及任何走 Anthropic 协议的客户端,都能用上 ChatGPT/Codex 后端。

> [!NOTE]
> ccProxy **只负责把 Codex 套餐转换成一个 Anthropic 兼容接口**,它本身不是客户端。
> 要在 Claude Code 里用上,还需要配合 [cc-switch](https://github.com/farion1231/cc-switch) 等工具,
> 把客户端的 API 地址指向本代理(`http://127.0.0.1:48317`)、并填入这里分发的 `sk-ccp-...` token。
> 任何支持自定义 Anthropic Base URL 的客户端同理。

## 特性

- **协议转换**:`/v1/messages` 接受 Anthropic 格式,转发到 Codex Responses 后端,流式与非流式都支持。
- **两种鉴权后端**:`codex` 模式直接复用 `~/.codex/auth.json` 并自动刷新 OAuth token;`api-key` 模式走标准 OpenAI API。
- **内置 OAuth 登录**:本机没有 Codex 登录态时,可直接通过 `/auth/login` 走完整登录流程。
- **多用户 Token 管理**:Web 后台为不同使用者分发独立 `sk-ccp-...` token,逐 token 统计请求数与输入/输出/推理 token 消耗。
- **模型名内联 reasoning effort**:`gpt-5.5-high` / `gpt-5.5:high` / `gpt-5.5 high` 自动拆成 `model` + `reasoning.effort`。
- **上游代理支持**:HTTP / HTTPS / SOCKS5 隧道。
- **零外部依赖**:单个 Rust 二进制,SQLite 内嵌,macOS App 不依赖 Node。

## 快速开始

```bash
cargo run --release
```

默认监听 `http://127.0.0.1:48317`。核心接口:

| 接口 | 说明 |
| --- | --- |
| `POST /v1/messages` | Anthropic Messages,主入口 |
| `POST /v1/responses` | OpenAI Responses 透传 |
| `GET /v1/models` | 模型列表 |
| `GET /health` | 健康检查 |
| `GET /admin` | Token 管理后台 |
| `GET /auth/login` · `/auth/status` | Codex OAuth 登录 / 状态 |

### 调用示例

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

把 reasoning effort 写进模型名,`gpt-5.5-high`、`gpt-5.5:high`、`gpt-5.5 high` 都会转成上游 `model: "gpt-5.5"` + `reasoning.effort: "high"`:

```json
{ "model": "gpt-5.5-high", "max_tokens": 200, "messages": [{ "role": "user", "content": "Solve a hard bug" }] }
```

流式加 `"stream": true`,返回标准 Anthropic SSE 事件流:

```bash
curl -N http://127.0.0.1:48317/v1/messages \
  -H 'content-type: application/json' \
  -H 'x-api-key: sk-ccp-...' \
  -d '{"model":"gpt-5.3-codex","max_tokens":200,"stream":true,
       "messages":[{"role":"user","content":"Count to 3"}]}'
```

## 鉴权后端

### Codex 模式(默认)

读取 `~/.codex/auth.json` 的 ChatGPT OAuth token,过期自动刷新。

本机还没登录时,先启动服务再走登录流程:

```bash
# 1. 拿到授权链接
curl -sS http://127.0.0.1:48317/auth/login
# 2. 把返回里的 authorization_url 复制到浏览器登录,完成后回调:
#    http://localhost:48317/auth/callback
# 3. 换到的 token 会写回 ~/.codex/auth.json,确认状态:
curl -sS http://127.0.0.1:48317/auth/status
```

### API key 模式

```bash
OPENAI_AUTH_MODE=api-key OPENAI_API_KEY=sk-... cargo run --release
```

## Token 管理

设置管理密码即可启用后台:

```bash
ADMIN_API_KEY=admin-secret cargo run --release
```

打开 `http://127.0.0.1:48317/admin`,用 `ADMIN_API_KEY` 登录。可以为不同使用者创建独立 token,查看每个 token 的请求数与 token 消耗、随时停用或删除。

鉴权优先级:

- **`PROXY_API_KEY`** —— 主密钥。客户端带它可直接访问,绕过 per-user 统计。
- **`sk-ccp-...`** —— 后台为每个使用者生成的 token,推荐分发给普通用户,用量逐个记录。

数据默认存到 SQLite `~/.ccproxy/ccproxy.db`,可用 `DB_PATH` 覆盖。

## 部署

### macOS 菜单栏 App

```bash
./scripts/build-mac-app.sh
open dist/ccProxy.app
```

App 内置并启动同一个 Rust 代理二进制。改了 `src/dashboard.html` 等被编译进二进制的资源后,需要重新运行此脚本。

### Linux systemd

参考 `systemd/ccproxy.service`:把 release 二进制和 `.env` 放到 `/opt/ccproxy`,再启用服务。

```bash
cargo build --release
```

## 配置

所有配置走环境变量,完整清单见 [`.env.example`](.env.example)。常用项:

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `HOST` / `PORT` | `127.0.0.1` / `48317` | 监听地址 |
| `OPENAI_AUTH_MODE` | `codex` | `codex` 或 `api-key` |
| `ADMIN_API_KEY` | — | 设置后启用 `/admin` 后台 |
| `PROXY_API_KEY` | — | 主密钥,绕过 per-user 统计 |
| `DB_PATH` | `~/.ccproxy/ccproxy.db` | SQLite 文件 |
| `OPENAI_API_KEY` | — | `api-key` 模式必填 |
| `CODEX_AUTH_FILE` | `~/.codex/auth.json` | Codex 登录态文件 |
| `UPSTREAM_PROXY_URL` | — | 上游代理,支持 `http/https/socks5` |
| `REQUEST_TIMEOUT_MS` | `600000` | 请求超时 |
| `STREAM_IDLE_TIMEOUT_MS` | `300000` | 流式空闲超时 |

上游代理未显式设置 `UPSTREAM_PROXY_URL` 时,会依次读取 `https_proxy` / `http_proxy` / `all_proxy`。

## 构建

```bash
cargo build --release   # 产物: target/release/ccproxy
cargo run --release     # 本地运行
```

## 许可证

[MIT](LICENSE) © Austin
