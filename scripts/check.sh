#!/usr/bin/env sh
# 本地质量门禁。flag 与 .github/workflows/test.yml 保持一致。
#
# 用法：
#   scripts/check.sh                        跑完整门禁
#   scripts/check.sh fmt clippy             只跑指定步骤（供 MCP 工具按需调用）
#   scripts/check.sh "feature powerset"     多词步骤要把整串作为一个参数传
#
# 不传参数时行为与 CI 完全一致；传参只用于开发期的局部验证。

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# 所有 worktree 共用一份构建缓存：冷缓存下整棵依赖树要编数分钟，而每个 worktree
# 各编一遍纯属重复。目录定位见脚本内注释。已显式设置 CARGO_TARGET_DIR 时不动。
. ./scripts/dev-tools-env.sh
mcsup_use_shared_target

# 选定要跑的步骤：无参数即全部。
#
# `architecture fmt check clippy test e2e doc` 是单 token；
# `feature powerset` 与 `deny` 含空格，作为整串一次传入（`"feature powerset"`）。
requested=''
if [ "$#" -gt 0 ]; then
	for arg in "$@"; do
		case " architecture fmt check clippy test e2e doc deny feature powerset " in
			*" $arg "*) ;;
			*)
				printf '未知步骤：%s\n' "$arg" >&2
				printf '可选：architecture / fmt / check / clippy / test / e2e / doc / deny\n' >&2
				printf '多词步骤要整串传：scripts/check.sh "feature powerset"\n' >&2
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
# 测试用并行脚本跑：`cargo test` 串行执行每个测试二进制，而本项目最慢的几个
# 各自十几秒、彼此不争抢资源（大量时间在等子进程 / IO / 超时），并行能把等待
# 叠起来。编译、feature 组合与 `cargo test` 完全一致（同一次 --no-run），
# 失败仍是非零退出，见 scripts/test-parallel.sh。
run 'test' sh scripts/test-parallel.sh

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
#
# 网络不可达时回退 `--offline`：`cargo deny` 默认每次都去拉 RustSec advisory
# 数据库，连不上 github 就直接非零退出。那与「依赖有漏洞或许可证不合规」是
# 两件事，却都表现为门禁红——结果是网络一抖动就把整道门禁跳过，反而更危险。
# 回退前先确认失败原因确实是取数据库失败（输出里有 fetch 相关报错），
# 真正的策略违规仍然照常失败。
if want 'deny'; then
	if cargo deny --version >/dev/null 2>&1; then
		printf '\n==> deny\n'
		deny_log="${TMPDIR:-/tmp}/mcsup-deny-$$.log"
		status=0
		cargo deny check >"$deny_log" 2>&1 || status=$?
		if [ "$status" -ne 0 ]; then
			# 只在「取不到 advisory 数据库」时回退，其余失败原样报出。
			if grep -qiE 'failed to (fetch|update)|Couldn.t connect|Failed to connect|network|Operation timed out|recv failure' "$deny_log"; then
				printf '提示：advisory 数据库不可达，改用 --offline（本地缓存的数据库）\n' >&2
				status=0
				cargo deny --offline check >"$deny_log" 2>&1 || status=$?
			fi
		fi
		cat "$deny_log"
		rm -f "$deny_log"
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