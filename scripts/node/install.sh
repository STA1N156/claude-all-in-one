#!/usr/bin/env bash
# caio 子机安装/升级/卸载 —— **系统级(root)版本**。
# 没有 sudo/root 的机器请用旁边的 install-user.sh(用户态版本)。
#
# 首次安装: sudo ./caio-node.run --token '<加入令牌>'
# 换主机:   sudo ./caio-node.run --token '<令牌>' --master <主机:端口> --fp '<证书指纹>'
# 升级:     sudo ./caio-node.run            (不带参数,沿用已有配置)
# 卸载:     sudo ./caio-node.run --uninstall [--purge]
#
# 这台机器只做出站连接:节点程序主动连主机 hub,dario 管理器只听 127.0.0.1。
# 不开放任何入站端口,所以 NAT 后、无公网 IP 的机器都能用。
set -euo pipefail

PREFIX=/opt/caio
CONF_DIR=/etc/caio
CONF=$CONF_DIR/node.env
DATA_DIR=/var/lib/caio
DARIO_DATA=$DATA_DIR/dario
SVC_NODE=caio-node.service
SVC_DARIO=caio-dario.service
RUN_USER=caio

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

TOKEN=""; MASTER=""; FP=""; SUP_URL=""
UNINSTALL=0; PURGE=0; NO_START=0
# 原始参数留一份:非 root 转交 install-user.sh 时要用(解析循环里的 shift 会把 $@ 吃空)。
ORIG_ARGS=("$@")

die() { printf '错误:%s\n' "$*" >&2; exit 1; }
info() { printf '[caio] %s\n' "$*"; }

usage() {
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    --token|--master|--fp|--sup-url)
      # 先验证有值再 shift:缺值时 shift 2 在 set -e 下会让脚本**静默**退出。
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

# 非 root:自动转交用户态版本(两个脚本在同一个包里)。
if [ "$(id -u)" != 0 ]; then
  if [ -f "$SRC/install-user.sh" ]; then
    info "当前不是 root,转用用户态安装(install-user.sh)…"
    exec bash "$SRC/install-user.sh" "${ORIG_ARGS[@]}"
  fi
  die "需要 root:请用 sudo 执行;没有 sudo 的机器请改用 install-user.sh"
fi
command -v systemctl >/dev/null 2>&1 || die "这台机器没有 systemd,系统级安装只支持 systemd 发行版(或用 install-user.sh 走用户态)"

# ── 卸载 ─────────────────────────────────────────────────────────────────────
if [ "$UNINSTALL" = 1 ]; then
  for s in $SVC_NODE $SVC_DARIO; do
    systemctl disable --now "$s" >/dev/null 2>&1 || true
    rm -f "/etc/systemd/system/$s"
  done
  systemctl daemon-reload
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
# 注意别写成 `ldd --version | grep -q ... | if`:pipefail + grep -q 时 ldd 会吃
# SIGPIPE(141),整个管道被判失败,好机器也报「没检测到 glibc」。
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

# 校验包内各文件。MANIFEST 与被校验的文件在**同一个未签名的包**里,所以它防的是
# 「传输/解压出错」,不是防篡改(改得了文件的人也改得了清单)。要验来源,比对后台
# 显示的 .run 整包 sha256。
if [ -f "$SRC/MANIFEST" ] && command -v sha256sum >/dev/null 2>&1; then
  ( cd "$SRC" && grep -E '^[0-9a-f]{64}  ' MANIFEST | sha256sum -c --quiet ) \
    || die "安装包内容与清单不符(下载或解压出错),请重新下载"
fi

# ── 账户与目录 ───────────────────────────────────────────────────────────────
if ! id "$RUN_USER" >/dev/null 2>&1; then
  nologin="$(command -v nologin || echo /bin/false)"
  useradd --system --home-dir "$DATA_DIR" --shell "$nologin" "$RUN_USER"
  info "已创建系统用户 $RUN_USER"
fi
# 终端(exec)最常干的事是看服务日志:把 caio 加进 systemd-journal 组,
# 不然 journalctl 读不到(组对**新进程**生效,已在跑的服务要重启才有)。
if getent group systemd-journal >/dev/null 2>&1 && ! id -nG "$RUN_USER" | tr ' ' '\n' | grep -qx systemd-journal; then
  usermod -aG systemd-journal "$RUN_USER" && info "已把 $RUN_USER 加入 systemd-journal 组(exec 可读服务日志)"
fi
install -d -m 0755 "$PREFIX" "$PREFIX/bin"
install -d -m 0750 -o root -g "$RUN_USER" "$CONF_DIR"
install -d -m 0750 -o "$RUN_USER" -g "$RUN_USER" "$DATA_DIR"
# 0700:号的令牌在这底下。
install -d -m 0700 -o "$RUN_USER" -g "$RUN_USER" "$DARIO_DATA"

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
# 打包时注入的默认主机/指纹:短令牌靠它们才知道往哪连。命令行与已有配置都优先。
if [ -f "$SRC/defaults.env" ]; then
  # shellcheck disable=SC1091
  . "$SRC/defaults.env"
  [ -n "$MASTER" ] || MASTER="${CAIO_DEFAULT_MASTER:-}"
  [ -n "$FP" ] || FP="${CAIO_DEFAULT_FP:-}"
fi

# 完整令牌(caio1.<主机>.<指纹>.<节点>.<密钥>)自带主机与指纹,不必再给。
case "$TOKEN" in
  caio1.*) ;;
  *) [ -n "$MASTER" ] && [ -n "$FP" ] || die "短令牌必须同时给 --master 和 --fp(或改用后台生成的完整令牌)" ;;
esac

# install_id:一台机器一个。主机首次连接时把令牌绑死在它上面,防同一个令牌被两台
# 机器用(那会让同一个号的令牌在两边各刷一次,整个号作废)。升级时必须保留。
#
# ⚠ 它就存在这个文件里,所以**整盘克隆或拷走 node.env 会把它一起带走**,两台机器都能
# 通过认证。要把这台子机迁到新硬件:先在新机器上删掉 /etc/caio/node.env(或别拷它),
# 再在后台点「解除机器绑定」,然后用原令牌安装。
if [ -z "$install_id" ]; then
  if [ -r /proc/sys/kernel/random/uuid ]; then install_id="$(cat /proc/sys/kernel/random/uuid)";
  else install_id="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"; fi
fi
# dario 管理器与各号 dario 进程共用的本机 key。只在这台机器上,不外传。
if [ -z "$dario_key" ]; then
  dario_key="$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
fi

umask 077
tmp_conf="$(mktemp "$CONF_DIR/.node.env.XXXXXX")"
{
  echo "# caio 子机配置。由安装脚本生成,含加入令牌与本机 key,不要外传。"
  echo "CAIO_TOKEN=$TOKEN"
  [ -n "$MASTER" ] && echo "CAIO_MASTER=$MASTER"
  [ -n "$FP" ] && echo "CAIO_FP=$FP"
  echo "CAIO_INSTALL_ID=$install_id"
  echo "CAIO_SUP_URL=$SUP_URL"
  echo "DARIO_API_KEY=$dario_key"
} > "$tmp_conf"
chmod 0640 "$tmp_conf"
chown root:"$RUN_USER" "$tmp_conf"
mv -f "$tmp_conf" "$CONF"

# ── 文件就位(先停服务,避免替换正在跑的二进制) ─────────────────────────────
upgrade=0
if [ -f "$PREFIX/bin/claude-all-in-one" ]; then upgrade=1; fi
if [ "$upgrade" = 1 ]; then
  info "检测到已安装,按升级处理(保留配置、令牌与各号凭证)"
  systemctl stop $SVC_NODE >/dev/null 2>&1 || true
  systemctl stop $SVC_DARIO >/dev/null 2>&1 || true
fi

install -m 0755 "$SRC/bin/claude-all-in-one" "$PREFIX/bin/claude-all-in-one"
install -m 0755 "$SRC/bin/bun" "$PREFIX/bin/bun"
# 留一份安装脚本供以后卸载/升级(.run 解包目录用完即删,一行命令里的 .run 装完也会自删)。
install -m 0755 "$SRC/install.sh" "$PREFIX/install.sh"
rm -rf "$PREFIX/dario" "$PREFIX/dario-supervisor"
cp -a "$SRC/dario" "$PREFIX/dario"
cp -a "$SRC/dario-supervisor" "$PREFIX/dario-supervisor"
[ -f "$SRC/MANIFEST" ] && install -m 0644 "$SRC/MANIFEST" "$PREFIX/MANIFEST"
chown -R root:root "$PREFIX"
install -m 0644 "$SRC/systemd/$SVC_DARIO" "/etc/systemd/system/$SVC_DARIO"
install -m 0644 "$SRC/systemd/$SVC_NODE" "/etc/systemd/system/$SVC_NODE"
systemctl daemon-reload

if [ "$NO_START" = 1 ]; then
  info "已安装到 $PREFIX(按要求没有启动服务)。启动:systemctl enable --now $SVC_DARIO $SVC_NODE"
  exit 0
fi

systemctl enable --now $SVC_DARIO
systemctl enable --now $SVC_NODE
# 本次启动时刻:下面只看这之后的日志。用「最近 2 分钟」会把上一轮安装留下的
# 「已连上主机」读成本次成功(参数写错、改完重跑的场景下必然误报)。
started_at="$(systemctl show -p ActiveEnterTimestamp --value $SVC_NODE 2>/dev/null || true)"
[ -n "$started_at" ] || started_at="-2min"

# ── 等它连上主机 ─────────────────────────────────────────────────────────────
node_id="$(printf '%s' "$TOKEN" | awk -F. '{ if ($1 == "caio1") print $4; else print $1 }')"
info "节点 $node_id 已启动,等它连上主机(最多 30 秒)…"
connected=0
for _ in $(seq 1 30); do
  sleep 1
  # 别用 grep -q:它会提前退出,journalctl 吃 SIGPIPE(141),pipefail 把整条管道
  # 判失败,「已连上」被误判成「没连上」。不带 -q 的 grep 会读完全部输入。
  if journalctl -u $SVC_NODE --since "$started_at" --no-pager 2>/dev/null | grep '已连上主机' >/dev/null; then
    connected=1; break
  fi
  if ! systemctl is-active --quiet $SVC_NODE; then break; fi
done

echo
if [ "$connected" = 1 ]; then
  info "已连上主机。现在可以在主机后台把 Claude 订阅号上到这台子机(节点 $node_id)。"
else
  info "还没连上。常见原因:主机 hub 端口没放行、主机地址/指纹不对、令牌已被吊销或绑了另一台机器。"
  info "看日志:journalctl -u $SVC_NODE -n 50 --no-pager"
fi
cat <<EOF

  节点 ID      $node_id
  主机         ${MASTER:-(令牌里自带)}
  程序         $PREFIX/bin/claude-all-in-one --mode node
  配置         $CONF              (0640,含令牌)
  号的令牌     $DARIO_DATA        (0700,**务必纳入备份**:丢了这些号要重新登录)
  服务         systemctl status $SVC_NODE $SVC_DARIO
  日志         journalctl -u $SVC_NODE -f
  卸载         sudo bash $PREFIX/install.sh --uninstall(加 --purge 连令牌一起清)
  入站端口     无(只做出站连接)
EOF
