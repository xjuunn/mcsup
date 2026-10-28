#!/usr/bin/env sh
# CLI 端到端门禁：真实二进制 + 真实临时目录，跑完整命令链路。
#
# 与 check.sh 的关系：check.sh 调用本脚本作为 `e2e` 一步，CI 也调用同一脚本。
# 单独成文件是为了让「本地跑了什么」和「CI 跑了什么」用同一份调用，而不是
# 各自拼 cargo 命令——那条路径曾经在 CI 里根本没被执行到。
#
# 单独重跑这一层：sh scripts/check-e2e.sh
#
# 用法：sh scripts/check-e2e.sh

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# 与其他构建共用一份缓存；本脚本也能单独运行，所以要自己接入。
. ./scripts/dev-tools-env.sh
mcsup_use_shared_target

printf '\n==> e2e（CLI 端到端）\n'
cargo test -p cli --test cli_e2e --all-features
