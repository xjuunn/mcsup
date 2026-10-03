#!/usr/bin/env sh
# 本地质量门禁。flag 与 .github/workflows/test.yml 保持一致。

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# `|| status=$?` 而非直接调用：set -e 会立刻退出，来不及报出是哪一步。
run() {
	label=$1
	shift
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
run 'doc' env 'RUSTDOCFLAGS=-D warnings' cargo doc --workspace --all-features --no-deps

# 硬要求：跳过去本地绿灯就不再等于 CI 绿灯。
printf '\n==> feature powerset\n'
if ! cargo hack --version >/dev/null 2>&1; then
	printf '失败于：feature powerset\n' >&2
	printf '未安装：cargo install cargo-hack --locked\n' >&2
	exit 1
fi
status=0
cargo hack check --workspace --feature-powerset --depth 1 --all-targets || status=$?
if [ "$status" -ne 0 ]; then
	printf '失败于：feature powerset（%s）\n' "$status" >&2
	printf '失败组合的 --features 即触发它的 feature。\n' >&2
	exit "$status"
fi

# 审计类，不影响构建，缺工具就静默跳过。
if cargo deny --version >/dev/null 2>&1; then
	printf '\n==> deny\n'
	status=0
	cargo deny check || status=$?
	if [ "$status" -ne 0 ]; then
		printf '失败于：deny（%s）\n' "$status" >&2
		printf '许可与 advisory 见 deny.toml。\n' >&2
		exit "$status"
	fi
fi

printf '\n门禁通过。\n'