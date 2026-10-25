#!/usr/bin/env sh
# CLI 端到端门禁：真实二进制 + 真实临时目录，跑完整命令链路。
#
# 与 check.sh 的关系：check.sh 是提交级门禁（单元 / 契约测试，必须快）；
# 本脚本是端到端层，跑一次需要真实进程与磁盘，单独成步便于失败时只重跑这一层。
# check.sh 默认调用它，可用 MCSUP_SKIP_E2E=1 跳过（例如只改 docs 时）。
#
# 用法：sh scripts/check-e2e.sh

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

printf '\n==> e2e（CLI 端到端）\n'
cargo test -p cli --test cli_e2e --all-features
