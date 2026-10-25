#!/usr/bin/env sh
# 本地质量门禁。flag 与 .github/workflows/test.yml 保持一致。
#
# 用法：
#   scripts/check.sh              跑完整门禁
#   scripts/check.sh fmt clippy   只跑指定步骤（供 MCP 工具按需调用）
#
# 不传参数时行为与 CI 完全一致；传参只用于开发期的局部验证。

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# 选定要跑的步骤：无参数即全部。
requested=''
if [ "$#" -gt 0 ]; then
	for arg in "$@"; do
		case " architecture fmt check clippy test doc deny " in
			*" $arg "*) ;;
			*)
				printf '未知步骤：%s\n' "$arg" >&2
				printf '可选：architecture / fmt / check / clippy / test / doc / deny\n' >&2
				exit 2
				;;
		esac
	done
	requested=" $* "
fi

# 该步骤是否应执行。
#
# 无参数时全部执行（与 CI 一致）。传参时只跑点名的步骤：
# `feature powerset` 与 `deny` 是独立步骤，不会因为跑了 fmt 就顺带执行——
# 否则「只跑 fmt」会实际编译整个 feature 组合，调用方等到的结果与请求不符。
want() {
	[ -z "$requested" ] && return 0
	case "$requested" in
		*" $1 "*) return 0 ;;
		*) return 1 ;;
	esac
}

run() {
	label=$1
	shift
	if ! want "$label"; then
		return 0
	fi
	printf '\n==> %s\n' "$label"
	status=0
	"$@" || status=$?
	if [ "$status" -ne 0 ]; then
		printf '失败于：%s（%s）\n' "$label" "$status" >&2
		exit "$status"
	fi
}

run 'architecture' sh scripts/check-architecture.sh
run 'fmt' cargo fmt --all -- --check
run 'check' cargo check --workspace --all-targets --all-features
run 'clippy' cargo clippy --workspace --all-targets --all-features -- -D warnings
run 'test' cargo test --workspace --all-features

# 端到端单独成步：失败时要一眼看出是契约测试挂的还是真实进程链路挂的。
# 只改文档 / 注释时可 MCSUP_SKIP_E2E=1 跳过，CI 不跳过。
if [ "${MCSUP_SKIP_E2E:-}" = "1" ]; then
	printf '\n==> e2e（已按 MCSUP_SKIP_E2E=1 跳过）\n'
else
	run 'e2e' sh scripts/check-e2e.sh
fi

run 'doc' env 'RUSTDOCFLAGS=-D warnings' cargo doc --workspace --all-features --no-deps

if want 'feature powerset'; then
	printf '\n==> feature powerset\n'
	if ! cargo hack --version >/dev/null 2>&1; then
		printf '失败于：feature powerset\n' >&2
		printf '未安装：cargo-hack。安装：scripts/init.sh\n' >&2
		exit 1
	fi
	status=0
	cargo hack check --workspace --feature-powerset --depth 1 --all-targets || status=$?
	if [ "$status" -ne 0 ]; then
		printf '失败于：feature powerset（%s）\n' "$status" >&2
		printf '失败组合的 --features 即触发它的 feature。\n' >&2
		exit "$status"
	fi
fi

# 审计类，不影响构建，缺工具就跳过并提示——静默跳过会让本地绿灯 ≠ CI 绿灯。
if want 'deny'; then
	if cargo deny --version >/dev/null 2>&1; then
		printf '\n==> deny\n'
		status=0
		cargo deny check || status=$?
		if [ "$status" -ne 0 ]; then
			printf '失败于：deny（%s）\n' "$status" >&2
			printf '许可与 advisory 见 deny.toml。\n' >&2
			exit "$status"
		fi
	else
		printf '\n提示：未安装 cargo-deny，已跳过 deny（CI 会执行）。安装：scripts/init.sh\n' >&2
	fi
fi

if [ -z "$requested" ]; then
	printf '\n门禁通过。\n'
else
	printf '\n指定步骤通过：%s\n' "$*"
fi