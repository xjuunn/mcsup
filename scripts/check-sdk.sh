#!/usr/bin/env sh
# 插件 SDK 门禁（sdks/*）。
#
# sdks/ 不在 Cargo workspace 内，`cargo fmt` / `clippy` / `test` 覆盖不到；
# 由本脚本单独把门，逻辑与 scripts/check.sh 一致：任一失败即中止并报出步骤名。
#
# 用法：
#   scripts/check-sdk.sh             全部语言 SDK
#   scripts/check-sdk.sh typescript  只跑某个 SDK
#
# 未安装对应工具链的语言只提示并跳过；CI 上必须全部通过。

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

targets=${1:-all}

if [ ! -d sdks ]; then
	printf 'sdks/ 不存在，跳过\n'
	exit 0
fi

has_node() {
	command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1
}

run_ts() {
	sdk=sdks/typescript
	[ -d "$sdk" ] || return 0
	if ! has_node; then
		printf '提示：未安装 Node.js，跳过 typescript SDK\n' >&2
		return 0
	fi
	(
		cd "$sdk"
		printf '\n==> sdk typescript install\n'
		if [ -f package-lock.json ]; then
			npm ci --no-audit --no-fund
		else
			npm install --no-audit --no-fund
		fi
		printf '\n==> sdk typescript typecheck\n'
		npx tsc -p tsconfig.test.json
		printf '\n==> sdk typescript build\n'
		npm run build
		printf '\n==> sdk typescript test\n'
		npm test
	)
}

case "$targets" in
	all)
		run_ts
		;;
	typescript | ts)
		run_ts
		;;
	*)
		printf '未知 SDK：%s（可选：typescript / all）\n' "$targets" >&2
		exit 2
		;;
esac

printf '\nSDK 门禁通过。\n'