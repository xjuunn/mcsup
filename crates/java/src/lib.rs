//! 本机已安装 Java 的平台扫描
//!
//! 各平台收集候选 `java` home 目录，再对每个候选项执行 `java -version`
//! 解析 major / vendor / 完整版本串，通过 [`port::JavaScanner`] 上报。
//!
//! 候选来源：`JAVA_HOME` 与 PATH；Windows 的注册表、`where` 与各发行商安装
//! 目录；macOS 的 `java_home` 与标准 JVM 目录；Linux 的 `update-alternatives`
//! 与发行版 JVM 目录；外加用户级 JDK 目录（`IntelliJ` `.jdks`、Gradle、sdkman、
//! winget、scoop 等）。目录收集一律走有深度上限的广度优先搜索，不遍历整盘。

use domain::java::{JavaMajor, JavaVendor, java_exe_name};
use port::{JavaScanner, ScannedJava};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::collections::VecDeque;
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
        // 实现方负责去重（见 port::JavaScanner 契约）：unix 上候选 home 先归一到
        // 真实路径——JAVA_HOME、sdkman `current` 等常是软链，不归一同一 JVM
        // 会被当成两个 home 重复注册
        let mut seen = std::collections::HashSet::new();
        for home in collect_homes() {
            let home = canonical_home(home);
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

/// unix 上把候选 home 归一到真实路径；路径不存在时保留原样
#[cfg(unix)]
fn canonical_home(home: PathBuf) -> PathBuf {
    std::fs::canonicalize(&home).unwrap_or(home)
}

/// Windows 不跟随链接，保留原路径形态（`canonicalize` 会加 `\\?\` 前缀，
/// 与注册表登记的路径不同形，去重与注册会重复）
#[cfg(not(unix))]
fn canonical_home(home: PathBuf) -> PathBuf {
    home
}

/// 收集候选 JVM home 目录
fn collect_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    collect_env_home(&mut homes);
    collect_path_homes(&mut homes);
    collect_platform_homes(&mut homes);
    homes
}

/// 运行命令并捕获输出
///
/// spawn 失败返回 `None` 并记 warn；`require_success` 为 `true` 且退出码非零时
/// 同样返回 `None`——失败命令的 stdout 往往是半截或误导性文本，
/// 拿它解析会把坏路径登记成 JVM home。
fn run_capture(
    cmd: &mut Command,
    label: &str,
    require_success: bool,
) -> Option<std::process::Output> {
    match cmd.output() {
        Ok(output) => {
            if require_success && !output.status.success() {
                tracing::warn!("{label} exited with {}; discarding output", output.status);
                return None;
            }
            Some(output)
        }
        Err(err) => {
            tracing::warn!(?err, "failed to run {label}");
            None
        }
    }
}

fn collect_env_home(homes: &mut Vec<PathBuf>) {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let path = PathBuf::from(home);
        if path.is_dir() {
            homes.push(path);
        }
    }
}

/// 从 PATH 里的 `java` 反推 home
///
/// Unix 上跟随符号链接：`/usr/bin/java` 常是 alternatives 链接，直接取上级
/// 会得到 `/usr` 而不是真实 JVM home。
fn collect_path_homes(homes: &mut Vec<PathBuf>) {
    let Some(path_var) = std::env::var_os("PATH") else {
        return;
    };
    for dir in std::env::split_paths(&path_var) {
        let binary = dir.join(java_exe_name());
        if !binary.is_file() {
            continue;
        }
        let real = resolve_binary(binary);
        if let Some(home) = real.parent().and_then(Path::parent) {
            homes.push(home.to_path_buf());
        }
    }
}

/// 解析到真实可执行文件路径
#[cfg(unix)]
fn resolve_binary(binary: PathBuf) -> PathBuf {
    std::fs::canonicalize(&binary).unwrap_or(binary)
}

/// Windows 不跟随链接，保留原路径形态（`canonicalize` 会加 `\\?\` 前缀，
/// 与 `JAVA_HOME` 等来源的路径不同形，去重与注册表会重复登记）
#[cfg(not(unix))]
fn resolve_binary(binary: PathBuf) -> PathBuf {
    binary
}

#[cfg(target_os = "macos")]
fn collect_platform_homes(homes: &mut Vec<PathBuf>) {
    collect_java_home_tool(homes);
    let mut roots = vec![
        // .jdk 包形如 {vendor}-{version}.jdk/Contents/Home
        (PathBuf::from("/Library/Java/JavaVirtualMachines"), 2),
        // Homebrew keg 链接：opt/openjdk@21/bin/java
        (PathBuf::from("/opt/homebrew/opt"), 1),
        (PathBuf::from("/usr/local/opt"), 1),
    ];
    push_user_roots(&mut roots);
    collect_from_roots(&roots, homes);
}

/// `/usr/libexec/java_home -V` 列出系统登记的全部 JDK
///
/// 表头与末行是无用文本，只取行尾的绝对路径；stdout / stderr 都读，
/// 不同 macOS 版本输出位置不一致。
#[cfg(target_os = "macos")]
fn collect_java_home_tool(homes: &mut Vec<PathBuf>) {
    let Some(output) = run_capture(
        Command::new("/usr/libexec/java_home").arg("-V"),
        "java_home -V",
        false,
    ) else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let texts: [&str; 2] = [stdout.as_ref(), stderr.as_ref()];
    for text in texts {
        for line in text.lines() {
            if let Some(path) = trailing_absolute_path(line) {
                homes.push(path);
            }
        }
    }
}

/// 取行尾绝对路径：`    21.0.5 (arm64) "Eclipse Adoptium" - "OpenJDK 21" /Library/…/Home`
/// 或整行就是路径
#[cfg(target_os = "macos")]
fn trailing_absolute_path(line: &str) -> Option<PathBuf> {
    let path = if let Some(index) = line.rfind(" /") {
        line[index + 2..].trim()
    } else if line.starts_with('/') {
        line.trim()
    } else {
        return None;
    };
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[cfg(target_os = "linux")]
fn collect_platform_homes(homes: &mut Vec<PathBuf>) {
    collect_update_alternatives(homes);
    let mut roots = vec![
        (PathBuf::from("/usr/lib/jvm"), 2),
        (PathBuf::from("/usr/java"), 2),
        // 手动解包的 tarball 常见落点
        (PathBuf::from("/opt"), 2),
        (PathBuf::from("/usr/local"), 2),
    ];
    push_user_roots(&mut roots);
    collect_from_roots(&roots, homes);
}

/// Debian 系 `update-alternatives` 登记的 java 链接
#[cfg(target_os = "linux")]
fn collect_update_alternatives(homes: &mut Vec<PathBuf>) {
    let Some(output) = run_capture(
        Command::new("update-alternatives").args(["--list", "java"]),
        "update-alternatives --list java",
        false,
    ) else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let path = Path::new(line.trim());
        // 链接指向 bin/java，home 是其上级
        if let Some(home) = path.parent().and_then(Path::parent) {
            homes.push(home.to_path_buf());
        }
    }
}

/// 用户级 JDK 目录（Unix）：IDE 与构建工具各自下载的运行时
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn push_user_roots(roots: &mut Vec<(PathBuf, usize)>) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    roots.push((home.join(".jdks"), 2)); // IntelliJ 下载的 JDK
    roots.push((home.join(".gradle").join("jdks"), 2)); // Gradle 工具链
    roots.push((home.join(".sdkman").join("candidates").join("java"), 2));
}

#[cfg(target_os = "windows")]
fn collect_platform_homes(homes: &mut Vec<PathBuf>) {
    collect_registry_homes(homes);
    collect_where_homes(homes);
    let roots = windows_roots();
    collect_from_roots(&roots, homes);
}

/// 注册表登记的 Java 安装路径（含 32 位视图与旧键名）
#[cfg(target_os = "windows")]
fn collect_registry_homes(homes: &mut Vec<PathBuf>) {
    for &key in JAVA_SOFT_KEYS {
        let Some(output) = run_capture(
            Command::new("reg").args(["query", key, "/s", "/v", "JavaHome"]),
            "reg query JavaHome",
            false,
        ) else {
            continue;
        };
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
}

/// `where java` 解析 PATH + PATHEXT，能补出裸目录扫描漏掉的入口
#[cfg(target_os = "windows")]
fn collect_where_homes(homes: &mut Vec<PathBuf>) {
    let Some(output) = run_capture(Command::new("where").arg("java"), "where java", false) else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let path = Path::new(line.trim());
        if let Some(home) = path.parent().and_then(Path::parent) {
            homes.push(home.to_path_buf());
        }
    }
}

/// `JavaSoft` 注册表键：JDK / JRE，64 位与 `Wow6432Node` 两个视图
#[cfg(target_os = "windows")]
const JAVA_SOFT_KEYS: &[&str] = &[
    r"HKLM\SOFTWARE\JavaSoft\JDK",
    r"HKLM\SOFTWARE\JavaSoft\JRE",
    r"HKLM\SOFTWARE\JavaSoft\Java Development Kit",
    r"HKLM\SOFTWARE\JavaSoft\Java Runtime Environment",
    r"HKLM\SOFTWARE\WOW6432Node\JavaSoft\JDK",
    r"HKLM\SOFTWARE\WOW6432Node\JavaSoft\JRE",
    r"HKLM\SOFTWARE\WOW6432Node\JavaSoft\Java Development Kit",
    r"HKLM\SOFTWARE\WOW6432Node\JavaSoft\Java Runtime Environment",
];

/// Windows 常见安装根目录及其搜索深度
///
/// 深度 2 覆盖 `{发行商}\{版本}\bin\java.exe` 与 IDE 内置运行时
/// `{IDE}\{版本}\jbr\bin\java.exe`；`Program Files\Microsoft` 只搜一层，
/// 避免钻进 Office / Visual Studio 目录树。
#[cfg(target_os = "windows")]
fn windows_roots() -> Vec<(PathBuf, usize)> {
    const VENDORS: &[&str] = &[
        "Java",
        "Eclipse Adoptium",
        "AdoptOpenJDK",
        "Amazon Corretto",
        "Azul",
        "Zulu",
        "BellSoft",
        "Semeru",
        "OpenJDK",
        "RedHat",
    ];
    let mut roots = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let program_files = PathBuf::from(program_files);
        for vendor in VENDORS {
            roots.push((program_files.join(vendor), 2));
        }
        roots.push((program_files.join("Microsoft"), 1));
        roots.push((program_files.join("JetBrains"), 2));
        roots.push((program_files.join("Android"), 2));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        let program_files_x86 = PathBuf::from(program_files_x86);
        for vendor in VENDORS {
            roots.push((program_files_x86.join(vendor), 2));
        }
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let local_app_data = PathBuf::from(local_app_data);
        // winget 安装：Packages\{包名}\{目录}\bin\java.exe
        roots.push((local_app_data.join(r"Microsoft\WinGet\Packages"), 2));
        // 用户级安装（含 Adoptium per-user 安装）
        roots.push((local_app_data.join("Programs"), 2));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let user_profile = PathBuf::from(user_profile);
        roots.push((user_profile.join(".jdks"), 2)); // IntelliJ 下载的 JDK
        roots.push((user_profile.join(".gradle").join("jdks"), 2));
        roots.push((user_profile.join("scoop").join("apps"), 2));
    }
    roots
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn collect_platform_homes(_homes: &mut Vec<PathBuf>) {}

/// 逐个根目录做有深度上限的搜索
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn collect_from_roots(roots: &[(PathBuf, usize)], homes: &mut Vec<PathBuf>) {
    for (root, depth) in roots {
        collect_homes_under(root, *depth, homes);
    }
}

/// 在 `root` 下广度优先寻找 JVM home，最多向下 `max_depth` 层
///
/// 找到 home 即停止向下：JDK 内部的 `jre` 等嵌套运行时不算独立 JVM，
/// 也让 `.jdks` 一类目录不因层层下钻而变慢。
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn collect_homes_under(root: &Path, max_depth: usize, homes: &mut Vec<PathBuf>) {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        // 目录本身 / macOS bundle 的候选与顺序统一在 domain，扫描与安装不走两份
        if let Some(home) = domain::java::find_jvm_home(&dir, Path::is_file) {
            homes.push(home);
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push_back((path, depth + 1));
            }
        }
    }
}

/// 对单个 home 执行 `java -version` 并解析
///
/// 大版本不在服务端支持区间（见 [`JavaMajor::parse`]）时返回 `None`，
/// 该 home 不登记。
fn probe_home(home: &Path) -> Option<ScannedJava> {
    let java_bin = domain::java::java_bin_under(home);
    if !java_bin.is_file() {
        return None;
    }
    let output = run_capture(
        Command::new(&java_bin).arg("-version"),
        "java -version",
        true,
    )?;
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
        return JavaMajor::parse_str(minor).ok();
    }
    JavaMajor::parse_str(head).ok()
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

    /// 目录搜索的测试：独立模块只在支持搜索的平台编译
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    mod walk {
        use super::*;

        /// 一次性工作目录；同进程内按用例名隔离
        fn scratch(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("mcsup-java-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            dir
        }

        /// 造一个含 `bin/java[.exe]` 的 JVM home
        fn fake_home(root: &Path) -> PathBuf {
            let home = root.join("vendor").join("jdk-21");
            std::fs::create_dir_all(home.join("bin")).expect("bin dir");
            std::fs::write(home.join("bin").join(java_exe_name()), b"stub").expect("java stub");
            home
        }

        #[test]
        fn finds_nested_home() {
            let root = scratch("nested");
            let home = fake_home(&root);
            let mut homes = Vec::new();
            collect_homes_under(&root, 2, &mut homes);
            assert_eq!(homes, vec![home]);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn finds_macos_bundle_home() {
            let root = scratch("bundle");
            let home = root.join("temurin-21.jdk").join("Contents").join("Home");
            std::fs::create_dir_all(home.join("bin")).expect("bin dir");
            std::fs::write(home.join("bin").join(java_exe_name()), b"stub").expect("java stub");
            let mut homes = Vec::new();
            collect_homes_under(&root, 2, &mut homes);
            assert_eq!(homes, vec![home]);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn stops_at_home_without_descending() {
            let root = scratch("stop");
            let home = fake_home(&root);
            // home 内嵌的 jre 不再登记为独立 JVM
            let jre = home.join("jre");
            std::fs::create_dir_all(jre.join("bin")).expect("jre bin");
            std::fs::write(jre.join("bin").join(java_exe_name()), b"stub").expect("java stub");
            let mut homes = Vec::new();
            collect_homes_under(&root, 4, &mut homes);
            assert_eq!(homes, vec![home]);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn missing_root_yields_nothing() {
            let root = std::env::temp_dir().join("mcsup-java-missing-root");
            let mut homes = Vec::new();
            collect_homes_under(&root, 2, &mut homes);
            assert!(homes.is_empty());
        }
    }
}
