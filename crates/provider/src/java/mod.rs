mod adoptium;
mod azul;
mod foojay;
mod registry;

pub use registry::JavaSourceRegistry;

/// id 即下载地址的源共用解析：`package.id` 直接作为下载地址
///
/// Adoptium 与 Azul 的列表响应都带完整下载 URL，回查反而不可靠——
/// Azul 的 `package_uuid` 过滤会被静默忽略，取回的是任意平台的包。
pub(crate) fn resolved_from_id(
    package: &port::JavaPackage,
) -> Result<port::ResolvedJava, port::SourceError> {
    let location =
        url::Url::parse(&package.id).map_err(|err| port::SourceError::Decode(Box::new(err)))?;
    Ok(port::ResolvedJava {
        package: package.clone(),
        location,
        expected_size: None,
    })
}

/// 归档名是否可直接解压部署（dmg / msi / pkg 等安装包一律不收）
///
/// 下错安装包会把「安装包所在目录」注册成运行时，service 解压不出 JVM home，
/// 所以各源在列表阶段就按扩展名过滤。
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(crate) fn is_deployable_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".zip")
}

/// 架构标识归一到「家族」，用于跨源比对
///
/// 三家源对同一架构用不同写法（`aarch64` / `arm64`、`x64` / `amd64` / `x86_64`），
/// 归一后比较才不会把正确的包当成不匹配。
fn arch_family(raw: &str) -> Option<&'static str> {
    let value = raw.trim().to_ascii_lowercase();
    Some(match value.as_str() {
        "aarch64" | "arm64" | "armv8" | "armv8l" => "aarch64",
        "x64" | "amd64" | "x86_64" | "x86-64" | "x8664" => "x64",
        "x86" | "i386" | "i486" | "i586" | "i686" | "x86_32" => "x86",
        "arm" | "arm32" | "armhf" | "armv7" | "armv7l" => "arm",
        "riscv64" | "risc-v64" => "riscv64",
        "ppc64le" | "powerpc64le" => "ppc64le",
        "s390x" | "s390" => "s390x",
        _ => return None,
    })
}

/// 请求的架构是否与候选一致
///
/// 候选无法识别时返回 `false`：宁可漏掉一个包，也不能把错架构的 JVM 装到
/// 用户机器上——错架构的 `java` 连 `execve` 都过不去。
pub(crate) fn arch_matches(requested: &str, candidate: &str) -> bool {
    match (arch_family(requested), arch_family(candidate)) {
        (Some(want), Some(got)) => want == got,
        _ => false,
    }
}

/// 操作系统标识归一到「家族」
fn os_family(raw: &str) -> Option<&'static str> {
    let value = raw.trim().to_ascii_lowercase();
    Some(match value.as_str() {
        "linux" | "alpine_linux" | "alpine" => "linux",
        "macos" | "mac" | "osx" | "darwin" | "macosx" => "mac",
        "windows" | "win" | "windows_server" => "windows",
        "aix" => "aix",
        "solaris" | "sunos" => "solaris",
        _ => return None,
    })
}

/// 请求的操作系统是否与候选一致；候选无法识别时返回 `false`（理由同 [`arch_matches`]）
pub(crate) fn os_matches(requested: &str, candidate: &str) -> bool {
    match (os_family(requested), os_family(candidate)) {
        (Some(want), Some(got)) => want == got,
        _ => false,
    }
}

/// 从归档文件名里推断架构（部分源列表响应不带架构字段）
///
/// 按 token 长度降序匹配，避免 `x86_64` 先命中 `x86`。推断不出时返回 `None`，
/// 由调用方决定是否放行。
pub(crate) fn arch_from_filename(filename: &str) -> Option<&'static str> {
    let lower = filename.to_ascii_lowercase();
    let mut tokens: Vec<(&str, &'static str)> = [
        ("aarch64", "aarch64"),
        ("arm64", "aarch64"),
        ("x86_64", "x64"),
        ("x86-64", "x64"),
        ("amd64", "x64"),
        ("x64", "x64"),
        ("riscv64", "riscv64"),
        ("ppc64le", "ppc64le"),
        ("s390x", "s390x"),
        ("i686", "x86"),
        ("i586", "x86"),
        ("x86", "x86"),
        ("armhf", "arm"),
        ("arm32", "arm"),
        ("arm", "arm"),
    ]
    .into_iter()
    .collect();
    tokens.sort_by_key(|(token, _)| std::cmp::Reverse(token.len()));
    tokens
        .into_iter()
        .find(|(token, _)| lower.contains(token))
        .map(|(_, family)| family)
}

/// 文件名推断出的架构是否与请求一致；推断不出时放行
pub(crate) fn filename_arch_matches(requested: &str, filename: &str) -> bool {
    match (arch_family(requested), arch_from_filename(filename)) {
        (Some(want), Some(got)) => want == got,
        _ => true,
    }
}

/// 解析列表项声明的大版本；失败时 warn 并丢弃该条目
///
/// 三家源统一在这一处兜底：解析不出来的大版本不拿请求的大版本顶替——
/// 错标 `major` 会让 `install <大版本>` 选中不属于它的包。
pub(crate) fn parse_major_or_drop(
    raw: i64,
    source: &'static str,
    id: &str,
) -> Option<domain::java::JavaMajor> {
    match domain::java::JavaMajor::parse(raw) {
        Ok(major) => Some(major),
        Err(err) => {
            tracing::warn!(source, id, %err, "drop java package with unparsable major");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{arch_from_filename, arch_matches, filename_arch_matches, os_matches};

    /// 回归：foojay 的架构参数名是 `architecture`，写成 `arch` 会被上游静默忽略，
    /// 于是 aarch64 机器装到 x64 的 JVM（`execve` 直接失败）。
    /// 参数名修好后仍要本地复核，这里锁定复核规则本身。
    #[test]
    fn arch_matches_normalizes_aliases() {
        assert!(arch_matches("aarch64", "arm64"));
        assert!(arch_matches("aarch64", "aarch64"));
        assert!(arch_matches("x64", "amd64"));
        assert!(arch_matches("x64", "x86_64"));
        assert!(arch_matches("arm", "armhf"));
        assert!(!arch_matches("aarch64", "x64"));
        assert!(!arch_matches("x64", "aarch64"));
        assert!(!arch_matches("aarch64", "riscv64"));
    }

    /// 无法识别的架构一律不匹配：宁可漏装，不可错装
    #[test]
    fn arch_matches_rejects_unknown() {
        assert!(!arch_matches("aarch64", ""));
        assert!(!arch_matches("aarch64", "sparc"));
        assert!(!arch_matches("sparc", "aarch64"));
    }

    #[test]
    fn filename_arch_detects_longest_token() {
        assert_eq!(
            arch_from_filename("OpenJDK21U-jdk_aarch64_linux_hotspot_21.0.13_9.tar.gz"),
            Some("aarch64")
        );
        assert_eq!(
            arch_from_filename("zulu21.54.107-ca-jdk21.0.13-linux_x64.tar.gz"),
            Some("x64")
        );
        // `x86_64` 必须先于 `x86` 命中
        assert_eq!(
            arch_from_filename("jdk-21-linux-x86_64.tar.gz"),
            Some("x64")
        );
        assert_eq!(arch_from_filename("jdk-21-linux-i686.tar.gz"), Some("x86"));
        assert_eq!(
            arch_from_filename("graalvm-ce-linux-amd64.tar.gz"),
            Some("x64")
        );
    }

    /// 回归：aarch64 请求不得接受 x64 归档
    #[test]
    fn filename_arch_rejects_foreign_arch() {
        assert!(!filename_arch_matches(
            "aarch64",
            "zulu21.54.107-ca-jdk21.0.13-linux_x64.tar.gz"
        ));
        assert!(!filename_arch_matches(
            "x64",
            "OpenJDK21U-jdk_aarch64_linux_hotspot_21.0.13_9.tar.gz"
        ));
        assert!(filename_arch_matches(
            "aarch64",
            "zulu21.54.107-ca-jdk21.0.13-linux_musl_aarch64.tar.gz"
        ));
    }

    /// 文件名里推断不出架构时放行，避免因命名风格差异漏装
    #[test]
    fn filename_arch_allows_unknown() {
        assert!(filename_arch_matches("aarch64", "jdk-21.tar.gz"));
    }

    #[test]
    fn os_matches_normalizes_aliases() {
        assert!(os_matches("mac", "macos"));
        assert!(os_matches("linux", "linux"));
        assert!(!os_matches("linux", "windows"));
        assert!(!os_matches("linux", ""));
    }
}
