use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Java 大版本非法（须为 [`JavaMajor::MIN`]–[`JavaMajor::MAX`] 的整数）
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid java major version: {0}")]
pub struct InvalidJavaMajor(pub i64);

/// Java 大版本：Minecraft 服务端绑定的粒度（8、11、17、21…）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JavaMajor(i16);

impl JavaMajor {
    /// 可构造的最小大版本
    pub const MIN: i64 = 8;
    /// 可构造的最大大版本
    pub const MAX: i64 = 99;

    /// 校验并构造大版本
    ///
    /// # Errors
    ///
    /// 不在 [`JavaMajor::MIN`]–[`JavaMajor::MAX`] 闭区间时返回 [`InvalidJavaMajor`]
    pub fn parse(raw: i64) -> Result<Self, InvalidJavaMajor> {
        if (Self::MIN..=Self::MAX).contains(&raw) {
            #[allow(clippy::cast_possible_truncation)]
            Ok(Self(raw as i16))
        } else {
            Err(InvalidJavaMajor(raw))
        }
    }

    #[must_use]
    pub const fn as_i16(self) -> i16 {
        self.0
    }

    #[must_use]
    pub fn as_i64(self) -> i64 {
        i64::from(self.0)
    }
}

impl fmt::Display for JavaMajor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Java 发行版厂商
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JavaVendor {
    /// Eclipse Temurin（Adoptium）
    Temurin,
    /// Azul Zulu
    Zulu,
    /// `BellSoft` Liberica
    Liberica,
    /// Amazon Corretto
    Corretto,
    /// 系统自带 / 未知来源
    System,
}

impl JavaVendor {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Temurin => "temurin",
            Self::Zulu => "zulu",
            Self::Liberica => "liberica",
            Self::Corretto => "corretto",
            Self::System => "system",
        }
    }

    /// 解析厂商名；未知归一为 [`JavaVendor::System`]
    #[must_use]
    pub fn parse_fuzzy(raw: &str) -> Self {
        let lower = raw.to_ascii_lowercase();
        if lower.contains("temurin") || lower.contains("adoptium") {
            Self::Temurin
        } else if lower.contains("zulu") || lower.contains("azul") {
            Self::Zulu
        } else if lower.contains("liberica") || lower.contains("bell") {
            Self::Liberica
        } else if lower.contains("corretto") || lower.contains("amazon") {
            Self::Corretto
        } else {
            Self::System
        }
    }
}

impl fmt::Display for JavaVendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 包类型：JDK 含编译器，JRE 仅运行时
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum JavaPackageType {
    #[default]
    Jdk,
    Jre,
}

impl JavaPackageType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jdk => "jdk",
            Self::Jre => "jre",
        }
    }
}

/// JVM home 内 `java` 可执行文件的文件名（Windows 为 `java.exe`）
#[cfg(target_os = "windows")]
#[must_use]
pub const fn java_exe_name() -> &'static str {
    "java.exe"
}

/// JVM home 内 `java` 可执行文件的文件名（Unix 为 `java`）
#[cfg(not(target_os = "windows"))]
#[must_use]
pub const fn java_exe_name() -> &'static str {
    "java"
}

/// 判断目录是否为 JVM home：存在可执行 `java`
///
/// 扫描与安装后定位 home 共用此判定，避免各处对 `.exe` 后缀分叉。
#[must_use]
pub fn is_jvm_home(path: &Path) -> bool {
    path.join("bin").join(java_exe_name()).is_file()
}

/// 已注册的本机 Java 运行时
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaRuntime {
    /// 稳定身份（UUID 文本）
    pub id: String,
    /// `java` 可执行文件所在目录（或 JVM home）
    pub path: PathBuf,
    pub major: JavaMajor,
    /// 完整版本串，如 `17.0.9+9`
    pub version: String,
    pub vendor: JavaVendor,
    /// `true` = 由 mcsup 安装在平台数据目录，卸载可删文件
    pub managed: bool,
    /// Unix 秒
    pub created_at: i64,
    /// Unix 秒
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_accepts_supported_range() {
        assert!(JavaMajor::parse(JavaMajor::MIN).is_ok());
        assert!(JavaMajor::parse(17).is_ok());
        assert!(JavaMajor::parse(21).is_ok());
        assert!(JavaMajor::parse(26).is_ok());
        assert!(JavaMajor::parse(JavaMajor::MAX).is_ok());
        assert!(JavaMajor::parse(7).is_err());
        assert!(JavaMajor::parse(100).is_err());
    }

    #[test]
    fn vendor_fuzzy_roundtrip() {
        assert_eq!(
            JavaVendor::parse_fuzzy("Eclipse Adoptium"),
            JavaVendor::Temurin
        );
        assert_eq!(JavaVendor::parse_fuzzy("Azul Systems"), JavaVendor::Zulu);
        assert_eq!(JavaVendor::parse_fuzzy("BellSoft"), JavaVendor::Liberica);
        assert_eq!(JavaVendor::parse_fuzzy("Amazon"), JavaVendor::Corretto);
        assert_eq!(JavaVendor::parse_fuzzy("Oracle"), JavaVendor::System);
        assert_eq!(JavaVendor::Temurin.as_str(), "temurin");
    }

    #[test]
    fn package_type_default_is_jdk() {
        assert_eq!(JavaPackageType::default(), JavaPackageType::Jdk);
        assert_eq!(JavaPackageType::Jre.as_str(), "jre");
    }

    #[test]
    fn java_exe_name_matches_platform() {
        #[cfg(target_os = "windows")]
        assert_eq!(java_exe_name(), "java.exe");
        #[cfg(not(target_os = "windows"))]
        assert_eq!(java_exe_name(), "java");
    }
}
