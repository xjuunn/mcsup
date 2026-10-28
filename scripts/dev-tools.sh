#!/usr/bin/env sh
# dev-tools MCP 启动器（POSIX）。
#
# 与 dev-tools.cmd 同职责，见那边的注释：所有 worktree 共用一份构建产物，产物
# 位置由 `git rev-parse --git-common-dir` 决定，而配置里没法执行 git 命令。
set -eu

common=$(git rev-parse --git-common-dir 2>/dev/null) || {
	printf '[dev-tools] 无法定位 git 公共目录（不在 git 仓库内？）\n' >&2
	exit 1
}

exe="$common/mcsup-build/debug/mcsup-dev-tools"
if [ ! -x "$exe" ]; then
	printf '[dev-tools] 找不到 %s\n' "$exe" >&2
	printf '[dev-tools] 先构建一次：cargo build -p dev --bin mcsup-dev-tools\n' >&2
	exit 1
fi

exec "$exe" "$@"
