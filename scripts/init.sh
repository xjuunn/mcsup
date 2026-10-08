#!/usr/bin/env sh
# 项目初始化：安装门禁所需工具、启用 git hooks。可重复执行，已就绪的步骤自动跳过。

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
if [ "$(git config --local core.hooksPath || true)" = '.githooks' ]; then
	printf 'core.hooksPath 已指向 .githooks\n'
else
	git config core.hooksPath .githooks
	printf 'core.hooksPath -> .githooks\n'
fi

printf '\n初始化完成。运行 scripts/check.sh 验证门禁。\n'
