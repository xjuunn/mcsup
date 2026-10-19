//! 本机已安装 Java 的平台扫描
//!
//! 各平台收集候选 `java` home 目录，再对每个候选项执行 `java -version`
//! 解析 major / vendor / 完整版本串，通过 [`port::JavaScanner`] 上报。

use domain::java::{JavaMajor, JavaVendor};
use port::{JavaScanner, ScannedJava};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 本机 Java 扫描器
#[derive(Debug, Default, Clone, Copy)]
pub struct HostJavaScanner;

impl HostJavaScanner {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl JavaScanner for HostJavaScanner {
    fn scan(
        &self,
        on_found: &mut dyn FnMut(ScannedJava),
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut seen = std::collections::HashSet::new();
        for home in collect_homes() {
            if !seen.insert(home.clone()) {
                continue;
            }
            if let Some(found) = probe_home(&home) {
                on_found(found);
            }
        }
        Ok(())
    }
}

/// 收集候选 JVM home 目录
fn collect_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    collect_env_home(&mut homes);
    collect_platform_homes(&mut homes);
    homes
}

fn collect_env_home(homes: &mut Vec<PathBuf>) {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let path = PathBuf::from(home);
        if path.is_dir() {
            homes.push(path);
        }
    }
}

#[cfg(target_os = "macos")]
fn collect_platform_homes(homes: &mut Vec<PathBuf>) {
    // /usr/libexec/java_home -V 输出每行一个 JDK，路径形如
    // `    21.0.5 (arm64) "Eclipse Adoptium" - "OpenJDK 21.0.5" /Library/Java/JavaVirtualMachines/temurin-21.jdk/Contents/Home`
    if let Ok(output) = Command::new("/usr/libexec/java_home").arg("-V").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(path) = line.rsplit(" /").next()
                && !path.is_empty()
            {
                homes.push(PathBuf::from(path.trim()));
            }
        }
    }
    // 扫描标准 JVM 安装目录
    let jvm_root = Path::new("/Library/Java/JavaVirtualMachines");
    if let Ok(entries) = std::fs::read_dir(jvm_root) {
        for entry in entries.flatten() {
            let home = entry.path().join("Contents").join("Home");
            if home.is_dir() {
                homes.push(home);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_platform_homes(homes: &mut Vec<PathBuf>) {
    if let Ok(output) = Command::new("update-alternatives")
        .args(["--list", "java"])
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let path = Path::new(line.trim());
            // update-alternatives 指向 bin/java，home 是其上级
            if let Some(home) = path.parent().and_then(Path::parent) {
                homes.push(home.to_path_buf());
            }
        }
    }
    let jvm_root = Path::new("/usr/lib/jvm");
    if let Ok(entries) = std::fs::read_dir(jvm_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("bin").join("java").is_file() {
                homes.push(path);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn collect_platform_homes(homes: &mut Vec<PathBuf>) {
    // 注册表路径形如 HKLM\SOFTWARE\JavaSoft\JDK\17
    if let Ok(output) = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\JavaSoft\JDK",
            "/s",
            "/v",
            "JavaHome",
        ])
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some((_, value)) = line.split_once("REG_SZ") {
                let path = PathBuf::from(value.trim());
                if path.is_dir() {
                    homes.push(path);
                }
            }
        }
    }
    if let Ok(output) = Command::new("where").arg("java").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let path = Path::new(line.trim());
            if let Some(home) = path.parent().and_then(Path::parent) {
                homes.push(home.to_path_buf());
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn collect_platform_homes(_homes: &mut Vec<PathBuf>) {}

/// 对单个 home 执行 `java -version` 并解析
fn probe_home(home: &Path) -> Option<ScannedJava> {
    let java_bin = home.join("bin").join(java_exe_name());
    if !java_bin.is_file() {
        return None;
    }
    let output = Command::new(&java_bin).arg("-version").output().ok()?;
    // `java -version` 写 stderr
    let text = String::from_utf8_lossy(&output.stderr);
    let first = text.lines().next()?;
    let version = parse_version_line(first)?;
    let major = extract_major(&version)?;
    let vendor = JavaVendor::parse_fuzzy(first);
    Some(ScannedJava {
        path: home.to_path_buf(),
        major,
        version,
        vendor,
    })
}

#[cfg(target_os = "windows")]
const fn java_exe_name() -> &'static str {
    "java.exe"
}

#[cfg(not(target_os = "windows"))]
const fn java_exe_name() -> &'static str {
    "java"
}

/// 从 `openjdk version "17.0.9" 2023-10-17` 提取 `17.0.9`
fn parse_version_line(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_owned())
}

/// 从完整版本串提取大版本；兼容 `1.8.0_392` 与 `17.0.9`
fn extract_major(version: &str) -> Option<JavaMajor> {
    let head = version.split(['.', '-', '+', '_']).next()?;
    if head == "1" {
        // 1.8.x → 8
        let minor = version.split('.').nth(1)?;
        let minor: i64 = minor.parse().ok()?;
        return JavaMajor::parse(minor).ok();
    }
    let major: i64 = head.parse().ok()?;
    JavaMajor::parse(major).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_version_line() {
        let line = r#"openjdk version "17.0.9" 2023-10-17"#;
        assert_eq!(parse_version_line(line).as_deref(), Some("17.0.9"));
    }

    #[test]
    fn parses_legacy_8_version_line() {
        let line = r#"java version "1.8.0_392""#;
        assert_eq!(parse_version_line(line).as_deref(), Some("1.8.0_392"));
    }

    #[test]
    fn extracts_major_from_modern() {
        assert_eq!(extract_major("17.0.9").map(JavaMajor::as_i64), Some(17));
        assert_eq!(extract_major("21.0.5+11").map(JavaMajor::as_i64), Some(21));
        assert_eq!(
            extract_major("21-internal").map(JavaMajor::as_i64),
            Some(21)
        );
    }

    #[test]
    fn extracts_major_from_legacy_8() {
        assert_eq!(extract_major("1.8.0_392").map(JavaMajor::as_i64), Some(8));
    }
}
