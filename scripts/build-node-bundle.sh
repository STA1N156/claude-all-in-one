#!/usr/bin/env bash
# 打子机一键安装包:产出 dist-node/caio-node-<版本>.run(自解压 shell + tar.gz)。
#
#   ./scripts/build-node-bundle.sh                        # 用默认值全量打包
#   ./scripts/build-node-bundle.sh --skip-musl            # 复用上次编好的二进制
#   ./scripts/build-node-bundle.sh --dario-dist /root/dario-upstream/dist
#
# 包里有:
#   bin/claude-all-in-one   musl 静态二进制(`--mode node`),不带 embed-ui
#   bin/bun                 与主机同版本的 bun(baseline 构建,glibc ≥ 2.17)
#   dario/dist              原版 dario 的编译产物(与生产 /root/dario-upstream 同一提交)
#   dario-supervisor/       本仓库的管理器源码
#   systemd/                两个 unit
#   install.sh  MANIFEST
#
# 默认主机地址与证书指纹**打包时注入**,这样子机只需要后台给的短令牌。
# 两个值从 config/node-bundle.env 读(该文件在 .gitignore 里,仓库是公开的,不写真实 IP):
#   CAIO_DEFAULT_MASTER=203.0.113.7:38993
#   CAIO_DEFAULT_FP=<后台「子机」页显示的证书指纹>
# 没有这个文件也能打包,只是安装时要自己带 --master/--fp(或用完整令牌)。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT_DIR="$ROOT/dist-node"
WORK="$ROOT/.tmp-node-build"
BUN_VERSION="${BUN_VERSION:-1.3.14}"
# 与生产 /root/dario-upstream 同源:上游 v6.12.28,即 dario-caio 那次合并的上游侧父提交。
DARIO_SRC="${DARIO_SRC:-$ROOT/../dario-caio}"
DARIO_REF="${DARIO_REF:-f41ff10}"
DARIO_DIST=""
RUST_IMAGE="${RUST_IMAGE:-rust:1.95-alpine}"
BUN_IMAGE="${BUN_IMAGE:-oven/bun:1}"
SKIP_MUSL=0
DEFAULTS_FILE="$ROOT/config/node-bundle.env"

die() { printf '错误:%s\n' "$*" >&2; exit 1; }
info() { printf '[bundle] %s\n' "$*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT_DIR="$2"; shift 2 ;;
    --dario-dist) DARIO_DIST="$2"; shift 2 ;;
    --dario-src) DARIO_SRC="$2"; shift 2 ;;
    --dario-ref) DARIO_REF="$2"; shift 2 ;;
    --bun-version) BUN_VERSION="$2"; shift 2 ;;
    --defaults) DEFAULTS_FILE="$2"; shift 2 ;;
    --skip-musl) SKIP_MUSL=1; shift ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "未知参数 $1" ;;
  esac
done

command -v docker >/dev/null 2>&1 || die "需要 docker(musl 交叉编译与 dario 构建都在容器里做)"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
BUILD="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
[ -n "$VERSION" ] || die "读不出 Cargo.toml 里的 version"

mkdir -p "$WORK" "$OUT_DIR"
PAY="$WORK/payload"
rm -rf "$PAY"
mkdir -p "$PAY/bin" "$PAY/systemd"

# ── 1. musl 静态二进制 ───────────────────────────────────────────────────────
# 静态链接 = 不吃子机的 glibc 版本;registry/target 用命名卷,增量编译几分钟。
# 不要再加 `RUSTFLAGS=-C target-feature=+crt-static`:musl 目标本来就是静态的,
# 显式加上会让 proc-macro(async-stream-impl 等)编不出来——它们必须是动态库。
if [ "$SKIP_MUSL" = 1 ] && [ -f "$WORK/claude-all-in-one" ]; then
  info "复用 $WORK/claude-all-in-one"
else
  info "在 $RUST_IMAGE 里编 musl 静态二进制(首次约 10 分钟)…"
  docker run --rm --network=host \
    -e RUSTUP_TOOLCHAIN=1.95.0-x86_64-unknown-linux-musl \
    -e CARGO_TERM_COLOR=never \
    -e CAIO_BUILD="$BUILD" \
    -v "$ROOT":/app \
    -v "$WORK":/out \
    -v caio-cargo-registry:/usr/local/cargo/registry \
    -v caio-cargo-git:/usr/local/cargo/git \
    -v caio-target-musl:/app/target \
    -w /app "$RUST_IMAGE" \
    sh -c 'apk add --no-cache musl-dev >/dev/null && cargo build --release -p gw-app && cp target/release/claude-all-in-one /out/'
fi
cp "$WORK/claude-all-in-one" "$PAY/bin/claude-all-in-one"
chmod 0755 "$PAY/bin/claude-all-in-one"
# 必须是静态的:子机的 glibc 可能比构建机老。
if command -v file >/dev/null 2>&1; then
  # musl 目标出的是 static-pie(不是传统的 "statically linked"),两种写法都认。
  file "$PAY/bin/claude-all-in-one" | grep -qE 'static-pie|statically linked' \
    || die "二进制不是静态链接,换台机器会跑不起来"
fi

# ── 2. bun(与主机同版本的 baseline 构建) ───────────────────────────────────
# baseline:不要求 AVX2,老 CPU 的小机器也能跑;版本与主机一致 → dario 的 TLS 指纹一致。
BUN_ZIP="$WORK/bun-$BUN_VERSION-baseline.zip"
if [ ! -f "$WORK/bun-$BUN_VERSION/bun" ]; then
  info "下载 bun $BUN_VERSION(baseline)…"
  curl -fsSL -o "$BUN_ZIP" \
    "https://github.com/oven-sh/bun/releases/download/bun-v$BUN_VERSION/bun-linux-x64-baseline.zip"
  # 校验 fail-closed:取不到官方校验和就停,不「跳过」——装到客户机器上的东西不赌网络状态。
  curl -fsSL -o "$WORK/bun-SHASUMS256.txt" \
    "https://github.com/oven-sh/bun/releases/download/bun-v$BUN_VERSION/SHASUMS256.txt" \
    || die "取不到 bun 官方 SHASUMS256.txt,拒绝在未校验的情况下继续"
  want="$(awk '/bun-linux-x64-baseline\.zip/{print $1}' "$WORK/bun-SHASUMS256.txt" | head -1)"
  got="$(sha256sum "$BUN_ZIP" | cut -d' ' -f1)"
  [ -n "$want" ] && [ "$want" = "$got" ] || die "bun 压缩包校验不过(want=$want got=$got)"
  info "bun 校验通过"
  rm -rf "$WORK/bun-$BUN_VERSION"
  mkdir -p "$WORK/bun-$BUN_VERSION"
  if command -v unzip >/dev/null 2>&1; then
    unzip -q -j -o "$BUN_ZIP" '*/bun' -d "$WORK/bun-$BUN_VERSION"
  else
    # 没装 unzip 就借 alpine 容器里的 busybox unzip。
    docker run --rm -v "$WORK":/w alpine:3.22 \
      sh -c "unzip -q -j -o /w/$(basename "$BUN_ZIP") '*/bun' -d /w/bun-$BUN_VERSION" \
      || die "解压 bun 失败:请先装 unzip"
  fi
fi
cp "$WORK/bun-$BUN_VERSION/bun" "$PAY/bin/bun"
chmod 0755 "$PAY/bin/bun"

# ── 3. 原版 dario 的 dist ────────────────────────────────────────────────────
# 子机跑的必须是**原版**(不打 caio 补丁的)dario:每号一个进程、令牌由它独占刷新,
# 与主机上的 /root/dario-upstream 同一份。
if [ -n "$DARIO_DIST" ]; then
  [ -f "$DARIO_DIST/cli.js" ] || die "$DARIO_DIST 里没有 cli.js"
  info "用现成的 dario dist:$DARIO_DIST"
  # dario 自己要读 dist/../package.json 报版本(dist/version.js),少了就恒报 unknown。
  [ -f "$DARIO_DIST/../package.json" ] || die "$DARIO_DIST 旁边缺 package.json"
  mkdir -p "$PAY/dario"
  cp -a "$DARIO_DIST" "$PAY/dario/dist"
  cp "$DARIO_DIST/../package.json" "$PAY/dario/package.json"
  DARIO_VER="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$DARIO_DIST/../package.json" | head -1)"
  DARIO_VER="${DARIO_VER:-unknown}"
else
  [ -d "$DARIO_SRC/.git" ] || die "$DARIO_SRC 不是 git 仓库(用 --dario-src 指到 dario 源码,或 --dario-dist 给现成产物)"
  info "从 $DARIO_SRC 的 $DARIO_REF 构建原版 dario dist…"
  # 上一轮的 node_modules 是容器里 root 建的,本机用户删不掉 —— 删不掉就借个容器删。
  if [ -d "$WORK/dario-src" ]; then
    rm -rf "$WORK/dario-src" 2>/dev/null \
      || docker run --rm -v "$WORK":/w alpine:3.22 sh -c 'rm -rf "/w/dario-src"'
  fi
  mkdir -p "$WORK/dario-src"
  git -C "$DARIO_SRC" archive "$DARIO_REF" | tar -x -C "$WORK/dario-src"
  DARIO_VER="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$WORK/dario-src/package.json" | head -1)"
  info "dario 版本 $DARIO_VER(提交 $DARIO_REF)"
  # --network=host + 透传代理:这台机器的 npm 出网走宿主的 127.0.0.1 代理。
  docker run --rm --network=host \
    -e HTTP_PROXY -e HTTPS_PROXY -e http_proxy -e https_proxy -e NO_PROXY -e no_proxy \
    -v "$WORK/dario-src":/src -w /src "$BUN_IMAGE" \
    sh -c 'bun install --no-save --frozen-lockfile >/dev/null && bun run build'
  [ -f "$WORK/dario-src/dist/cli.js" ] || die "dario 构建没产出 dist/cli.js"
  mkdir -p "$PAY/dario"
  cp -a "$WORK/dario-src/dist" "$PAY/dario/dist"
  # 运行时要读 package.json(版本号、bin 入口)。
  cp "$WORK/dario-src/package.json" "$PAY/dario/package.json"
fi

# ── 4. 管理器 + unit + 安装脚本 ──────────────────────────────────────────────
mkdir -p "$PAY/dario-supervisor"
cp -a sidecars/dario-supervisor/src "$PAY/dario-supervisor/src"
cp sidecars/dario-supervisor/package.json "$PAY/dario-supervisor/package.json"
cp sidecars/dario-supervisor/README.md "$PAY/dario-supervisor/README.md"
cp scripts/node/caio-node.service scripts/node/caio-dario.service "$PAY/systemd/"
install -m 0755 scripts/node/install.sh "$PAY/install.sh"
# 用户态版本(rootless)一起进包:install.sh 在非 root 时会自动转交给它。
install -m 0755 scripts/node/install-user.sh "$PAY/install-user.sh"

# 默认主机地址与指纹:写进 install.sh 旁边的 defaults,安装时当缺省值用。
if [ -f "$DEFAULTS_FILE" ]; then
  # 只取这两个键,别把别的东西(key 之类)带进包里。
  grep -E '^(CAIO_DEFAULT_MASTER|CAIO_DEFAULT_FP)=' "$DEFAULTS_FILE" > "$PAY/defaults.env" || true
  if [ -s "$PAY/defaults.env" ]; then
    info "已注入默认主机/指纹(来自 $(basename "$DEFAULTS_FILE"))"
  else
    rm -f "$PAY/defaults.env"
  fi
else
  info "没有 $DEFAULTS_FILE:安装时需要完整令牌,或自己带 --master/--fp"
fi

# ── 5. MANIFEST(install.sh 用它校验包的完整性) ─────────────────────────────
( cd "$PAY" && {
    echo "# caio 子机安装包"
    echo "# caio=$VERSION+$BUILD bun=$BUN_VERSION dario=${DARIO_VER:-unknown} built=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    find . -type f ! -name MANIFEST -print0 | sort -z | xargs -0 sha256sum
  } > "$PAY/MANIFEST" )

# ── 6. 自解压 .run ───────────────────────────────────────────────────────────
TAR="$WORK/payload.tar.gz"
tar -czf "$TAR" -C "$PAY" .
RUN="$OUT_DIR/caio-node-$VERSION-$BUILD.run"
cat > "$RUN" <<'HEADER'
#!/usr/bin/env bash
# caio 子机安装包(自解压)。用法:sudo bash caio-node-*.run --token '<加入令牌>'
# 卸载:sudo bash caio-node-*.run --uninstall      其它参数见 --help
#
# 注意:不要用 `curl ... | bash` 的管道形态 —— bash -s 从 stdin 读脚本时会预读缓冲,
# 自解压负载取不全。后台给的一行命令是「先下载成文件再执行」,没有这个问题。
set -euo pipefail
SKIP=$(awk '/^__PAYLOAD__$/ { print NR + 1; exit 0; }' "$0")
# 解包目录可能到 ~200MB:/tmp 在小内存 VPS 上常是 tmpfs(实测 476MB 的都有),
# 空间不够就换 /var/tmp,别写一半 No space left。
TMPBASE=/tmp
_avail=$(df -Pk /tmp 2>/dev/null | awk 'NR==2{print $4}')
if [ -n "$_avail" ] && [ "$_avail" -lt 524288 ]; then TMPBASE=/var/tmp; fi
TMP="$(mktemp -d "$TMPBASE/caio-node.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
tail -n "+$SKIP" "$0" | tar -xz -C "$TMP"
# 不要 exec:进程被替换后 EXIT trap 不会触发,/tmp 会留下完整解包目录。
bash "$TMP/install.sh" "$@"
exit $?
__PAYLOAD__
HEADER
cat "$TAR" >> "$RUN"
chmod 0755 "$RUN"

info "产出 $RUN($(du -h "$RUN" | cut -f1))"
grep '^# caio=' "$PAY/MANIFEST"
cat <<EOF

  推荐:把包装到主机 data/node-bundle/caio-node.run,然后到后台「子机」页新建节点,
        复制那条「一串 bash」安装命令在子机上执行(下载 + 安装一条完成)。
  手动:传到子机:  scp '$(basename "$RUN")' root@<子机>:/root/
        在子机执行: sudo /root/$(basename "$RUN") --token '<后台给的令牌>'
EOF
