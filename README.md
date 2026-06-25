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
- **内置 OAuth 登录**:本机没有 Codex 登录态时,可走 `/auth/login`(浏览器回调)或 `/auth/device/*`(设备码,无需 localhost,适合远程服务器)。
- **多用户 Token 管理**:Web 后台为不同使用者分发独立 `sk-ccp-...` token,逐 token 统计请求数与输入/输出/推理 token 消耗。
- **Claude Code 开箱即用**:自动把 `claude-*`(opus/sonnet/haiku,含 `[1m]` 后缀)映射到对应 gpt 模型,并把 Anthropic `thinking` 块转成 `reasoning.effort`;思维链通过加密 reasoning 跨轮保留。
- **模型名内联 reasoning effort**:`gpt-5.5-high` / `gpt-5.5:high` / `gpt-5.5 high` / `gpt-5.5-max` 自动拆成 `model` + `reasoning.effort`(`max` → `xhigh`)。
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
| `GET /auth/login` · `/auth/status` | Codex OAuth 登录(浏览器)/ 状态 |
| `GET /auth/device/start` · `/poll` | Codex 设备码登录(无需回调) |

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

远程/无头服务器没有浏览器回调时,用设备码登录:

```bash
# 1. 拿到 user_code 和验证地址
curl -sS http://127.0.0.1:48317/auth/device/start
# 2. 浏览器打开返回的 verification_url,输入 user_code 完成授权
# 3. 轮询直到 status=complete(token 会写回 ~/.codex/auth.json)
curl -sS "http://127.0.0.1:48317/auth/device/poll?device_auth_id=...&user_code=..."
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

打开 `http://127.0.0.1:48317/admin`,用 `ADMIN_API_KEY` 登录。可以为不同使用者创建独立 token,查看每个 token 的请求数与输入/输出/推理 token 消耗、随时停用或删除。

可选给某个 token 设**额度**(token 总量上限,输入+输出+推理累计):用满后该 token 的请求会被 429 拒绝,直到调高或清除额度。`/v1/messages`(流式与非流式)和 `/v1/responses` 透传的用量都会按 token 计入。

鉴权优先级:

- **`PROXY_API_KEY`** —— 主密钥。客户端带它可直接访问,绕过 per-user 统计与额度。
- **`sk-ccp-...`** —— 后台为每个使用者生成的 token,推荐分发给普通用户,用量逐个记录、可设额度。

> 当 `PROXY_API_KEY` 未设、且尚未创建任何 token 时,接口完全开放;一旦创建了 token(或设了 `PROXY_API_KEY`),未知 key 会被拒绝。

数据默认存到 SQLite `~/.ccproxy/ccproxy.db`,可用 `DB_PATH` 覆盖。

## 部署

### macOS 菜单栏 App

```bash
./scripts/build-mac-app.sh
open dist/ccProxy.app
```

App 内置并启动同一个 Rust 代理二进制。改了 `src/dashboard.html` 等被编译进二进制的资源后,需要重新运行此脚本。

### Linux systemd

用一键脚本安装 / 更新 / 卸载(自动编译、布署到 `/opt/ccproxy`、生成 systemd 服务并启动):

```bash
./scripts/server.sh install     # 首次安装
./scripts/server.sh update      # 拉新代码后更新:--pull 可顺带 git pull
./scripts/server.sh uninstall   # 卸载(加 --purge 连数据一起删)
```

安装目录、运行用户可用环境变量覆盖,例如 `CCPROXY_PREFIX=/srv/ccproxy CCPROXY_USER=ccproxy ./scripts/server.sh install`。安装后编辑 `/opt/ccproxy/.env` 填入配置,再 `sudo systemctl restart ccproxy`。token 数据库在更新时自动迁移。

手动布署可参考 `systemd/ccproxy.service`:把 `cargo build --release` 的产物和 `.env` 放到 `/opt/ccproxy` 再启用服务。

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
