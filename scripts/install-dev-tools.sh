#!/usr/bin/env sh
# 把 dev-tools MCP 服务放到仓库外固定位置，供所有 worktree 共用。
#
# 位置与 `scripts/dev-tools.sh` / `scripts/dev-tools.cmd` 保持一致：
# `$(git rev-parse --git-common-dir)/mcsup-build/dev-tools/`。
#
# 为什么单独放：dev-tools 是 MCP 客户端常驻拉起的进程，放在某个 worktree 的
# target 里，那个 worktree 一删或一重建，客户端手里的路径就失效。放在共同目录
# 后与 worktree 生命周期无关，worktree 内的构建缓存也能放心清理。
#
# 用法（构建后执行一次，重建后再次执行）：
#
#   cargo build -p dev --bin mcsup-dev-tools
#   sh scripts/install-dev-tools.sh

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

common=$(git rev-parse --git-common-dir) || {
	printf '无法定位 git 公共目录（不在 git 仓库内？）\n' >&2
	exit 1
}
common=$(CDPATH='' cd -- "$common" && pwd)

. ./scripts/dev-tools-env.sh
mcsup_use_local_target

bin="mcsup-dev-tools"
case "$(uname -s 2>/dev/null || echo unknown)" in
MINGW* | MSYS* | CYGWIN* | Windows_NT*) bin="$bin.exe" ;;
esac

source_exe="${CARGO_TARGET_DIR:-target}/debug/$bin"
if [ ! -x "$source_exe" ]; then
	printf '缺少 %s，先执行：cargo build -p dev --bin %s\n' "$source_exe" "mcsup-dev-tools" >&2
	exit 1
fi

dest_dir="$common/mcsup-build/dev-tools"
mkdir -p "$dest_dir"
# 先写临时文件再 rename：客户端可能正握着旧 exe，直接覆盖在 Windows 上会失败。
cp "$source_exe" "$dest_dir/.$bin.new"
mv -f "$dest_dir/.$bin.new" "$dest_dir/$bin"
chmod +x "$dest_dir/$bin" 2>/dev/null || true

printf '已安装：%s\n' "$dest_dir/$bin"
