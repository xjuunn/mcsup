#!/usr/bin/env sh
# 清理已合入 worktree 占用的编译产物与目录。默认只预览，加 --yes 才真删。
#
# 为什么默认不做全局共享 CARGO_TARGET_DIR：多个 agent 同时跑 check.sh 会争用
# 同一份 target 的锁与构建产物，并行退化成本地排队；不同分支交替写入还会互相
# 触发重编译。要跨 worktree 复用编译结果，用 sccache（RUSTC_WRAPPER=sccache），
# 它只共享产物、不共享构建状态。
#
# 体积问题出在没有回收，不出在没有共享：及时删 worktree 才是解法。
#
# 判定与 scripts/worktree-status.sh 一致：无未提交改动且已全部合入 = 可删。
#
# 用法：
#   sh scripts/clean-worktrees.sh            预览
#   sh scripts/clean-worktrees.sh --yes      执行删除

set -u

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$root" || exit 1

base=${MCSUP_BASE:-origin/master}
if ! git rev-parse --verify --quiet "$base" >/dev/null; then
	base=master
fi

apply=no
for arg in "$@"; do
	case $arg in
		--yes) apply=yes ;;
		-h | --help)
			sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//' | sed 's/^#\{0,1\}$//'
			exit 0
			;;
		*)
			printf '未知参数：%s\n' "$arg" >&2
			exit 2
			;;
	esac
done

dir_size() {
	if command -v du >/dev/null 2>&1; then
		du -sh "$1" 2>/dev/null | awk '{print $1}'
	else
		printf '?'
	fi
}

main_path=$(git worktree list --porcelain | sed -n 's/^worktree //p' | head -n 1)

git worktree list --porcelain | awk '
	/^worktree / { if (path != "") print path; path = substr($0, 10) }
	END          { if (path != "") print path }
' | while IFS= read -r path; do
	[ -n "$path" ] || continue
	[ "$path" = "$main_path" ] && continue
	if [ ! -d "$path" ]; then
		printf '缺失 %s（可 git worktree prune）\n' "$path"
		continue
	fi
	dirty=$(git -C "$path" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
	ahead=$(git -C "$path" rev-list --count "$base..HEAD" 2>/dev/null || echo '?')
	if [ "$dirty" != "0" ] || [ "$ahead" != "0" ]; then
		printf '保留 %s（未提交 %s 项，未合入 %s 个提交）\n' "$path" "$dirty" "$ahead"
		continue
	fi
	size=$(dir_size "$path")
	if [ "$apply" = yes ]; then
		# 不加 --force：目录里有未跟踪文件时 git 会拒绝，那正是需要人确认的情况，
		# 自动清掉会丢掉没提交的东西。
		if ! git worktree remove "$path" >/dev/null 2>&1; then
			printf '删除失败（有未跟踪文件或进程占用，请人工处理）：%s\n' "$path" >&2
			continue
		fi
		printf '已删除 %s（回收 %s）\n' "$path" "$size"
	else
		printf '可删除 %s（%s）\n' "$path" "$size"
	fi
done

if [ "$apply" = no ]; then
	printf '\n这是预览。加 --yes 执行删除。\n'
fi
