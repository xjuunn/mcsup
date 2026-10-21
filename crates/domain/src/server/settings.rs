//! 服务属性领域模型：键集合、值解析与校验（`server set` 用）
//!
//! 属性键统一 kebab-case，随领域扩展追加；所有值格式判定收敛在此，
//! 入口（CLI / API / MCP）只做参数搬运与交互补齐。

use crate::java::JavaMajor;
use std::fmt;
use thiserror::Error;

/// 内存下限（MiB）：低于此值 JVM 无法正常服务玩家
pub const MIN_RAM_MIB: u32 = 128;

/// 未识别的服务属性键
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown server setting key: {raw}")]
pub struct UnknownSettingKey {
    pub raw: String,
}

/// 属性值非法（载荷保留原始输入，非数字输入也能原样带回给用户）
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid value for {key}: {raw}")]
pub struct InvalidSettingValue {
    pub key: SettingKey,
    pub raw: String,
}

/// 服务属性键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingKey {
    /// 绑定本服务的 JVM 大版本
    JavaVersion,
    /// 启动内存上限（MiB）
    Ram,
    /// 机器启动时自动拉起
    Autostart,
}

impl SettingKey {
    /// 全部键：交互选择、用法提示与文档的唯一清单
    pub const ALL: [SettingKey; 3] = [Self::JavaVersion, Self::Ram, Self::Autostart];

    /// 键名（kebab-case，即命令行口径）
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JavaVersion => "java-version",
            Self::Ram => "ram",
            Self::Autostart => "autostart",
        }
    }

    /// 解析键名；大小写不敏感，首尾空白自动裁剪
    ///
    /// # Errors
    ///
    /// 未知键返回 [`UnknownSettingKey`]（载荷为原始输入）
    pub fn parse(raw: &str) -> Result<Self, UnknownSettingKey> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "java-version" => Ok(Self::JavaVersion),
            "ram" => Ok(Self::Ram),
            "autostart" => Ok(Self::Autostart),
            _ => Err(UnknownSettingKey {
                raw: raw.to_owned(),
            }),
        }
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 服务属性集：`server set` 的持久化对象
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerSettings {
    /// 绑定的 JVM 大版本；`None` = 自动取已注册运行时中的最高大版本
    pub java_major: Option<JavaMajor>,
    /// 堆上限（MiB）；`None` = 平台默认（默认值口径由 service 持有）
    pub ram_mib: Option<u32>,
    /// 机器启动时自动拉起
    pub autostart: bool,
}

impl ServerSettings {
    /// 按键写入原始值；解析规则收敛在本类型，调用方不再各自匹配键
    ///
    /// # Errors
    ///
    /// 值不满足该键的格式（见 [`parse_ram_mib`] 与各键说明）时返回
    /// [`InvalidSettingValue`]，属性集不变
    pub fn apply(&mut self, key: SettingKey, raw: &str) -> Result<(), InvalidSettingValue> {
        let invalid = || InvalidSettingValue {
            key,
            raw: raw.to_owned(),
        };
        match key {
            SettingKey::JavaVersion => {
                self.java_major = Some(JavaMajor::parse_str(raw).map_err(|_| invalid())?);
            }
            SettingKey::Ram => {
                self.ram_mib = Some(parse_ram_mib(raw)?);
            }
            SettingKey::Autostart => {
                self.autostart = match raw.trim().to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(invalid()),
                };
            }
        }
        Ok(())
    }

    /// 键的当前值（人类展示口径）；`None` 表示该键尚未设置
    #[must_use]
    pub fn value_of(&self, key: SettingKey) -> Option<String> {
        match key {
            SettingKey::JavaVersion => self.java_major.map(|major| major.to_string()),
            SettingKey::Ram => self.ram_mib.map(|mib| format!("{mib}M")),
            SettingKey::Autostart => Some(self.autostart.to_string()),
        }
    }
}

/// 解析内存文本为 MiB：`2G` / `1024M` / 裸数字（按 MiB），大小写不敏感
///
/// # Errors
///
/// 格式非法、低于 [`MIN_RAM_MIB`] 或换算溢出时返回 [`InvalidSettingValue`]
/// （key 恒为 [`SettingKey::Ram`]）
pub fn parse_ram_mib(raw: &str) -> Result<u32, InvalidSettingValue> {
    let invalid = || InvalidSettingValue {
        key: SettingKey::Ram,
        raw: raw.to_owned(),
    };
    let trimmed = raw.trim();
    let (digits, unit) = match trimmed.as_bytes().last().map(u8::to_ascii_uppercase) {
        Some(b'G') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some(b'M') => (&trimmed[..trimmed.len() - 1], 1u64),
        _ => (trimmed, 1u64),
    };
    let count: u64 = digits.trim().parse().map_err(|_| invalid())?;
    let mib = count.checked_mul(unit).ok_or_else(invalid)?;
    let mib = u32::try_from(mib).map_err(|_| invalid())?;
    if mib < MIN_RAM_MIB {
        return Err(invalid());
    }
    Ok(mib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_key_roundtrip_and_unknown() {
        for key in SettingKey::ALL {
            assert_eq!(SettingKey::parse(key.as_str()).expect("roundtrip"), key);
        }
        assert_eq!(
            SettingKey::parse(" RAM ").expect("case and space insensitive"),
            SettingKey::Ram
        );
        let err = SettingKey::parse("max-players").expect_err("unknown");
        assert_eq!(err.raw, "max-players");
    }

    #[test]
    fn parse_ram_accepts_suffix_and_bare_mib() {
        assert_eq!(parse_ram_mib("2G").expect("2G"), 2048);
        assert_eq!(parse_ram_mib("1024M").expect("1024M"), 1024);
        assert_eq!(parse_ram_mib("2g").expect("lowercase"), 2048);
        assert_eq!(parse_ram_mib(" 512 ").expect("bare mib"), 512);
    }

    #[test]
    fn parse_ram_rejects_bad_format_and_out_of_range() {
        assert!(parse_ram_mib("").is_err());
        assert!(parse_ram_mib("abc").is_err());
        assert!(parse_ram_mib("1.5G").is_err());
        assert!(parse_ram_mib("2T").is_err());
        assert!(parse_ram_mib("0").is_err(), "低于下限");
        assert!(parse_ram_mib("64").is_err(), "低于下限");
        assert!(parse_ram_mib("99999999G").is_err(), "溢出");
    }

    #[test]
    fn apply_sets_each_key_and_rejects_invalid() {
        let mut settings = ServerSettings::default();
        settings.apply(SettingKey::JavaVersion, "21").expect("java");
        settings.apply(SettingKey::Ram, "2G").expect("ram");
        settings
            .apply(SettingKey::Autostart, "TRUE")
            .expect("autostart");
        assert_eq!(
            settings,
            ServerSettings {
                java_major: Some(JavaMajor::parse(21).expect("21")),
                ram_mib: Some(2048),
                autostart: true,
            }
        );

        let snapshot = settings;
        assert!(settings.apply(SettingKey::JavaVersion, "abc").is_err());
        assert!(settings.apply(SettingKey::Autostart, "yes").is_err());
        // 失败的 apply 不改变属性集
        assert_eq!(settings, snapshot);
    }

    #[test]
    fn value_of_uses_human_canonical_forms() {
        let mut settings = ServerSettings::default();
        assert_eq!(settings.value_of(SettingKey::JavaVersion), None);
        assert_eq!(settings.value_of(SettingKey::Ram), None);
        assert_eq!(
            settings.value_of(SettingKey::Autostart),
            Some("false".to_owned())
        );

        settings.apply(SettingKey::Ram, "2G").expect("ram");
        assert_eq!(settings.value_of(SettingKey::Ram), Some("2048M".to_owned()));
    }
}
