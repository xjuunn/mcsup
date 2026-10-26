#!/usr/bin/env sh
# 架构规则校验。只读。
#
# cargo 不校验 crate 命名与依赖方向，AGENTS.md 的命名与依赖规则只能在这里断言。
# 检查项：成员覆盖、禁用名、mcsup- 前缀（库禁止 / bin 必需）、apps 依赖、
# [lints]、模块依赖方向、feature 与模块对应、feature 语义（拉入内层集合
# 等于允许集合）。
#
# 用法：sh scripts/check-architecture.sh

set -u

violations=0

fail() {
	printf '架构检查失败: %s\n' "$1" >&2
	violations=$((violations + 1))
}

# package name 必须是字面量（不允许 workspace 继承），可安全按行读取。
pkg_name() {
	sed -n 's/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -n 1
}

contains() {
	needle=$1
	list=$2
	for item in $list; do
		if [ "$item" = "$needle" ]; then
			return 0
		fi
	done
	return 1
}

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$root" || exit 1

manifests=''
for dir in crates/* apps/*; do
	[ -d "$dir" ] || continue
	[ -f "$dir/Cargo.toml" ] || continue
	manifests="$manifests $dir/Cargo.toml"
done

if [ -z "$manifests" ]; then
	printf '架构检查失败: 未找到任何 workspace 包\n' >&2
	exit 1
fi

# workspace 加载失败就停，否则后面每个包都会误报「无法解析」。
if ! cargo metadata --format-version 1 --no-deps >/dev/null; then
	printf '架构检查失败: workspace 无法加载，请修正上面的 cargo 错误\n' >&2
	exit 1
fi

# 2 / 3 / 4: 命名与成员覆盖

app_names=''
inner_pkgs=''
for manifest in $manifests; do
	name=$(pkg_name "$manifest")
	if [ -z "$name" ]; then
		continue
	fi
	# 除门面外的内层包（供 crate 级依赖方向判定）
	if [ "$name" != mcsup ] && [ "${manifest#crates/}" != "$manifest" ]; then
		inner_pkgs="$inner_pkgs $name"
	fi
done

for manifest in $manifests; do
	name=$(pkg_name "$manifest")
	if [ -z "$name" ]; then
		fail "$manifest 无法解析出 package name"
		continue
	fi

	# 包不在 members 里就既不编译也不测试，CI 仍然全绿。
	if ! cargo tree -p "$name" --depth 0 >/dev/null 2>&1; then
		fail "$manifest: 包 $name 无法被 cargo 解析，请检查 Cargo.toml 的 members 与本包的 manifest"
	fi

	case "$name" in
	core | std | mod | crate | self | super)
		fail "$name: 禁用 crate 名称，会遮蔽内置 crate 或破坏语法"
		;;
	esac

	case "$manifest" in
	crates/*)
		if [ "${name#mcsup-}" != "$name" ]; then
			fail "$name: crates/ 下的库禁止使用 mcsup- 前缀"
		fi
		;;
	apps/*)
		app_names="$app_names $name"
		if [ "${name#mcsup-}" != "$name" ]; then
			fail "$name: apps/ 下的包禁止使用 mcsup- 前缀，前缀只加在 bin 名上"
		fi
		# 只取 [[bin]] section 内的 name。
		bin_names=$(awk '
			/^\[\[/ { in_bin = ($0 ~ /^\[\[bin\]\]/) ; next }
			in_bin && /^[[:space:]]*name[[:space:]]*=/ {
				gsub(/^[^"]*"|"[^"]*$/, "")
				print
			}
		' "$manifest")
		if [ -z "$bin_names" ]; then
			# 无显式 [[bin]] 时 Cargo 用 package 名
			bin_names="$name"
		fi
		for bin in $bin_names; do
			case "$bin" in
			mcsup | mcsup-*) ;;
			*) fail "$name: bin $bin 未带 mcsup- 前缀，用户在 PATH 上无法辨识来源" ;;
			esac
		done
		;;
	esac
done

# 5: 包级依赖方向。只看 normal 与 build 边，dev-dependency 允许任意方向。

for manifest in $manifests; do
	pkg=$(pkg_name "$manifest")
	[ -n "$pkg" ] || continue

	deps=$(cargo tree -p "$pkg" --depth 1 --prefix none -e normal,build) || {
		fail "cargo tree -p $pkg 执行失败"
		continue
	}
	deps=$(printf '%s\n' "$deps" | tail -n +2 | cut -d' ' -f1)

	for dep in $deps; do
		if contains "$dep" "$app_names"; then
			fail "$pkg -> $dep: 禁止依赖 apps/ 下的包，apps 是并行入口"
		fi
	done
done

# 5b: 包级实现层依赖方向（crate 级，与 7 的模块级互补）。
#
# 允许的边（只列本仓库内部包）：
#   domain  -> （无）
#   port    -> domain
#   auth    -> （无）                 叶子契约 crate：权限 / 角色 / token 的词汇表
#   protocol-> domain port auth      契约 crate：帧载荷、结果类型、权限映射
#   service -> domain port
#   <实现层> -> domain port + 契约 crate（runtime 额外允许 protocol）
#   transport -> 契约 crate           （transport 只搬字节，不含业务判定）
#   backend -> domain port + 实现层     （执行位置聚合器，唯一允许组合具体实现）
#   mcsup   -> 任意内层（门面）
#   apps    -> 只允许 mcsup
# 实现层之间除 backend 外互不依赖。
impl_layers='runtime java storage file backup provider modkit transport'

for manifest in $manifests; do
	pkg=$(pkg_name "$manifest")
	[ -n "$pkg" ] || continue
	# mcsup 门面与 apps 不在本表约束内
	case "$pkg" in
	mcsup) continue ;;
	esac
	if contains "$pkg" "$app_names"; then
		continue
	fi

	deps=$(cargo tree -p "$pkg" --depth 1 --prefix none -e normal,build 2>/dev/null | tail -n +2 | cut -d' ' -f1)

	for dep in $deps; do
		# 只检查本仓库内部包
		if ! contains "$dep" "$inner_pkgs"; then
			continue
		fi
		case "$pkg" in
		domain)
			fail "$pkg -> $dep: domain 不得依赖任何本仓库模块"
			;;
		port)
			if [ "$dep" != domain ]; then
				fail "$pkg -> $dep: 只允许依赖 domain"
			fi
			;;
		auth)
			fail "$pkg -> $dep: auth 是叶子契约 crate，不得依赖本仓库模块"
			;;
		protocol)
			case "$dep" in
			domain | port | auth) ;;
			*) fail "protocol -> $dep: 契约层只允许依赖 domain / port / auth" ;;
			esac
			;;
		service)
			case "$dep" in
			domain | port) ;;
			*) fail "service -> $dep: service 只允许依赖 domain / port，实现层须经 port 注入" ;;
			esac
			;;
		transport)
			case "$dep" in
			domain | port | protocol | auth) ;;
			*) fail "transport -> $dep: 传输层只允许依赖契约 crate（domain / port / protocol / auth）" ;;
			esac
			;;
		backend)
			case "$dep" in
			domain | port) ;;
			*)
				if ! contains "$dep" "$impl_layers"; then
					fail "backend -> $dep: 只允许依赖 domain / port 或实现层"
				fi
				;;
			esac
			;;
		*)
			# 其余实现层
			if contains "$dep" "$impl_layers"; then
				fail "$pkg -> $dep: 实现层之间禁止互相依赖（只有 backend 可组合实现层）"
			fi
			;;
		esac
	done
done

# 6: 每个包必须启用 workspace lint

for manifest in $manifests; do
	if ! grep -q '^[[:space:]]*workspace[[:space:]]*=[[:space:]]*true' "$manifest"; then
		fail "$manifest: 未启用 [lints] workspace = true，会逃过 workspace lint 门禁"
	fi
done

# 7: 模块级依赖方向。Cargo 看不见 intra-crate 依赖，只能扫 crate:: 路径。
#
# 实现层之间不互相依赖。backup 要读文件、要发 save-all，但只能经 port 的
# trait 访问；file / runtime 是 feature 图里的默认实现，不是它的编译期依赖。
#
# transport 与 api 可以用 protocol：protocol 是契约类型，不是实现。

# 常开模块：类型词汇表与基础设施，任何层都可依赖
core_modules='domain|port|error|path|version'

check_module_layer() {
	layer=$1
	allowed=$2
	dir="crates/mcsup/src/$layer"

	# 模块未建则无事可查
	[ -d "$dir" ] || return 0

	targets=$(
		{
			# 扫全文而非只扫 use，否则全路径调用、类型别名、限定路径都能绕过
			grep -rh --include='*.rs' -vE '^[[:space:]]*(//|/\*)' "$dir" |
				grep -oE 'crate::[A-Za-z_][A-Za-z0-9_]*' |
				sed -E 's/^crate:://'
			# 花括号形式匹配不到上一条
			grep -rh --include='*.rs' -vE '^[[:space:]]*(//|/\*)' "$dir" |
				grep -oE 'crate::\{[^}]*\}' |
				sed -E 's/^crate::\{//; s/\}$//' |
				tr ',' '\n' |
				sed -E 's/[[:space:]]//g' |
				grep -E '^[A-Za-z_][A-Za-z0-9_]*$'
		} | sort -u
	)

	for target in $targets; do
		if ! printf '%s' "$target" | grep -qE "^($allowed)$"; then
			fail "mcsup::$layer -> crate::$target: $layer 层禁止依赖该模块"
		fi
	done
}

check_module_layer domain "$core_modules"
check_module_layer port "$core_modules"
check_module_layer service "service|$core_modules"
check_module_layer protocol "protocol|$core_modules"

# 实现层：只允许依赖 core 与自身
for layer in backend runtime java storage file backup provider modkit; do
	check_module_layer "$layer" "$layer|$core_modules"
done

# 实现层中可用契约类型的
check_module_layer transport "transport|protocol|$core_modules"
check_module_layer api "api|protocol|$core_modules"
check_module_layer auth "auth|$core_modules"

# cli 是库侧的「命令语义 + 渲染」，不是入口，不允许碰实现层
check_module_layer cli "cli|service|protocol|$core_modules"

# agent 侧：认证 + 帧分发 + supervisor 入口，只经组合根访问本机服务
check_module_layer remote "remote|bootstrap|$core_modules"

# 8: feature 与模块一一对应，第三方才能推断「开哪个 feature 拿到哪个模块」。
# 双向断言，且没有「待落地」豁免：没模块就不该声明 feature。

pkg_manifest='crates/mcsup/Cargo.toml'
pkg_lib='crates/mcsup/src/lib.rs'

# default 不是模块名，不参与对应检查
feature_bundles='default'

if [ -f "$pkg_manifest" ] && [ -f "$pkg_lib" ]; then
	declared_features=$(
		sed -n '/^\[features\]/,/^\[[a-z]/p' "$pkg_manifest" |
			grep -E '^[a-z_]+[[:space:]]*=' |
			cut -d'=' -f1 |
			tr -d '[:blank:]'
	)

	for feature in $declared_features; do
		if contains "$feature" "$feature_bundles"; then
			continue
		fi

		if ! grep -A3 -E "cfg\(feature[[:space:]]*=[[:space:]]*\"$feature\"\)" "$pkg_lib" |
			grep -qE "pub[[:space:]]+(mod|use)[[:space:]]+$feature;"; then
			fail "$feature: 声明了 feature 但没有 #[cfg(feature = \"$feature\")] pub mod/use $feature;"
		fi
	done

	# 常开模块：类型词汇表与基础设施
	ungated_modules='domain port error path version'

	declared_modules=$(
		grep -oE '^[[:space:]]*pub[[:space:]]+mod[[:space:]]+[A-Za-z_][A-Za-z0-9_]*' "$pkg_lib" |
			sed -E 's/.*mod[[:space:]]+//'
	)

	for module in $declared_modules; do
		if contains "$module" "$ungated_modules"; then
			continue
		fi
		if ! contains "$module" "$declared_features"; then
			fail "mcsup::$module: 非常开模块必须声明同名 feature，否则它会无条件编译"
		fi
	done

	# 9: feature 语义。feature 若硬依赖其他能力 crate（如 file 拉入 provider），
	# 「只开 file」名存实亡。断言：每个 feature 拉入的内层 crate 集合等于其
	# 级联闭包；空 feature 一个内层也不拉。允许集合 = facade_allow，新增
	# crate / feature 时同步更新。

	inner_names=''
	for manifest in crates/*/Cargo.toml; do
		name=$(pkg_name "$manifest")
		if [ -n "$name" ] && [ "$name" != 'mcsup' ]; then
			inner_names="$inner_names $name"
		fi
	done

	facade_allow() {
		case "$1" in
		domain) printf '%s' 'domain' ;;
		port) printf '%s' 'domain port' ;;
		protocol) printf '%s' 'auth domain port protocol' ;;
		service) printf '%s' 'domain port service' ;;
		auth) printf '%s' 'auth' ;;
		file) printf '%s' 'domain port file' ;;
		java) printf '%s' 'domain port java' ;;
		provider) printf '%s' 'domain port provider' ;;
		storage) printf '%s' 'domain port storage' ;;
		runtime) printf '%s' 'auth domain port protocol runtime' ;;
		backup) printf '%s' 'domain port backup' ;;
		transport) printf '%s' 'auth domain port protocol transport' ;;
		backend) printf '%s' 'auth backend backup domain port protocol runtime' ;;
		# 组合 feature：一次拉入全部内层（默认组合根）
		bootstrap) printf '%s' 'auth backend backup domain file java port protocol provider runtime service storage transport' ;;
		# 组合 feature：agent 侧要装配整套服务 + 传输 + supervisor 入口
		remote) printf '%s' 'auth backend backup domain file java port protocol provider runtime service storage transport' ;;
		*) printf '%s' '' ;;
		esac
	}

	facade_inner_of() {
		if [ -n "$1" ]; then
			cargo tree -p mcsup --no-default-features --features "$1" \
				--edges normal,build --prefix none 2>/dev/null
		else
			cargo tree -p mcsup --no-default-features \
				--edges normal,build --prefix none 2>/dev/null
		fi | awk '{print $1}' | sort -u
	}

	actual=$(facade_inner_of '')
	for name in $inner_names; do
		if printf '%s\n' "$actual" | grep -qx "$name"; then
			fail "门面空 feature: 不应拉入内层 $name"
		fi
	done

	for feature in $declared_features; do
		if contains "$feature" "$feature_bundles"; then
			continue
		fi

		actual=$(facade_inner_of "$feature")
		picked=''
		for name in $inner_names; do
			if printf '%s\n' "$actual" | grep -qx "$name"; then
				picked="$picked $name"
			fi
		done
		picked=$(printf '%s\n' $picked | sort | tr '\n' ' ' | sed 's/[[:space:]]*$//')
		allowed=$(facade_allow "$feature" | tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' ' | sed 's/[[:space:]]*$//')
		if [ "$picked" != "$allowed" ]; then
			fail "feature $feature: 拉入内层 [$picked]，允许 [$allowed]；feature 级联或允许集合表不同步"
		fi
	done
fi

if [ "$violations" -gt 0 ]; then
	printf '\n架构检查失败，共 %d 项。\n' "$violations" >&2
	exit 1
fi

count=$(printf '%s\n' $manifests | wc -l | tr -d ' ')
printf '架构检查通过（%s 个包）。\n' "$count"