#!/usr/bin/env sh
# 统一版本 bump：一处推进，全 workspace 锁步。
#
# 版本声明的唯一入口在根 Cargo.toml（方案 B）：
# 1. [workspace.package].version —— 所有包自身的版本（各 manifest 以
#    version.workspace = true 继承）
# 2. [workspace.dependencies] 里 9 个内层互依的 version —— 依赖的版本
#    要求（各 manifest 以 xxx.workspace = true 继承，禁止再写 version
#    字面量）
#
# 内层 manifest 不携带任何版本字面量，因此本脚本只改根一个文件；
# cargo publish 时继承会展开为完整声明，对 crates.io 等价。
#
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
total=$(grep -Ec "version = \"$old_version\"" "$manifest" || true)

# 根版本已是目标值且互依已同步（无残留旧值）时无事可做
if [ "$old_version" = "$new_version" ]; then
	stale_inner=$(grep -rEc "version = \"$old_version\"" crates/*/Cargo.toml 2>/dev/null | awk -F: '{s+=$NF} END {print s}')
	if [ "${stale_inner:-0}" -eq 0 ]; then
		printf '版本已是 %s（根 %s 处全部同步），无需 bump\n' "$new_version" "$total"
		exit 0
	fi
	printf '根已是 %s，但内层仍有 %s 处旧值，先修内层 manifest（应全部继承）\n' \
		"$new_version" "$stale_inner" >&2
	exit 1
fi

if [ "$check" -eq 1 ]; then
	printf 'bump %s -> %s：根 %s 共 %s 处（package 1 + 内层互依 %s）\n' \
		"$old_version" "$new_version" "$manifest" "$total" "$((total - 1))"
	exit 0
fi

# 替换后立即复核：一处不剩即失败，杜绝静默成功（上一版的教训）。
# 注意此时 old != new 已由上方幂等分支保证，残留判定不会误伤。
sed -i "s/version = \"$old_version\"/version = \"$new_version\"/g" "$manifest"
left=$(grep -Ec "version = \"$old_version\"" "$manifest" || true)
if [ "$left" -ne 0 ]; then
	printf '错误: 根 manifest 替换后仍剩 %s 处旧版本\n' "$left" >&2
	exit 1
fi

# 终验 1：内层 manifest 里 9 个内层名不允许携带版本字面量（继承形态下
# 出现即违规）；外部依赖（zip/diesel 等）的 version 声明是合法的，不在此列
inner='domain|port|protocol|service|file|java|provider|storage|runtime'
stray=$(grep -lE "^(($inner)) = \{ version = \"[0-9]" crates/*/Cargo.toml apps/*/Cargo.toml 2>/dev/null || true)
if [ -n "$stray" ]; then
	printf '错误: 以下 manifest 的内层互依含版本字面量，应改为 xxx.workspace = true 继承:\n%s\n' "$stray" >&2
	exit 1
fi
# 终验 2：cargo 解析能发现 manifest 语法错误
cargo metadata --format-version 1 --no-deps >/dev/null
printf '完成。根 manifest %s 处已同步为 %s，内层均为继承。\n' "$total" "$new_version"
