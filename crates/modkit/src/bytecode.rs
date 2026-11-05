//! class 文件字节码扫描
//!
//! 两件事，都不需要完整解析 class 结构：
//!
//! 1. **Java 编译版本**：class 文件头固定布局，第 6-7 字节是大版本号
//! 2. **是否引用客户端类**：常量池里的类名是明文 UTF-8，直接在原始字节里
//!    找即可
//!
//! # Fabric 为什么扫不出结果
//!
//! Fabric 的 mod 运行在 **intermediary** 名字空间下：字节码引用的是
//! `net/minecraft/class_2338` 这类映射名，而不是 `net/minecraft/client/Minecraft`。
//! 实测某 mod 的 class 里 `class_` 命中 64 次、`net/minecraft/client/`
//! 命中 0 次。
//!
//! 所以本扫描对 **Forge / NeoForge（官方名或 MCP 名）有效**，对 Fabric
//! **只能得出「无信号」**，绝不能因此判定「非客户端 mod」。调用方必须按
//! 加载器决定是否采信本结果——见 `ZipModInspector` 的用法。

/// class 文件魔数
const CLASS_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

/// 客户端专属包的字节串（常量池里以 `/` 分隔）
const CLIENT_PACKAGES: [&[u8]; 4] = [
    b"net/minecraft/client/",
    b"net/minecraft/class_310", // intermediary 下的 MinecraftClient 类号
    b"com/mojang/blaze3d/",     // 渲染
    b"net/minecraft/src/",      // MCP 旧名
];

/// 读 class 文件头里的 Java 大版本
///
/// 响应：52 = Java 8，55 = 11，61 = 17，65 = 21。
/// 返回 `None` 表示不是合法 class 文件。
#[must_use]
pub(crate) fn java_major(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 8 || bytes[0..4] != CLASS_MAGIC {
        return None;
    }
    // minor(2) + major(2) 紧跟魔数
    Some(u32::from(u16::from_be_bytes([bytes[6], bytes[7]])))
}

/// 该 class 是否引用客户端专属类
///
/// `Some(false)` 只说明「本 class 里没找到」，不代表整个 mod 不是客户端 mod。
#[must_use]
pub(crate) fn references_client_classes(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || bytes[0..4] != CLASS_MAGIC {
        return false;
    }
    CLIENT_PACKAGES
        .iter()
        .any(|needle| contains_bytes(bytes, needle))
}

/// 在字节流里查找子串
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小 class 头（Java 21 = 65）
    fn class_with_major(major: u16) -> Vec<u8> {
        let mut v = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00];
        v.extend_from_slice(&major.to_be_bytes());
        v.push(0x00); // 后面随便补一点
        v
    }

    #[test]
    fn reads_java_21() {
        assert_eq!(java_major(&class_with_major(65)), Some(65));
    }

    #[test]
    fn reads_java_8_and_17() {
        assert_eq!(java_major(&class_with_major(52)), Some(52));
        assert_eq!(java_major(&class_with_major(61)), Some(61));
    }

    #[test]
    fn rejects_non_class_bytes() {
        assert_eq!(java_major(b"not a class file"), None);
        assert_eq!(java_major(&[]), None);
        assert_eq!(java_major(&[0xCA, 0xFE]), None);
    }

    #[test]
    fn detects_client_package_reference() {
        let mut v = class_with_major(65);
        v.extend_from_slice(b"\x00\x1dnet/minecraft/client/Minecraft");
        assert!(references_client_classes(&v));
    }

    #[test]
    fn detects_blaze3d_reference() {
        let mut v = class_with_major(65);
        v.extend_from_slice(b"com/mojang/blaze3d/platform/GlStateManager");
        assert!(references_client_classes(&v));
    }

    #[test]
    fn server_only_class_has_no_client_reference() {
        let mut v = class_with_major(65);
        v.extend_from_slice(b"net/minecraft/server/MinecraftServer");
        v.extend_from_slice(b"net/minecraft/world/level/Level");
        assert!(!references_client_classes(&v));
    }

    #[test]
    fn intermediary_names_are_not_detected() {
        // 关键限制：Fabric 的 intermediary 名扫不出来，这是已知且刻意的
        let mut v = class_with_major(65);
        v.extend_from_slice(b"net/minecraft/class_2338");
        v.extend_from_slice(b"net/minecraft/class_1937");
        assert!(!references_client_classes(&v));
    }

    #[test]
    fn rejects_non_class_even_if_it_contains_needle() {
        // 不是 class 文件就不该给出「引用了客户端类」的结论
        let v = b"net/minecraft/client/Minecraft".to_vec();
        assert!(!references_client_classes(&v));
    }
}
