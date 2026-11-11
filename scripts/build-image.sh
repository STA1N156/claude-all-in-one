#!/usr/bin/env bash
# 从 **git 提交** 构建生产镜像 —— 源码只认 git,不再用服务器上的源码目录。
#
#   CAIO_SSH="ssh root@<主机>" ./scripts/build-image.sh <标签前缀> [提交,默认 HEAD]
#
# 例:CAIO_SSH="ssh caio" ./scripts/build-image.sh layering
#     → 镜像 claude-all-in-one:layering-<7 位提交号>,二进制里编进完整提交号。
#
# 为什么:10-23 起三次回归(自动模式分类器失效、逐条筛查丢失、kimi-k3 未提交)都是
# 在服务器源码目录里直接构建造成的 —— 那份源码不受 git 管,落后于线上也没人知道。
# 本脚本用 `git archive` 打包**已提交**的内容(未提交的改动、.gitignore 里的真实配置
# 一律不会进去),在服务器的独立目录里构建。
#
# 只构建,不切换线上容器。切换照旧改 docker-compose.yml 的镜像标签,须运维确认。
# 上线后用 `/health` 的 build_commit 核对线上跑的就是这个提交。
#
# 环境变量:
#   CAIO_SSH         连服务器的命令(必填,如 "ssh root@host" 或带 sshpass 的包装脚本)
#   CAIO_BUILD_ROOT  服务器上的构建目录根(默认 /root/caio-builds)
set -euo pipefail

if [ $# -lt 1 ] || [ -z "${CAIO_SSH:-}" ]; then
    sed -n '2,6p' "$0" >&2
    exit 2
fi
TAG_PREFIX="$1"
REV="${2:-HEAD}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SHA="$(git rev-parse --verify "${REV}^{commit}")"
SHORT="${SHA:0:7}"
TAG="claude-all-in-one:${TAG_PREFIX}-${SHORT}"
BUILD_ROOT="${CAIO_BUILD_ROOT:-/root/caio-builds}"
REMOTE_DIR="${BUILD_ROOT}/${SHORT}"

if [ "$REV" = "HEAD" ] && ! git diff --quiet HEAD --; then
    echo "注意:工作树有未提交改动,它们不会进镜像(构建的是提交 ${SHORT})。" >&2
fi

echo "== 打包提交 ${SHA} → 服务器 ${REMOTE_DIR}"
git archive --format=tar "$SHA" \
    | $CAIO_SSH "rm -rf '${REMOTE_DIR}' && mkdir -p '${REMOTE_DIR}' && tar -x -C '${REMOTE_DIR}'"

echo "== 构建 ${TAG}(日志 ${REMOTE_DIR}.build.log)"
$CAIO_SSH "cd '${REMOTE_DIR}' && docker build --network=host \
    --build-arg GW_BUILD_COMMIT='${SHA}' -t '${TAG}' . > '${REMOTE_DIR}.build.log' 2>&1 \
    || { tail -30 '${REMOTE_DIR}.build.log'; exit 1; }"

echo "== 核对二进制里的提交号"
hits="$($CAIO_SSH "docker run --rm --entrypoint sh '${TAG}' -c 'grep -c ${SHA} /usr/local/bin/claude-all-in-one'")"
if [ "${hits:-0}" -lt 1 ]; then
    echo "镜像 ${TAG} 的二进制里找不到提交号 ${SHA},不要上线" >&2
    exit 1
fi
echo "完成:${TAG}"
