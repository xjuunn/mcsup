#!/usr/bin/env sh
# 共享构建缓存目录的定位与导出。
#
# 多个 worktree 各自编译会把整棵依赖树重新编一遍（冷缓存下数分钟）。本脚本让
# 所有 worktree 共用一份 `target/`，编译一次即可复用。
#
# 目录选在 `$(git rev-parse --git-common-dir)/mcsup-build`：
# `.git` 是所有 worktree 天然共享的同一处，不依赖 worktree 的目录名或相对位置，
# 也不需要把绝对路径写进受版本控制的文件。
#
# 用法（必须 source，不能直接执行——要改变调用者的环境）：
#
#   . scripts/dev-tools-env.sh
#
# 已显式设置 `CARGO_TARGET_DIR` 时不做任何事，尊重调用者的选择。

# 共享构建目录；不在 git 仓库里时输出空。
#
# `--git-common-dir` 在任意 worktree 中都指向同一个 git 目录；`cd` 进去用
# `pwd` 规范化，避免 Git Bash 下拿到 `C:/...` 与 MSYS 路径混用。
mcsup_shared_target_dir() {
	common=$(CDPATH='' cd -- "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd) || return 1
	[ -n "$common" ] || return 1
	printf '%s/mcsup-build' "$common"
}

# 导出 CARGO_TARGET_DIR（已设置则保留）。
mcsup_use_shared_target() {
	if [ -n "${CARGO_TARGET_DIR:-}" ]; then
		return 0
	fi
	shared=$(mcsup_shared_target_dir) || return 0
	mkdir -p "$shared" 2>/dev/null || return 0
	CARGO_TARGET_DIR=$shared
	export CARGO_TARGET_DIR
}
