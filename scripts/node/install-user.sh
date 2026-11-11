#!/usr/bin/env bash
# caio 子机安装/升级/卸载 —— **用户态版本(不需要 root/sudo)**。
# 系统级安装请用旁边的 install.sh(root)。
#
# 首次安装: bash ./install-user.sh --token '<加入令牌>'     (在解包目录里;或 .run 非 root 运行会自动转到这里)
# 升级:     bash ./install-user.sh            (不带参数,沿用已有配置)
# 卸载:     bash ./install-user.sh --uninstall [--purge]
#
# 与系统级版本的差别:
# - 装在 ~/.local/{lib,share,config}/caio 下,以当前用户运行,不写系统目录;
# - 有 systemd 用户会话就装用户服务(重启要 linger 才能自启,脚本会尝试 loginctl enable-linger);
# - 没有就 nohup 兜底(重启后不会自启,重跑 start.sh 即可);
# - 令牌文件权限不变(0600/0700),但因为跑在你的用户下,exec 通道的权限上限就是你本人。
#
# 这台机器只做出站连接:节点程序主动连主机 hub,dario 管理器只听 127.0.0.1。
# 不开放任何入站端口,所以 NAT 后、无公网 IP 的机器都能用。
set -euo pipefail

PREFIX="$HOME/.local/lib/caio"
CONF_DIR="$HOME/.config/caio"
CONF=$CONF_DIR/node.env
DATA_DIR="$HOME/.local/share/caio"
DARIO_DATA=$DATA_DIR/dario
LOG_DIR=$DATA_DIR/logs
SVC_NODE=caio-node.service
SVC_DARIO=caio-dario.service
UNIT_DIR="$HOME/.config/systemd/user"
RUN_USER="$(id -un)"

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

TOKEN=""; MASTER=""; FP=""; SUP_URL=""
UNINSTALL=0; PURGE=0; NO_START=0

die() { printf '错误:%s\n' "$*" >&2; exit 1; }
info() { printf '[caio] %s\n' "$*"; }

usage() {
  sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    --token|--master|--fp|--sup-url)
      [ -n "${2:-}" ] || die "$1 需要一个值"
      case "$1" in
        --token) TOKEN="$2" ;;
        --master) MASTER="$2" ;;
        --fp) FP="$2" ;;
        --sup-url) SUP_URL="$2" ;;
      esac
      shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    --purge) PURGE=1; shift ;;
    --no-start) NO_START=1; shift ;;
    -h|--help) usage ;;
    *) die "未知参数 $1(--help 看用法)" ;;
  esac
done

# systemd 用户会话探测:有 user bus 才用用户服务,否则 nohup。
USER_SYSTEMD=0
if command -v systemctl >/dev/null 2>&1 && systemctl --user status >/dev/null 2>&1; then
  USER_SYSTEMD=1
fi

# nohup 模式的进程停止(按本安装目录的路径杀,不碰别人的进程)。
nohup_stop() {
  pkill -f "$PREFIX/bin/claude-all-in-one" 2>/dev/null || true
  pkill -f "$PREFIX/dario-supervisor" 2>/dev/null || true
  pkill -f "$PREFIX/dario/dist/cli.js" 2>/dev/null || true
  pkill -f "$PREFIX/bin/bun" 2>/dev/null || true
}

# ── 卸载 ─────────────────────────────────────────────────────────────────────
if [ "$UNINSTALL" = 1 ]; then
  if [ "$USER_SYSTEMD" = 1 ]; then
    for s in $SVC_NODE $SVC_DARIO; do
      systemctl --user disable --now "$s" >/dev/null 2>&1 || true
      rm -f "$UNIT_DIR/$s"
    done
    systemctl --user daemon-reload
  else
    nohup_stop
  fi
  rm -rf "$PREFIX"
  if [ "$PURGE" = 1 ]; then
    rm -rf "$DATA_DIR" "$CONF_DIR"
    info "已卸载并清除 $DATA_DIR(各号的令牌已删除,这些号需要在别处重新登录)"
  else
    info "已卸载。保留 $DATA_DIR(各号令牌)与 $CONF(配置)"
    info "⚠ 保留下来的令牌**仍然有效**,而主机侧这些号的 refresh_token 在交接时已清空 ——"
    info "  这份就是唯一的副本:要么按密钥级别保管,要么 --uninstall --purge 销毁。"
    info "要一起删掉:再跑一次 --uninstall --purge"
  fi
  info "别忘了在主机后台吊销这台子机的令牌"
  exit 0
fi

# ── 环境检查 ─────────────────────────────────────────────────────────────────
arch="$(uname -m)"
[ "$arch" = x86_64 ] || die "只支持 x86_64,当前是 $arch"
[ -d "$SRC/bin" ] || die "安装包不完整:缺 bin/(请重新下载 .run)"

# bun 是 glibc 构建(baseline),musl 发行版(Alpine)跑不了。
ldd_out="$(ldd --version 2>&1 | head -3 || true)"
if ! printf '%s' "$ldd_out" | grep -qiE 'glibc|gnu libc'; then
  die "没检测到 glibc(Alpine 等 musl 发行版暂不支持)"
fi
glibc_ver="$(printf '%s' "$ldd_out" | grep -oE '[0-9]+\.[0-9]+' | tail -1 || true)"
if [ -n "$glibc_ver" ]; then
  major="${glibc_ver%%.*}"; minor="${glibc_ver##*.}"
  if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 17 ]; }; then
    die "glibc $glibc_ver 过旧(bun 需要 ≥ 2.17)"
  fi
fi

if [ -f "$SRC/MANIFEST" ] && command -v sha256sum >/dev/null 2>&1; then
  ( cd "$SRC" && grep -E '^[0-9a-f]{64}  ' MANIFEST | sha256sum -c --quiet ) \
    || die "安装包内容与清单不符(下载或解压出错),请重新下载"
fi

# ── 目录 ─────────────────────────────────────────────────────────────────────
info "用户态安装:当前用户 $RUN_USER,不写系统目录"
install -d -m 0755 "$PREFIX" "$PREFIX/bin"
install -d -m 0750 "$CONF_DIR"
install -d -m 0750 "$DATA_DIR"
install -d -m 0700 "$DARIO_DATA"   # 号的令牌在这底下
install -d -m 0755 "$LOG_DIR"

# ── 配置:命令行优先,其余沿用已有值 ─────────────────────────────────────────
get_conf() { [ -f "$CONF" ] && sed -n "s/^$1=//p" "$CONF" | tail -1 || true; }
old_token="$(get_conf CAIO_TOKEN)"
old_master="$(get_conf CAIO_MASTER)"
old_fp="$(get_conf CAIO_FP)"
old_sup="$(get_conf CAIO_SUP_URL)"
install_id="$(get_conf CAIO_INSTALL_ID)"
dario_key="$(get_conf DARIO_API_KEY)"

[ -n "$TOKEN" ] || TOKEN="$old_token"
[ -n "$TOKEN" ] || die "缺少加入令牌:在主机后台「子机」页新建一台,把那条命令整条贴过来"
[ -n "$MASTER" ] || MASTER="$old_master"
[ -n "$FP" ] || FP="$old_fp"
[ -n "$SUP_URL" ] || SUP_URL="${old_sup:-http://127.0.0.1:8788}"
if [ -f "$SRC/defaults.env" ]; then
  # shellcheck disable=SC1091
  . "$SRC/defaults.env"
  [ -n "$MASTER" ] || MASTER="${CAIO_DEFAULT_MASTER:-}"
  [ -n "$FP" ] || FP="${CAIO_DEFAULT_FP:-}"
fi

case "$TOKEN" in
  caio1.*) ;;
  *) [ -n "$MASTER" ] && [ -n "$FP" ] || die "短令牌必须同时给 --master 和 --fp(或改用后台生成的完整令牌)" ;;
esac

# install_id:一台机器一个,主机首次连接时把令牌绑死在它上面。升级必须保留;
# 拷走它 = 两台机器都能通过认证 = 同一个号两边各刷一次令牌 → 整族作废。
if [ -z "$install_id" ]; then
  if [ -r /proc/sys/kernel/random/uuid ]; then install_id="$(cat /proc/sys/kernel/random/uuid)";
  else install_id="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"; fi
fi
if [ -z "$dario_key" ]; then
  dario_key="$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
fi

umask 077
tmp_conf="$(mktemp "$CONF_DIR/.node.env.XXXXXX")"
{
  echo "# caio 子机配置(用户态)。由安装脚本生成,含加入令牌与本机 key,不要外传。"
  echo "CAIO_TOKEN=$TOKEN"
  [ -n "$MASTER" ] && echo "CAIO_MASTER=$MASTER"
  [ -n "$FP" ] && echo "CAIO_FP=$FP"
  echo "CAIO_INSTALL_ID=$install_id"
  echo "CAIO_SUP_URL=$SUP_URL"
  echo "DARIO_API_KEY=$dario_key"
} > "$tmp_conf"
chmod 0600 "$tmp_conf"
mv -f "$tmp_conf" "$CONF"

# ── 文件就位(先停,避免替换正在跑的二进制) ──────────────────────────────────
upgrade=0
if [ -f "$PREFIX/bin/claude-all-in-one" ]; then upgrade=1; fi
if [ "$upgrade" = 1 ]; then
  info "检测到已安装,按升级处理(保留配置、令牌与各号凭证)"
  if [ "$USER_SYSTEMD" = 1 ]; then
    systemctl --user stop $SVC_NODE >/dev/null 2>&1 || true
    systemctl --user stop $SVC_DARIO >/dev/null 2>&1 || true
  else
    nohup_stop
  fi
fi

install -m 0755 "$SRC/bin/claude-all-in-one" "$PREFIX/bin/claude-all-in-one"
install -m 0755 "$SRC/bin/bun" "$PREFIX/bin/bun"
install -m 0755 "$SRC/install-user.sh" "$PREFIX/install.sh"
install -m 0755 "$SRC/install-user.sh" "$PREFIX/install-user.sh"
rm -rf "$PREFIX/dario" "$PREFIX/dario-supervisor"
cp -a "$SRC/dario" "$PREFIX/dario"
cp -a "$SRC/dario-supervisor" "$PREFIX/dario-supervisor"
[ -f "$SRC/MANIFEST" ] && install -m 0644 "$SRC/MANIFEST" "$PREFIX/MANIFEST"

# ── 服务 ─────────────────────────────────────────────────────────────────────
# 用户态 unit 由系统级模板改写:去掉 User/Group 与 home 目录防护(数据本来就在 $HOME),
# 路径全部换成用户态路径。
gen_user_unit() {
  sed -e '/^User=/d' -e '/^Group=/d' -e '/^ProtectHome=/d' -e '/^ReadWritePaths=/d' \
      -e "s|/opt/caio|$PREFIX|g" \
      -e "s|/etc/caio/node.env|$CONF|g" \
      -e "s|/var/lib/caio/dario|$DARIO_DATA|g" \
      -e "s|/var/lib/caio|$DATA_DIR|g" \
      -e "s|^Environment=HOME=.*|Environment=HOME=$HOME|" \
      -e 's|^WantedBy=multi-user.target|WantedBy=default.target|' \
      "$SRC/systemd/$1"
}

write_nohup_scripts() {
  cat > "$PREFIX/start.sh" <<EOF
#!/usr/bin/env bash
# caio 子机启动(nohup 模式,无 systemd)。重启后不会自启,重跑本脚本即可。
set -euo pipefail
set -a; . "$CONF"; set +a
mkdir -p "$LOG_DIR"
SUPERVISOR_HOST=127.0.0.1 SUPERVISOR_PORT=8788 DARIO_CLI="$PREFIX/dario/dist/cli.js" \\
BUN_BIN="$PREFIX/bin/bun" DATA_DIR="$DARIO_DATA" DARIO_MAX_BODY_MB=24 HOME="$HOME" \\
  nohup "$PREFIX/bin/bun" "$PREFIX/dario-supervisor/src/main.ts" >> "$LOG_DIR/dario.log" 2>&1 &
nohup "$PREFIX/bin/claude-all-in-one" --mode node >> "$LOG_DIR/node.log" 2>&1 &
echo "caio 子机已启动(日志在 $LOG_DIR)"
EOF
  cat > "$PREFIX/stop.sh" <<EOF
#!/usr/bin/env bash
pkill -f "$PREFIX/bin/claude-all-in-one" 2>/dev/null || true
pkill -f "$PREFIX/dario-supervisor" 2>/dev/null || true
pkill -f "$PREFIX/dario/dist/cli.js" 2>/dev/null || true
pkill -f "$PREFIX/bin/bun" 2>/dev/null || true
echo "caio 子机已停止"
EOF
  chmod 0755 "$PREFIX/start.sh" "$PREFIX/stop.sh"
}

if [ "$USER_SYSTEMD" = 1 ]; then
  install -d -m 0755 "$UNIT_DIR"
  gen_user_unit $SVC_DARIO > "$UNIT_DIR/$SVC_DARIO"
  gen_user_unit $SVC_NODE > "$UNIT_DIR/$SVC_NODE"
  systemctl --user daemon-reload
  # 登出/重启后还要活着就得开 linger;开不了就明说(重启后手动 start)。
  if ! loginctl enable-linger "$RUN_USER" >/dev/null 2>&1; then
    info "⚠ loginctl enable-linger 没成功:登出/重启后服务会停,要手动 systemctl --user start $SVC_NODE"
  fi
else
  write_nohup_scripts
  info "没有 systemd 用户会话,按 nohup 模式跑(重启后不会自启,重跑 $PREFIX/start.sh)"
fi

if [ "$NO_START" = 1 ]; then
  info "已安装到 $PREFIX(按要求没有启动服务)"
  exit 0
fi

started_at=""
if [ "$USER_SYSTEMD" = 1 ]; then
  systemctl --user enable --now $SVC_DARIO
  systemctl --user enable --now $SVC_NODE
  started_at="$(systemctl --user show -p ActiveEnterTimestamp --value $SVC_NODE 2>/dev/null || true)"
else
  nohup_stop >/dev/null 2>&1 || true
  "$PREFIX/start.sh"
fi
[ -n "$started_at" ] || started_at="-2min"

# ── 等它连上主机 ─────────────────────────────────────────────────────────────
node_id="$(printf '%s' "$TOKEN" | awk -F. '{ if ($1 == "caio1") print $4; else print $1 }')"
info "节点 $node_id 已启动,等它连上主机(最多 30 秒)…"
connected=0
for _ in $(seq 1 30); do
  sleep 1
  # 别用 grep -q(提前退出 → 上游吃 SIGPIPE → pipefail 误判),grep 读完全部输入即可。
  if [ "$USER_SYSTEMD" = 1 ]; then
    if journalctl --user -u $SVC_NODE --since "$started_at" --no-pager 2>/dev/null | grep '已连上主机' >/dev/null; then
      connected=1; break
    fi
    if ! systemctl --user is-active --quiet $SVC_NODE; then break; fi
  else
    if grep '已连上主机' "$LOG_DIR/node.log" >/dev/null 2>&1; then connected=1; break; fi
    if ! pgrep -f "$PREFIX/bin/claude-all-in-one" >/dev/null; then break; fi
  fi
done

echo
if [ "$connected" = 1 ]; then
  info "已连上主机。现在可以在主机后台把 Claude 订阅号上到这台子机(节点 $node_id)。"
else
  info "还没连上。常见原因:主机 hub 端口没放行、主机地址/指纹不对、令牌已被吊销或绑了另一台机器。"
fi

if [ "$USER_SYSTEMD" = 1 ]; then
  svc_hint="systemctl --user status $SVC_NODE $SVC_DARIO"
  log_hint="journalctl --user -u $SVC_NODE -f"
else
  svc_hint="$PREFIX/stop.sh / $PREFIX/start.sh"
  log_hint="tail -f $LOG_DIR/node.log"
fi
cat <<EOF

  节点 ID      $node_id
  形态         用户态($RUN_USER)
  主机         ${MASTER:-(令牌里自带)}
  程序         $PREFIX/bin/claude-all-in-one --mode node
  配置         $CONF              (0600,含令牌)
  号的令牌     $DARIO_DATA        (0700,**务必纳入备份**:丢了这些号要重新登录)
  服务         $svc_hint
  日志         $log_hint
  卸载         bash $PREFIX/install-user.sh --uninstall(加 --purge 连令牌一起清)
  入站端口     无(只做出站连接)
EOF
