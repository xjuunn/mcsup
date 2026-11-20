#!/usr/bin/env sh
# 每个 worktree 一份独立构建缓存。
#
# 为什么不再共享：多个 worktree 指到同一个 `CARGO_TARGET_DIR` 时，同名包
# （`mcsup-*`）在不同分支上源码不同，Cargo 会把它们交替重编重链，并共用同一把
# 目录锁；两个 worktree 同时跑门禁就退化成排队，`check.sh` 从几十秒变成小时级。
# 各用一份 target 后，增量命中只看本 worktree 自己改了哪些 crate。
#
# 代价是磁盘：每份 1–4 GB。回收靠 `scripts/worktree-status.sh` 查看、
# `sh scripts/clean-worktrees.sh --yes` 删除，以及合入后立即删除 worktree。
#
# 目录选在 `$(git rev-parse --git-common-dir)/mcsup-build/<worktree 目录名>`：
# `.git` 是所有 worktree 天然共享的同一处，不依赖 worktree 的相对位置，也不需要
# 把绝对路径写进受版本控制的文件；主工作区与其 worktree 目录名必然不同。
#
# 用法（必须 source，不能直接执行——要改变调用者的环境）：
#
#   . scripts/dev-tools-env.sh
#
# 已显式设置 `CARGO_TARGET_DIR` 时不做任何事，尊重调用者的选择。

# 本源构建缓存目录；不在 git 仓库里时输出空。
#
# `--git-common-dir` 在任意 worktree 中都指向同一个 git 目录；`cd` 进去用
# `pwd` 规范化，避免 Git Bash 下拿到 `C:/...` 与 MSYS 路径混用。
#
# `--show-toplevel` 决定本 worktree 的标识：取目录名最后一段。worktree 之间
# 目录名唯一（`git worktree add` 拒绝重复），足以区分。
mcsup_local_target_dir() {
	common=$(CDPATH='' cd -- "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd) || return 1
	[ -n "$common" ] || return 1
	top=$(git rev-parse --show-toplevel 2>/dev/null) || return 1
	[ -n "$top" ] || return 1
	name=$(basename -- "$top")
	[ -n "$name" ] || return 1
	printf '%s/mcsup-build/%s' "$common" "$name"
}

# 导出 CARGO_TARGET_DIR（已设置则保留）。
mcsup_use_local_target() {
	if [ -n "${CARGO_TARGET_DIR:-}" ]; then
		return 0
	fi
	local_dir=$(mcsup_local_target_dir) || return 0
	mkdir -p "$local_dir" 2>/dev/null || return 0
	CARGO_TARGET_DIR=$local_dir
	export CARGO_TARGET_DIR
}
