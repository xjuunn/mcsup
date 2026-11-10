#!/usr/bin/env sh
# 统一版本 bump：一处推进，全 workspace 锁步。
#
# 版本声明的两个位置：
# 1. 根 Cargo.toml 的 [workspace.package].version —— 所有包自身的版本
#    （各 manifest 以 version.workspace = true 继承）
# 2. 各 manifest 里内层互依的 version 字面量 —— 发布后 crates.io 要求
#    path 依赖同时给出版本要求，不能省略
#
# 本脚本同时推进这两处，并在写入后复核，任何残留旧值都直接失败。
# 发布条件与前置步骤见 docs/embedding.md 分发形态一节。
#
# 用法：
#   sh scripts/bump-version.sh 0.2.0          # bump 到 0.2.0
#   sh scripts/bump-version.sh 0.2.0 --check  # 只校验不写入

set -eu

usage() {
	printf '用法: sh scripts/bump-version.sh <新版本号> [--check]\n' >&2
	exit 2
}

[ $# -ge 1 ] || usage
new_version=$1
check=0
[ "${2:-}" = '--check' ] && check=1

# 版本号形状校验：major.minor.patch（预发布标签可选）
printf '%s' "$new_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$' || {
	printf '版本号形状非法: %s\n' "$new_version" >&2
	exit 2
}

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$root" || exit 1

manifest=Cargo.toml
old_version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -n 1)
[ -n "$old_version" ] || {
	printf '错误: 无法从 %s 读取 workspace.package.version\n' "$manifest" >&2
	exit 1
}

# 内层依赖键。testkit 只作 dev-dependency，仍随 workspace 锁步。
internal='auth|backend|backup|domain|file|java|modkit|mcsup|port|protocol|provider|runtime|service|storage|testkit|transport'

manifests="$manifest"
for f in crates/*/Cargo.toml apps/*/Cargo.toml; do
	[ -f "$f" ] && manifests="$manifests $f"
done

# 内层互依里所有带版本要求的行；写入与校验都只看这些行，
# 不会误改第三方依赖（serde / zip 等）的 version。
internal_version_lines() {
	grep -HnE "^($internal)[[:space:]]*=.*version = \"[^\"]+\"" $manifests 2>/dev/null || true
}

stale_internal() {
	[ -n "$1" ] || return 0
	printf '%s\n' "$1" | grep -v "version = \"$new_version\"" || true
}

current_lines=$(internal_version_lines)
stale=$(stale_internal "$current_lines")
current_count=$(printf '%s\n' "$current_lines" | grep -c . || true)

# 根版本已是目标值且内层互依已同步时无事可做
if [ "$old_version" = "$new_version" ]; then
	if [ -z "$stale" ]; then
		printf '版本已是 %s（内层互依 %s 处全部同步），无需 bump\n' "$new_version" "$current_count"
		exit 0
	fi
	printf '根已是 %s，但内层互依仍有旧值：\n%s\n' "$new_version" "$stale" >&2
	exit 1
fi

if [ "$check" -eq 1 ]; then
	printf 'bump %s -> %s：根 1 处 + 内层互依 %s 处\n' \
		"$old_version" "$new_version" "$current_count"
	exit 0
fi

# 先改根版本（只改 [workspace.package] 的第一处 version 行）。
tmp=$(mktemp "${TMPDIR:-/tmp}/mcsup-version.XXXXXX")
awk -v old="$old_version" -v new="$new_version" '
	BEGIN { done = 0 }
	!done && $0 ~ "^version[[:space:]]*=[[:space:]]*\"" old "\"" {
		sub("version[[:space:]]*=[[:space:]]*\"" old "\"", "version = \"" new "\"")
		done = 1
	}
	{ print }
' "$manifest" >"$tmp"
mv "$tmp" "$manifest"

# 再改各 manifest 的内层互依版本要求。
for f in $manifests; do
	tmp=$(mktemp "${TMPDIR:-/tmp}/mcsup-version.XXXXXX")
	sed -E "/^($internal)[[:space:]]*=/ s/version = \"$old_version\"/version = \"$new_version\"/" "$f" >"$tmp"
	mv "$tmp" "$f"
done

# 复核：任何一处旧值残留都失败，杜绝静默成功。
left=$(grep -HnE "version = \"$old_version\"" $manifests 2>/dev/null || true)
if [ -n "$left" ]; then
	printf '错误: 替换后仍有旧版本 %s：\n%s\n' "$old_version" "$left" >&2
	exit 1
fi
stale=$(stale_internal "$(internal_version_lines)")
if [ -n "$stale" ]; then
	printf '错误: 内层互依版本未同步到 %s：\n%s\n' "$new_version" "$stale" >&2
	exit 1
fi

# cargo 解析能发现 manifest 语法错误
cargo metadata --format-version 1 --no-deps >/dev/null
printf '完成。根版本与内层互依共 %s 处已同步为 %s。\n' \
	"$((current_count + 1))" "$new_version"
