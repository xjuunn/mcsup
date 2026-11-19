//! 相对路径校验：服务器目录与快照共用的跨平台规则
//!
//! 路径一律以 `/` 分隔的 POSIX 文本保存，不存平台分隔符：同一份记录换到
//! 另一个平台后仍能拼回正确落点。判定按最严的平台（Windows）执行——unix
//! 上能建出的 `a:b`、`CON`、`trail.` 到 Windows 上会写到盘符外、写进设备
//! 或被静默改名，不能在读取时才按本机规则放行。

use std::fmt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// 相对路径：服务器目录下的 POSIX 风格相对路径
///
/// 用 `/` 分隔而不是平台分隔符：记录可跨平台读取（工作区随目录整体搬迁），
/// 存 `\` 的元数据换到 unix 上就拼不出文件。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

impl RelPath {
    /// 从相对路径构造并校验
    ///
    /// 规则：非空、只含 `Normal` 分量与 `.`（`.` 直接归一掉）、不以上级跳转
    /// 起始、不含盘符前缀、每个分量都满足 `segment_is_safe`。这是文件读写
    /// 能安全落盘的前提——一个 `..` 就能让落点跑出服务器目录。
    ///
    /// # Errors
    ///
    /// 空路径、含 `..` / 根 / 盘符前缀，或分量在 Windows 上不可表示时返回
    /// [`InvalidRelPath`]
    pub fn from_path(path: &Path) -> Result<Self, InvalidRelPath> {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    let raw = || InvalidRelPath {
                        raw: path.to_string_lossy().into_owned(),
                    };
                    let part = part.to_str().ok_or_else(raw)?;
                    if !segment_is_safe(part) {
                        return Err(raw());
                    }
                    parts.push(part.to_owned());
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(InvalidRelPath {
                        raw: path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
        if parts.is_empty() {
            return Err(InvalidRelPath {
                raw: path.to_string_lossy().into_owned(),
            });
        }
        Ok(Self(parts.join("/")))
    }

    /// 从已存盘的 POSIX 文本构造并校验（读元数据用）
    ///
    /// 不能转交 [`Self::from_path`]：`Path::components` 按平台判定分隔符，
    /// 同一条文本在 Windows 上把 `\` 当分隔符切开、在 unix 上原样保留，
    /// 导致「回写后不含反斜杠」这条跨平台不变量随平台漂移。文本一律按
    /// POSIX 规则切分，`\` 在任何平台都是非法字符。
    ///
    /// 分段校验与 [`Self::from_path`] 一致：记录可能来自别的平台，一条在
    /// unix 上合法的路径到 Windows 上可能是盘符相对路径、数据流或设备名，
    /// 写入即失败或写错位置。
    ///
    /// # Errors
    ///
    /// 同上
    pub fn parse(raw: &str) -> Result<Self, InvalidRelPath> {
        let invalid = || InvalidRelPath {
            raw: raw.to_owned(),
        };
        let mut parts = Vec::new();
        for segment in raw.split('/') {
            match segment {
                "." => {}
                "" | ".." => return Err(invalid()),
                part if !segment_is_safe(part) => return Err(invalid()),
                part => parts.push(part),
            }
        }
        if parts.is_empty() {
            return Err(invalid());
        }
        Ok(Self(parts.join("/")))
    }

    /// POSIX 文本
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 拼回平台路径；只在落盘时与根目录 `join`
    #[must_use]
    pub fn to_platform_path(&self) -> PathBuf {
        self.0.split('/').collect()
    }

    /// 是否落在给定顶层项之下（备份范围判定用）
    #[must_use]
    pub fn starts_with(&self, top: &str) -> bool {
        self.0 == top
            || self
                .0
                .strip_prefix(top)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 相对路径非法
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid relative path: {raw}")]
pub struct InvalidRelPath {
    /// 原始输入
    pub raw: String,
}

/// 单个路径分量是否能在 Windows 上原样落盘
///
/// 记录要能跨平台还原，判定就必须按最严的平台（Windows）来：
///
/// - `:` 在 Windows 上是盘符相对前缀（`C:foo` 相对当前盘符）与 NTFS 数据流
///   分隔符（`file::$DATA`），`join` 出来的落点与记录不一致。
/// - `CON` / `NUL` / `COM1` / `LPT9` 等设备名即便带扩展名（`CON.txt`）也会被
///   解析成设备，写入不是失败就是写进设备。
/// - 结尾的 `.` 与空格会被 Win32 静默裁掉，`a.` 与 `a` 会互相覆盖。
/// - 控制字符在两侧平台上都无法进入合法文件名。
///
/// 这些限制对 unix 文件名是多余的，但「unix 上能写、Windows 上不能读」
/// 等于记录失效，宁可在构造路径时就拒绝。
fn segment_is_safe(part: &str) -> bool {
    if part.is_empty() || part == "." || part == ".." {
        return false;
    }
    if part
        .chars()
        .any(|c| c == ':' || c == '\\' || c == '/' || c.is_control())
    {
        return false;
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return false;
    }
    !is_windows_device_name(part)
}

/// `CON` / `NUL` / `COM1`…`COM9` / `LPT1`…`LPT9`（大小写不敏感，扩展名不影响）
fn is_windows_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = stem.as_bytes();
    bytes.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(raw: &str) -> RelPath {
        RelPath::parse(raw).expect("相对路径")
    }

    #[test]
    fn rel_path_rejects_traversal_and_absolute() {
        assert!(RelPath::from_path(Path::new("../escape")).is_err());
        assert!(RelPath::from_path(Path::new("world/../../escape")).is_err());
        assert!(RelPath::from_path(Path::new("/abs")).is_err());
        assert!(RelPath::from_path(Path::new("")).is_err());
        assert!(RelPath::from_path(Path::new("./")).is_err());
    }

    #[test]
    fn rel_path_normalizes_cur_dir_and_uses_posix_separator() {
        let path = RelPath::from_path(Path::new("./world/level.dat")).expect("ok");
        assert_eq!(path.as_str(), "world/level.dat");
        assert_eq!(
            path.to_platform_path(),
            Path::new("world").join("level.dat")
        );
    }

    /// 反斜杠在任何平台都是非法字符
    ///
    /// `parse` 曾转交 `from_path`，而 `Path::components` 按平台判分隔符：
    /// Windows 上 `\` 被切开、unix 上原样残留，同一条元数据跨平台读出的
    /// 路径不一致。记录要在平台间搬运，判定必须与平台无关。
    #[test]
    fn rel_path_rejects_backslash_on_every_platform() {
        assert!(RelPath::parse("world\\level.dat").is_err());
        assert!(RelPath::parse("-/\\").is_err());
        assert!(RelPath::parse("a/b\\c").is_err());
        // 正常路径不受影响
        assert_eq!(RelPath::parse("a/b").expect("ok").as_str(), "a/b");
    }

    /// 盘符 / NTFS ADS 的 `:` 在 Unix 上是普通字符，必须按跨平台口径拒绝
    #[test]
    fn rel_path_rejects_colon_on_every_platform() {
        assert!(RelPath::parse("C:/evil").is_err());
        assert!(RelPath::parse("a/C:evil").is_err());
        assert!(RelPath::from_path(Path::new("a/C:evil")).is_err());
    }

    #[test]
    fn rel_path_prefix_match_respects_component_boundary() {
        let path = rel("world_nether/region/r.0.0.mca");
        assert!(path.starts_with("world_nether"));
        assert!(!path.starts_with("world"));
        assert!(rel("world").starts_with("world"));
    }

    /// Windows 专有路径形态在**所有平台**上都必须被拒绝
    ///
    /// 记录可以跨平台读取：unix 上能建出 `a:b`、`CON`、`trail.` 这类名字，
    /// 一旦写进元数据，到 Windows 上还原就会写到盘符外、写进设备或被静默
    /// 改名。判定只能按最严的平台做，不能「本机允许就放行」。
    #[test]
    fn rel_path_rejects_windows_only_forms_everywhere() {
        // 盘符相对：`C:foo` 相对当前盘符，不是普通文件名
        assert!(RelPath::parse("C:foo").is_err());
        assert!(RelPath::parse("world/C:foo").is_err());
        // NTFS ADS
        assert!(RelPath::parse("server.properties:evil").is_err());
        // 设备名（含扩展名）
        for name in [
            "CON", "nul", "Aux", "prn", "COM1", "lpt9", "CON.txt", "com3.log",
        ] {
            assert!(RelPath::parse(name).is_err(), "{name} should be rejected");
        }
        // 结尾点 / 空格会被 Win32 裁掉
        assert!(RelPath::parse("world/trail.").is_err());
        assert!(RelPath::parse("world/trail ").is_err());
        // 控制字符
        assert!(RelPath::parse("world/a\nb").is_err());
        // 形似但不是设备名的仍然合法
        assert_eq!(
            RelPath::parse("world/concrete").expect("ok").as_str(),
            "world/concrete"
        );
        assert_eq!(RelPath::parse("COM10").expect("ok").as_str(), "COM10");
        assert_eq!(
            RelPath::parse("world/1.20.1").expect("ok").as_str(),
            "world/1.20.1"
        );
    }

    #[test]
    fn from_path_applies_the_same_segment_rules() {
        assert!(RelPath::from_path(Path::new("world/C:foo")).is_err());
        assert!(RelPath::from_path(Path::new("CON")).is_err());
        assert!(RelPath::from_path(Path::new("world/trail.")).is_err());
    }
}
