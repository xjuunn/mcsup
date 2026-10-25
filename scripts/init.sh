#!/usr/bin/env sh
# 项目初始化：安装门禁所需工具、启用 git hooks。可重复执行，已就绪的步骤自动跳过。
#
# hooks 走 core.hooksPath 而非 .git/hooks：worktree 与主仓库共享同一份 repository
# 配置，在主仓库配一次，所有 worktree 都生效。hooksPath 是相对当前工作树根目录
# 解析的，所以每个 worktree 各自命中同一份 .githooks 内容。

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

printf '\n==> toolchain\n'
if ! command -v cargo >/dev/null 2>&1; then
	printf '未检测到 cargo。先安装 rustup：https://rustup.rs\n' >&2
	exit 1
fi
printf 'rustc %s\n' "$(rustc --version)"

printf '\n==> cargo tools\n'
for tool in cargo-hack cargo-deny; do
	sub=${tool#cargo-}
	if cargo "$sub" --version >/dev/null 2>&1; then
		printf '%s 已安装\n' "$tool"
	else
		printf '安装 %s…\n' "$tool"
		cargo install "$tool" --locked
	fi
done

printf '\n==> git hooks\n'
# 判断依据是 git 解析出的 hooks 目录，不是配置值本身：worktree 里
# `git config --local` 读的是 per-worktree 配置，在主仓库配过的 hooksPath
# 在那里是空，据此判断会误报「没配」。
#
# `--git-path` 在 Windows 上返回 `C:/...`，Posix 的 `/*` 匹配不到，只能把
# 相对路径补到 `--show-toplevel` 上比较。
hook_ok() {
	toplevel=$(git rev-parse --show-toplevel) || return 1
	path=$(git rev-parse --git-path hooks) || return 1
	case $path in
		"$toplevel"/.githooks | .githooks) return 0 ;;
		*) return 1 ;;
	esac
}

if hook_ok; then
	printf 'core.hooksPath 已生效\n'
else
	git config core.hooksPath .githooks
	if ! hook_ok; then
		printf '设置 core.hooksPath 后 hooks 仍不在 .githooks。\n' >&2
		printf '当前目录须在 git 仓库内，且 core.hooksPath 未被 worktree 级配置覆盖。\n' >&2
		exit 1
	fi
	printf 'core.hooksPath -> .githooks\n'
fi

printf '\n初始化完成。运行 scripts/check.sh 验证门禁。\n'
