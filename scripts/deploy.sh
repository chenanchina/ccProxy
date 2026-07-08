#!/usr/bin/env bash
#
# ccProxy one-shot deployer for Ubuntu/Debian.
#
# 链路: 客户端 →(HTTPS)Caddy → ccProxy →(UPSTREAM_PROXY_URL)mihomo → 节点 → chatgpt.com
#
# 流程:先一次性采集所有信息 → 用(可本地上传的)mihomo 配好代理并验证 →
#       验证通过后把代理导出为环境变量,后续下载 deb / 装 Caddy 全走这个代理 →
#       安装 ccProxy → 写 /etc/ccproxy/.env 并重启 → 可选 Caddy HTTPS。
#
# 用法:
#   sudo -E bash deploy.sh                          # 全交互
#   # 国内服务器 + 已上传的 mihomo 二进制/压缩包:
#   CCP_MIHOMO_BIN=~/mihomo CCP_SUB_URL='https://airport/sub' sudo -E bash deploy.sh
#
# 常用环境变量(不设则交互询问,可留空表示跳过):
#   CCP_INSTALL_MIHOMO   yes | no             是否配置本地代理 mihomo
#   CCP_MIHOMO_BIN       路径                 已上传的 mihomo 二进制或 .gz(留空则从 GitHub 下)
#   CCP_SUB_URL          机场订阅链接
#   CCP_MIHOMO_PORT      本地代理端口         (默认 7890)
#   CCP_MIHOMO_VERSION   GitHub 下载用的版本  (默认见 DEFAULT_MIHOMO_VERSION)
#   CCP_INSTALL_METHOD   deb | source         (默认 deb)
#   CCP_DEB_URL          .deb 地址            (留空=查 GitHub 最新;本地包用 file:///路径)
#   CCP_ADMIN_KEY        /admin 管理密码       (留空自动生成)
#   CCP_PROXY_KEY        主密钥 PROXY_API_KEY (可选)
#   CCP_DOMAIN           对外域名             (设了才装 Caddy 走 HTTPS)
#
set -euo pipefail

REPO_SLUG="chenanchina/ccProxy"
DEFAULT_MIHOMO_VERSION="v1.18.10"
CONFDIR="/etc/ccproxy"
ENVFILE="${CONFDIR}/.env"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v apt-get >/dev/null 2>&1 || die "本脚本仅支持 Debian/Ubuntu(apt)。"

# ask VAR "提示" ["默认值"] —— env 已设则沿用,否则从 /dev/tty 读,无 tty 则用默认。
ask() {
  local __var="$1" __prompt="$2" __def="${3:-}" __in=""
  [ -n "${!__var:-}" ] && return
  if [ -r /dev/tty ]; then
    printf '%s' "$__prompt" >/dev/tty
    read -r __in </dev/tty || true
  fi
  printf -v "$__var" '%s' "${__in:-$__def}"
}

# yesno VAR "提示" "默认(yes/no)"
yesno() {
  local __var="$1"
  ask "$__var" "$2" "$3"
  case "${!__var,,}" in
    y|yes|true|1) printf -v "$__var" 'yes' ;;
    *)            printf -v "$__var" 'no'  ;;
  esac
}

# set_env KEY VALUE —— 删掉旧行(含注释形态)再追加,幂等且取值不经 sed 解释。
set_env() {
  local key="$1" val="$2"
  $SUDO sed -i -E "/^#?[[:space:]]*${key}=/d" "$ENVFILE"
  printf '%s=%s\n' "$key" "$val" | $SUDO tee -a "$ENVFILE" >/dev/null
}

gen_secret() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 24
  else
    head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'
  fi
}

# 探测已上传的 mihomo(二进制或 .gz),给交互默认值。
detect_mihomo() {
  local d p
  for d in "$PWD" "$HOME" ${SUDO_USER:+/home/$SUDO_USER} /root; do
    for p in "$d"/mihomo "$d"/mihomo*.gz; do
      [ -f "$p" ] && { echo "$p"; return; }
    done
  done
}

# ---------- 阶段 0:采集所有信息 ----------
log "采集部署信息"
yesno CCP_INSTALL_MIHOMO "是否配置本地代理 mihomo?海外直连可选 no [Y/n]: " yes
if [ "$CCP_INSTALL_MIHOMO" = "yes" ]; then
  det="$(detect_mihomo || true)"
  ask CCP_MIHOMO_BIN "已上传的 mihomo 二进制或 .gz 路径(回车=从 GitHub 下载)${det:+ [默认 $det]}: " "$det"
  ask CCP_SUB_URL "机场订阅链接(回车则稍后手填 /opt/mihomo/config.yaml): "
fi
MIHOMO_PORT="${CCP_MIHOMO_PORT:-7890}"

default_method="deb"
[ -f "$ROOT_DIR/Cargo.toml" ] && default_method="${CCP_INSTALL_METHOD:-deb}"
ask CCP_INSTALL_METHOD "ccProxy 安装方式 deb/source [${default_method}]: " "$default_method"
if [ "$CCP_INSTALL_METHOD" != "source" ]; then
  ask CCP_DEB_URL "ccProxy .deb 地址(回车=查 GitHub 最新;本地包用 file:///绝对路径): "
fi

ask CCP_ADMIN_KEY "管理密码 ADMIN_API_KEY(回车自动生成): "
ask CCP_DOMAIN "对外域名(设了才装 Caddy 走 HTTPS,回车跳过): "

# ---------- 阶段 1:基础依赖 ----------
log "安装基础依赖并开启 NTP"
$SUDO apt-get update -y
$SUDO apt-get install -y curl wget ca-certificates gnupg
$SUDO timedatectl set-ntp true 2>/dev/null || true

# ---------- 阶段 2:mihomo 代理(先配好,后续下载走它) ----------
UPSTREAM=""
if [ "$CCP_INSTALL_MIHOMO" = "yes" ]; then
  if [ -n "${CCP_MIHOMO_BIN:-}" ] && [ -f "$CCP_MIHOMO_BIN" ]; then
    log "使用本地 mihomo: $CCP_MIHOMO_BIN"
    if [[ "$CCP_MIHOMO_BIN" == *.gz ]]; then
      tmp="$(mktemp)"
      gunzip -c "$CCP_MIHOMO_BIN" > "$tmp"
      $SUDO install -m 0755 "$tmp" /usr/local/bin/mihomo
      rm -f "$tmp"
    else
      $SUDO install -m 0755 "$CCP_MIHOMO_BIN" /usr/local/bin/mihomo
    fi
  elif [ -x /usr/local/bin/mihomo ]; then
    log "复用已安装的 /usr/local/bin/mihomo"
  else
    ver="${CCP_MIHOMO_VERSION:-$DEFAULT_MIHOMO_VERSION}"
    case "$(uname -m)" in
      x86_64)  march="amd64-compatible" ;;
      aarch64) march="arm64" ;;
      *) die "未知架构 $(uname -m),请上传 mihomo 并用 CCP_MIHOMO_BIN 指定" ;;
    esac
    log "从 GitHub 下载 mihomo ${ver} (${march})"
    tmp="$(mktemp)"
    url="https://github.com/MetaCubeX/mihomo/releases/download/${ver}/mihomo-linux-${march}-${ver}.gz"
    curl -fsSL "$url" -o "${tmp}.gz" || die "mihomo 下载失败,建议上传后用 CCP_MIHOMO_BIN 指定: $url"
    gunzip -f "${tmp}.gz"
    $SUDO install -m 0755 "$tmp" /usr/local/bin/mihomo
    rm -f "$tmp"
  fi

  $SUDO mkdir -p /opt/mihomo/providers
  if [ -f /opt/mihomo/config.yaml ]; then
    log "已存在 /opt/mihomo/config.yaml,保留不覆盖"
  elif [ -n "${CCP_SUB_URL:-}" ]; then
    log "写入 mihomo 订阅配置"
    $SUDO tee /opt/mihomo/config.yaml >/dev/null <<EOF
mixed-port: ${MIHOMO_PORT}
allow-lan: false
mode: rule
log-level: warning

proxy-providers:
  airport:
    type: http
    url: "${CCP_SUB_URL}"
    interval: 3600
    path: ./providers/airport.yaml
    health-check: { enable: true, url: https://www.gstatic.com/generate_204, interval: 300 }

proxy-groups:
  - name: PROXY
    type: url-test
    use: [airport]
    url: https://www.gstatic.com/generate_204
    interval: 300

rules:
  - MATCH,PROXY
EOF
  else
    warn "未提供订阅,写入占位配置。编辑 /opt/mihomo/config.yaml 填节点后 systemctl restart mihomo"
    $SUDO tee /opt/mihomo/config.yaml >/dev/null <<EOF
mixed-port: ${MIHOMO_PORT}
allow-lan: false
mode: rule
log-level: warning
proxies: []
proxy-groups:
  - { name: PROXY, type: select, proxies: [DIRECT] }
rules:
  - MATCH,PROXY
EOF
  fi

  log "安装并启动 mihomo systemd 服务"
  $SUDO tee /etc/systemd/system/mihomo.service >/dev/null <<'EOF'
[Unit]
Description=mihomo proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/mihomo -d /opt/mihomo
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable mihomo
  $SUDO systemctl restart mihomo
  UPSTREAM="http://127.0.0.1:${MIHOMO_PORT}"

  log "验证代理出口(等待订阅拉取和节点就绪)"
  ok=0
  for _ in 1 2 3 4 5 6; do
    if curl -x "$UPSTREAM" -fsS https://chatgpt.com -o /dev/null 2>/dev/null; then ok=1; break; fi
    sleep 3
  done
  if [ "$ok" -eq 1 ]; then
    log "代理可访问 chatgpt.com,后续下载将走此代理"
    export https_proxy="$UPSTREAM" http_proxy="$UPSTREAM" all_proxy="$UPSTREAM"
    export no_proxy="127.0.0.1,localhost,::1"
  else
    warn "代理暂时不通;后续 GitHub 下载可能失败。排查: journalctl -u mihomo -f"
    warn "可先修好代理再重跑,或用 CCP_DEB_URL=file:///本地路径 走本地 deb"
  fi
fi

# ---------- 阶段 3:安装 ccProxy ----------
if [ "$CCP_INSTALL_METHOD" = "source" ]; then
  [ -f "$ROOT_DIR/Cargo.toml" ] || die "source 模式需在仓库目录内运行"
  if ! command -v cargo >/dev/null 2>&1; then
    log "安装 Rust 工具链(经代理)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  log "源码编译并安装(server.sh)"
  "$ROOT_DIR/scripts/server.sh" install
else
  deb_url="${CCP_DEB_URL:-}"
  if [ -z "$deb_url" ]; then
    log "查询 GitHub 最新 release 的 .deb"
    deb_url="$(curl -fsSL "https://api.github.com/repos/${REPO_SLUG}/releases/latest" \
      | grep -oE '"browser_download_url":[[:space:]]*"[^"]+amd64\.deb"' \
      | head -1 | sed -E 's/.*"(https[^"]+)"/\1/')" || true
  fi
  [ -n "$deb_url" ] || die "拿不到 .deb 地址,请设 CCP_DEB_URL(可 file:///本地路径)或改用 source 模式"
  log "下载并安装 $deb_url"
  deb="$(mktemp --suffix=.deb)"
  curl -fsSL "$deb_url" -o "$deb"
  $SUDO dpkg -i "$deb" || $SUDO apt-get install -f -y
  rm -f "$deb"
fi

[ -f "$ENVFILE" ] || die "$ENVFILE 未生成,安装可能失败"

# ---------- 阶段 4:写配置并重启 ----------
ADMIN_KEY="${CCP_ADMIN_KEY:-}"
if [ -z "$ADMIN_KEY" ]; then
  ADMIN_KEY="$(gen_secret)"
  log "已自动生成 ADMIN_API_KEY"
fi

log "写入 $ENVFILE"
set_env HOST 127.0.0.1
set_env OPENAI_AUTH_MODE codex
set_env ADMIN_API_KEY "$ADMIN_KEY"
[ -n "${CCP_PROXY_KEY:-}" ] && set_env PROXY_API_KEY "$CCP_PROXY_KEY"
[ -n "$UPSTREAM" ] && set_env UPSTREAM_PROXY_URL "$UPSTREAM"
$SUDO chmod 600 "$ENVFILE"

log "重启 ccProxy"
$SUDO systemctl restart ccproxy
sleep 1
$SUDO systemctl --no-pager --full status ccproxy | head -n 8 || true

# ---------- 阶段 5:Caddy HTTPS 反代 ----------
if [ -n "${CCP_DOMAIN:-}" ]; then
  if ! command -v caddy >/dev/null 2>&1; then
    log "安装 Caddy"
    $SUDO apt-get install -y debian-keyring debian-archive-keyring apt-transport-https
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
      | $SUDO gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
      | $SUDO tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
    $SUDO apt-get update -y
    $SUDO apt-get install -y caddy
  fi
  log "配置 Caddy 反代 ${CCP_DOMAIN} -> 127.0.0.1:48317"
  $SUDO tee /etc/caddy/Caddyfile >/dev/null <<EOF
${CCP_DOMAIN} {
    reverse_proxy 127.0.0.1:48317
}
EOF
  $SUDO systemctl reload caddy || $SUDO systemctl restart caddy
  BASE_URL="https://${CCP_DOMAIN}"
else
  BASE_URL="http://127.0.0.1:48317"
fi

# ---------- 完成 ----------
printf '\n\033[1;32m部署完成。\033[0m\n' >&2
cat >&2 <<EOF

  Base URL   : ${BASE_URL}
  管理后台   : ${BASE_URL}/admin
  ADMIN_KEY  : ${ADMIN_KEY}

下一步:登录 Codex(无头服务器用设备码)
  curl -sS http://127.0.0.1:48317/auth/device/start
  # 浏览器打开返回的 verification_url,输入 user_code 授权,然后轮询:
  curl -sS "http://127.0.0.1:48317/auth/device/poll?device_auth_id=...&user_code=..."
  curl -sS http://127.0.0.1:48317/auth/status

运维:
  sudo journalctl -u ccproxy -f
  sudo journalctl -u mihomo -f
  sudo systemctl restart ccproxy
EOF
