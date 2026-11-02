#!/usr/bin/env sh
# 并行跑测试二进制。
#
# 为什么需要它：`cargo test` 把每个测试目标编译成独立二进制，然后**串行**
# 依次执行。本项目 40+ 个目标里最慢的几个（`cli_remote` 全链路远程、
# `cas_properties` 真实落盘、`mcsup` 单元测试）各自十几二十秒，串行时总耗时
# 就是它们的和。这些用例 CPU 占用不高（大量时间在等子进程、等 IO、等超时），
# 并行执行能把这部分等待叠起来：实测（10 核 M 系列，热缓存）串行 33.5s、
# 十路并发 11.9s；只算运行阶段是 29.2s → 12.0s。
#
# 为什么不直接用 nextest：它会引入一个新的外部依赖（CI 与本地都要装），而这里
# 需要的只是「多个二进制同时跑」。几十行 shell 就够，且行为完全可见。
#
# 与 `cargo test` 的一致性：
# - 编译、feature 组合、profile 全部走同一次 `cargo test --no-run`，不另建
#   target 目录，不给 CI 增加第二套编译
# - 任一目标失败即非零退出，并汇总失败的二进制名与完整输出
#
# 用法：
#   sh scripts/test-parallel.sh                    # 全部测试目标
#   sh scripts/test-parallel.sh cli_e2e            # 只跑 apps/cli 的某个目标
#   MCSUP_TEST_JOBS=4 sh scripts/test-parallel.sh  # 限制并发（默认 CPU 数）

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# 与其他构建共用一份缓存；本脚本也能单独运行，所以要自己接入。
. ./scripts/dev-tools-env.sh
mcsup_use_shared_target

# ---- 子进程模式：跑单个目标，结果写进 outdir ----
#
# 由 `xargs -P` 并发调起。这里不用 `wait -n`：它是 bash 扩展，dash
# （Debian / Ubuntu 的 /bin/sh）会直接报 `Illegal option -n`。
if [ -n "${MCSUP_TEST_OUTDIR:-}" ]; then
	idx=$1
	bin=$2
	if "$bin" >"$MCSUP_TEST_OUTDIR/$idx.out" 2>&1; then
		: >"$MCSUP_TEST_OUTDIR/$idx.ok"
	fi
	exit 0
fi

# 并发数：默认 CPU 数的 1.5 倍（至少 2）。
#
# 这些用例大部分时间不在算，而在等：等子进程起停、等 TCP/握手、等 50~200ms
# 的轮询睡眠、等文件落盘。因此适度超订比「一个核跑一个目标」更快——
# 10 核实测：1.0×（10）11.6s、1.4×（14）9.3s、1.8×（18）10.0s。超过 1.5× 后
# 收益消失（争抢开始抵消等待），所以取 1.5×。
jobs=${MCSUP_TEST_JOBS:-}
if [ -z "$jobs" ]; then
	cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
	jobs=$((cores + cores / 2))
	[ "$jobs" -ge 2 ] || jobs=2
fi

# 只跑指定目标时用 `-p cli --test <name>`；否则全 workspace。
if [ "$#" -gt 0 ]; then
	cargo_args="-p cli --all-features --test $1"
else
	cargo_args="--workspace --all-features"
fi

printf '\n==> 编译测试目标\n'
json="${TMPDIR:-/tmp}/mcsup-test-targets-$$.json"
targets="${TMPDIR:-/tmp}/mcsup-test-targets-$$.txt"
# shellcheck disable=SC2086 # cargo_args 需要按空格拆开
cargo test $cargo_args --no-run --message-format=json >"$json"

# 只取真正带 test harness 的目标：bin 目标也会出现在 artifact 里，但它的
# `kind` 是 `bin`，直接执行会跑成 CLI 本身（返回用法码 2）。同一路径可能
# 出现多次（test harness 与 bin 目标共享 executable），去重。
if command -v python3 >/dev/null 2>&1; then
python3 - "$json" >"$targets" <<'PY'
import json, sys

seen = []
for line in open(sys.argv[1], encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") != "compiler-artifact" or not m.get("executable"):
        continue
    if "bin" in m.get("target", {}).get("kind", []):
        continue
    exe = m["executable"]
    if exe not in seen:
        seen.append(exe)
for exe in seen:
    print(exe)
PY
else
	# 没有 python3（精简镜像）时退回 grep：artifact 一行一个 JSON，
	# `"executable"` 与 `"kind"` 同行；排除 kind 含 bin 的目标。
	grep -o '"executable":"[^"]*"' "$json" |
		sed -e 's/.*:"//' -e 's/"$//' |
		grep -v "/debug/mcsup$" |
		awk '!seen[$0]++' >"$targets"
fi

count=$(grep -c . "$targets" || true)
if [ "${count:-0}" -eq 0 ]; then
	printf '失败：没有找到任何测试目标\n' >&2
	rm -f "$json" "$targets"
	exit 1
fi
printf '共 %s 个测试目标，并发 %s\n\n' "$count" "$jobs"

outdir="${TMPDIR:-/tmp}/mcsup-test-parallel.$$"
numbered="${TMPDIR:-/tmp}/mcsup-test-numbered-$$.txt"
mkdir -p "$outdir"
trap 'rm -rf "$outdir" "$json" "$targets" "$numbered"' EXIT INT TERM

# 给每个目标编号，供子进程回写结果用
awk '{ printf "%d\t%s\n", NR, $0 }' "$targets" >"$numbered"

MCSUP_TEST_OUTDIR="$outdir" xargs -P "$jobs" -n 2 sh "$0" <"$numbered" || true
wait

# 汇总：通过的只打一行摘要，失败的在末尾打印完整输出
failed=0
while IFS="$(printf '\t')" read -r idx bin; do
	summary=$(grep -E '^test result' "$outdir/$idx.out" 2>/dev/null | tail -1 || true)
	if [ -f "$outdir/$idx.ok" ]; then
		printf '  ok      %-46s %s\n' "$(basename "$bin")" "$summary"
	else
		failed=$((failed + 1))
		printf '  FAILED  %-46s %s\n' "$(basename "$bin")" "$summary"
	fi
done <"$numbered"

if [ "$failed" -ne 0 ]; then
	printf '\n%s 个测试目标失败，输出如下：\n' "$failed"
	while IFS="$(printf '\t')" read -r idx bin; do
		if [ ! -f "$outdir/$idx.ok" ]; then
			printf '\n===== %s =====\n' "$(basename "$bin")"
			cat "$outdir/$idx.out"
		fi
	done <"$numbered"
	exit 1
fi

printf '\n全部测试目标通过。\n'
