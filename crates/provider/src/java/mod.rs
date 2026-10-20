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
