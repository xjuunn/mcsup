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

# Windows 直接回退到 `cargo test`。
#
# 两个硬性原因，都不是「懒得适配」：
# 1. `xargs` 会把 Windows 路径里的反斜杠当转义吃掉（`D:\a\mcsup` 变成
#    `D:amcsup`），所有目标路径都会失效；
# 2. `xargs -P` / `awk` / `getconf` 依赖 POSIX 环境，而 GitHub 的
#    `windows-latest` 上 `run:` 默认走 pwsh，这些工具不保证存在。
#
# 回退不降低覆盖：`cargo test --workspace --all-features` 跑的是同一组
# 用例（含 doctest），只是串行。CI 的 Windows 任务因此变慢，但结论一致。
case "$(uname -s 2>/dev/null || echo unknown)" in
MINGW* | MSYS* | CYGWIN* | Windows_NT*)
	printf '\n==> 在 Windows 上回退到 cargo test（串行）\n'
	if [ "$#" -gt 0 ]; then
		exec cargo test -p cli --all-features --test "$1"
	fi
	exec cargo test --workspace --all-features
	;;
esac

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

# 并发数：默认 CPU 数的 1.5 倍，再按内存与核数封顶。
#
# 这些用例大部分时间不在算，而在等：等子进程起停、等 TCP/握手、等 50~200ms
# 的轮询睡眠、等文件落盘。因此适度超订比「一个核一个目标」更快——10 核实测：
# 1.0×（10）11.6s、1.4×（14）9.3s、1.8×（18）10.0s。
#
# 但**不能无条件超订**：目标是同时适配 2 核 / 7GB 的 GitHub Action runner 和
# 低端笔记本。那边超订会直接触发内存压缩（本机 16GB 实测跑到 6.8GB 压缩、
# 61% CPU 空闲，二进制耗时翻倍），比不超订还慢。所以：
#   - 少于 4 核：不超订（小机器上争抢的代价高于等待的收益）
#   - 内存紧张时按每个任务 ~1GB 封顶（单个测试进程实测 4~40MB，但一轮里会
#     派生编译产物、临时目录与 JVM 替身，留足余量）
jobs=${MCSUP_TEST_JOBS:-}
if [ -z "$jobs" ]; then
	cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
	if [ "$cores" -lt 4 ]; then
		jobs=$cores
	else
		jobs=$((cores + cores / 2))
	fi

	# 可用内存（MB）：Linux 读 MemAvailable，macOS 没有该字段，用 vm_stat
	# 的 free+inactive+speculative 估算；都取不到就跳过，不为探测失败而失败。
	mem_mb=''
	if [ -r /proc/meminfo ]; then
		mem_mb=$(awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null || true)
	elif command -v vm_stat >/dev/null 2>&1; then
		mem_mb=$(vm_stat 2>/dev/null | awk '
			/page size of/ { ps = $8; gsub(/\./, "", ps) }
			/^Pages free/ { free = $3; gsub(/\./, "", free) }
			/^Pages inactive/ { inact = $3; gsub(/\./, "", inact) }
			/^Pages speculative/ { spec = $3; gsub(/\./, "", spec) }
			END {
				if (ps > 0) printf "%d", (free + inact + spec) * ps / 1048576
			}' || true)
	fi
	# 每个并发任务按 64MB 计。这个数是实测出来的，不是拍的：满载 41 个
	# 测试目标时，整轮可用内存的峰值增量只有 211MB（单进程 4~40MB，其余是
	# 临时目录与派生进程）。留 3 倍余量，取 64MB/任务——既能在 7GB 的
	# GitHub runner 上放开跑（7GB/64MB 远大于核数，不会成为限制），也只在
	# 内存真的吃紧时才收窄并发。早期版本按 1GB/任务封顶，结果本机被压到
	# 并发 3、整体变慢（10s → 15s），属于过度保守。
	if [ -n "$mem_mb" ] && [ "$mem_mb" -gt 0 ] 2>/dev/null; then
		mem_cap=$((mem_mb / 64))
		[ "$mem_cap" -ge 1 ] || mem_cap=1
		if [ "$jobs" -gt "$mem_cap" ]; then
			jobs=$mem_cap
		fi
	fi
	[ "$jobs" -ge 1 ] || jobs=1
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

# 只取「带 test harness 的产物」。
#
# 判据是 `profile.test == true`，**不是** `kind`。原先按 `kind` 排除 bin，
# 会连 bin 目标的单元测试一起丢掉：`mcsup` 有 208 个、`mcsup_dev_tools`
# 有 25 个单元测试写在 main.rs 里，它们被静默跳过（1174 vs 1407 passed），
# 门禁仍然绿——是最难发现的那种退化。
#
# cargo 对 bin 目标产出两份产物：`test=false` 的真二进制（用户执行的那个，
# 直接跑会变成 CLI 本身）与 `test=true` 的 harness 副本（跑单元测试）。
# 同一路径可能重复出现，去重。
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
    if not m.get("profile", {}).get("test"):
        continue
    exe = m["executable"]
    if exe not in seen:
        seen.append(exe)
for exe in seen:
    print(exe)
PY
else
	# 没有 python3（精简镜像）时退回 awk：artifact 一行一个 JSON，
	# 同行的 `"test"` 与 `"executable"` 都可逐行判断。
	#
	# 早期版本按路径白名单排除 bin，三个 bin 目标只挡住了 mcsup 一个；
	# `mcsup-agent` 被当成测试跑会一直挂着等 stdin，整个门禁就此卡死。
	# 现在按 `"test":false` 排除真二进制，同时保留 bin 的单元测试。
	awk '
		/"reason"[[:space:]]*:[[:space:]]*"compiler-artifact"/ &&
		/"test"[[:space:]]*:[[:space:]]*false/ { next }
		/"executable"[[:space:]]*:[[:space:]]*"[^"]+"/ {
			line = $0
			sub(/.*"executable"[[:space:]]*:[[:space:]]*"/, "", line)
			sub(/".*/, "", line)
			if (!(line in seen)) { seen[line] = 1; print line }
		}
	' "$json" >"$targets"
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
doclog="${TMPDIR:-/tmp}/mcsup-test-doc-$$.log"
mkdir -p "$outdir"
trap 'rm -rf "$outdir" "$json" "$targets" "$numbered" "$doclog"' EXIT INT TERM

# 给每个目标编号，供子进程回写结果用
awk '{ printf "%d\t%s\n", NR, $0 }' "$targets" >"$numbered"

# doctest 与测试二进制并行跑。
#
# 它**不在** `--no-run` 的 artifact 清单里：doctest 由 rustdoc 驱动，没有
# 独立可执行文件，所以必须单独一条 `cargo test --doc`。漏掉它不会有任何
# 报错——只是静默少跑 4 个用例（`mcsup` 1 个、`testkit` 3 个），门禁照样
# 绿。那正是「本地跑的和 `cargo test` 不一样」的典型退化，必须显式补上。
#
# 只在全量模式下跑：`--doc` 不能与 `--test <name>` 同时出现（cargo 直接报
# `can't mix --doc with other target selecting options`）。单目标模式本来就
# 是「只验这一个集成测试」，与 doctest 无关。
#
# 后台启动后立刻记下 pid，等主批次结束后再 wait 取退出码；不能写成
# `cmd || status=$? &`——`&` 的优先级低于 `||`，退出码会丢失。
if [ "$#" -eq 0 ]; then
	# shellcheck disable=SC2086 # cargo_args 需要按空格拆开
	cargo test $cargo_args --doc >"$doclog" 2>&1 &
	doc_pid=$!
else
	doc_pid=''
	: >"$doclog"
fi

MCSUP_TEST_OUTDIR="$outdir" xargs -P "$jobs" -n 2 sh "$0" <"$numbered" || true

# 必须先 `wait "$doc_pid"` 再 `wait`。
#
# 裸 `wait` 会回收**所有**子进程，包括 doctest 那个；之后再 `wait "$doc_pid"`
# 时它已经不是「待回收的子进程」，sh 返回 127（`not a child of this shell`）。
# 于是 doctest 明明全通过（`4 passed; 0 failed`），退出码却是 127，门禁永远红
# ——而摘要那行恰恰只打印 passed/failed 计数，看起来像「通过了却报失败」。
#
# 先取 doctest 的退出码，再裸 `wait` 收尾其余进程。
doc_status=0
if [ -n "$doc_pid" ]; then
	wait "$doc_pid" || doc_status=$?
fi
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

# doctest 的摘要：把各 crate 的 `test result` 行累加成一个总数。
# 单目标模式没有 doctest（见上），这里整段跳过。
if [ -n "$doc_pid" ]; then
	doc_summary=$(
		grep -E '^test result' "$doclog" 2>/dev/null |
			awk '{ p += $4; f += $6 } END { printf "%d passed; %d failed", p, f }' || true
	)
	[ -n "$doc_summary" ] || doc_summary="（没有 doctest 输出）"
	if [ "$doc_status" -eq 0 ]; then
		printf '  ok      %-46s %s\n' "doctest（全部 crate）" "$doc_summary"
	else
		failed=$((failed + 1))
		printf '  FAILED  %-46s %s\n' "doctest（全部 crate）" "$doc_summary"
	fi
fi

if [ "$failed" -ne 0 ]; then
	printf '\n%s 个测试目标失败，输出如下：\n' "$failed"
	while IFS="$(printf '\t')" read -r idx bin; do
		if [ ! -f "$outdir/$idx.ok" ]; then
			printf '\n===== %s =====\n' "$(basename "$bin")"
			cat "$outdir/$idx.out"
		fi
	done <"$numbered"
	if [ "$doc_status" -ne 0 ]; then
		printf '\n===== doctest（全部 crate） =====\n'
		cat "$doclog"
	fi
	exit 1
fi

printf '\n全部测试目标通过。\n'
