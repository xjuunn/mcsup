#!/usr/bin/env sh
# 列出所有 worktree 的「该不该删」与磁盘占用。只读，不删除任何东西。
#
# 规则来自 docs/development-workflow.md：worktree 有提交即任务结束，应删除。
# 这条规则靠记忆执行不住，所以在这里把判断结果摊开。
#
# 用法：sh scripts/worktree-status.sh

set -u

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$root" || exit 1

base=${MCSUP_BASE:-origin/master}
if ! git rev-parse --verify --quiet "$base" >/dev/null; then
	base=master
fi

# 目录大小：优先 du（Git Bash / Linux / macOS），没有就跳过
dir_size() {
	if command -v du >/dev/null 2>&1; then
		du -sh "$1" 2>/dev/null | awk '{print $1}'
	else
		printf '?'
	fi
}

# 表头用 ASCII：Windows 终端默认不是 UTF-8 代码页，中文表头会错位
printf '%-9s %-6s %-6s %-5s %s\n' STATE BEHIND AHEAD DIRTY PATH

main_path=$(git worktree list --porcelain | sed -n 's/^worktree //p' | head -n 1)

git worktree list --porcelain | awk '
	/^worktree / { if (path != "") print path "\t" head; path = substr($0, 10); head = "" }
	/^HEAD /     { head = substr($0, 6) }
	END          { if (path != "") print path "\t" head }
' | while IFS="$(printf '\t')" read -r path head; do
	[ -n "$path" ] || continue
	norm=$(CDPATH='' cd -- "$path" 2>/dev/null && pwd) || norm=$path
	label=worktree

	if [ ! -d "$path" ]; then
		printf '%-9s %-6s %-6s %-5s %s（目录不存在，git worktree prune 可清理）\n' \
			MISSING - - - "$path"
		continue
	fi

	dirty=$(git -C "$path" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
	ahead=$(git -C "$path" rev-list --count "$base..HEAD" 2>/dev/null || echo '?')
	behind=$(git -C "$path" rev-list --count "HEAD..$base" 2>/dev/null || echo '?')

	if [ "$path" = "$main_path" ]; then
		label=main
	elif [ "$dirty" != "0" ]; then
		label=dirty
	elif [ "$ahead" = "0" ]; then
		label=removable
	elif [ "$ahead" = "?" ]; then
		label=unknown
	else
		label=pending
	fi

	size=$(dir_size "$path/target")
	printf '%-9s %-6s %-6s %-5s %s  target=%s\n' \
		"$label" "$behind" "$ahead" "$dirty" "$path" "$size"
done

cat <<'EOF'

BEHIND/AHEAD 相对 origin/master；DIRTY 为未提交条目数。
main      当前仓库，不删。
dirty     有未提交改动：任务未完成，继续在其中做。
pending   有提交未合入：待合入。
removable 无改动且已全部合入：按规范应删掉。
删除：git worktree remove <路径>
EOF
