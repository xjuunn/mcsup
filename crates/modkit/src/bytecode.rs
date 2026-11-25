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

/// 能自己成为**顶层窗口**的 AWT / Swing 容器
///
/// 只收这类：`JOptionPane` / `JFileChooser` 需要父组件才有意义，单看常量池
/// 分不清是否真会弹窗。实测 `midnightlib` 引用 `JFileChooser`、`blue_skies`
/// 引用 `JFrame`，两者在 dedicated server 上都正常装载，不能因为出现窗口
/// 类名就报问题。
const WINDOW_CLASSES: [&[u8]; 5] = [
    b"javax/swing/JFrame",
    b"javax/swing/JDialog",
    b"javax/swing/JWindow",
    b"java/awt/Frame",
    b"java/awt/Window",
];

/// 直接终止 JVM 的调用（`Runtime.halt`）
///
/// 用「类 + 方法名」两个常量共同确认：`java/lang/Runtime` 单独出现很常见
/// （`Runtime.getRuntime().availableProcessors()`），`halt` 单独出现也可能
/// 是日志文案或配置键；两者同在一个 class 里只剩「调用 `Runtime.halt`」这
/// 一种解释。
///
/// 刻意不收 `System.exit`：`exit` 这个字符串在任何 mod 里都可能出现
/// （退出按钮、配置键、日志），收进来会立刻变成误报源。
///
/// `halt` 按**完整常量池项**比对（见 [`contains_pool_utf8`]），`halted` /
/// `asphalt` 这类更长的词不会被命中；类名按普通子串比对即可。
const HALT_CLASS: &[u8] = b"java/lang/Runtime";
const HALT_METHOD: &[u8] = b"halt";

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

/// 该 class 是否会在打开图形窗口后终止 JVM
///
/// 判定要求两件事出现在**同一个** class 里：
///
/// 1. 常量池引用一个能自成顶层窗口的容器（[`WINDOW_CLASSES`]）
/// 2. 常量池里有 `halt` 这个方法名，且同处还有 `java/lang/Runtime`
///
/// 只查窗口类名会误伤：实测 `blue_skies` 引用 `JFrame`、`midnightlib` 引用
/// `JFileChooser`，两者在 dedicated server 上都正常装载。只查 `halt` 更没
/// 意义——退出按钮、配置键都可能是这个字符串。
///
/// 「窗口 + 终止」合在一起才是服务端上的实质问题：专用服务端没有可操作的
/// 桌面，脚本要么卡在窗口上等一个永远不会到来的关闭，要么窗口一关就把 JVM
/// 一起带走。实测 `missingmodschecker` 正是这个形状（`MissingModsWindow`
/// 里 `setVisible(true)` 后 `Runtime.getRuntime().halt(0)`）。
///
/// 这是**启发式**：能证明「代码里有这条路」，不能证明每次启动都走它。
/// 降级为「待确认」（风险 `Review`）而不是直接拦死，处置由上层决定。
#[must_use]
pub(crate) fn halts_after_window(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || bytes[0..4] != CLASS_MAGIC {
        return false;
    }
    WINDOW_CLASSES
        .iter()
        .any(|needle| contains_bytes(bytes, needle))
        && contains_pool_utf8(bytes, HALT_METHOD)
        && contains_bytes(bytes, HALT_CLASS)
}

/// 常量池里是否有一个**完整等于** `text` 的 UTF-8 项
///
/// 常量池的字符串项是 `tag(1) + 长度(2, big-endian) + 字节`。带上长度前缀
/// 才不会把 `asphalt`（含 `halt`）、`halted` 这类更长的词算成方法名 `halt`。
fn contains_pool_utf8(bytes: &[u8], text: &[u8]) -> bool {
    let Ok(len) = u16::try_from(text.len()) else {
        return false;
    };
    let mut needle = Vec::with_capacity(2 + text.len());
    needle.extend_from_slice(&len.to_be_bytes());
    needle.extend_from_slice(text);
    contains_bytes(bytes, &needle)
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

    /// 把一段常量池内容拼到合法 class 头后面
    fn class_with_pool(pool: &[u8]) -> Vec<u8> {
        let mut v = class_with_major(61);
        v.extend_from_slice(pool);
        v
    }

    /// 完整的 UTF-8 常量池项写法：长度前缀 + 内容
    fn pool_utf8(text: &str) -> Vec<u8> {
        let mut v = u16::try_from(text.len())
            .expect("test strings are short")
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(text.as_bytes());
        v
    }

    /// 现场形态：窗口容器 + `Runtime.halt`
    ///
    /// 取自 `missingmodschecker` 的 `MissingModsWindow`：引用 `javax/swing/JFrame`，
    /// 窗口关闭后调用 `Runtime.getRuntime().halt(0)`。
    #[test]
    fn window_plus_halt_is_detected() {
        let mut pool = pool_utf8("javax/swing/JFrame");
        pool.extend_from_slice(&pool_utf8("watchdog"));
        pool.extend_from_slice(&pool_utf8("halt"));
        pool.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(halts_after_window(&class_with_pool(&pool)));
    }

    /// `java/awt/Window` 这类顶层容器也认
    #[test]
    fn awt_window_plus_halt_is_detected() {
        let mut pool = pool_utf8("java/awt/Frame");
        pool.extend_from_slice(&pool_utf8("halt"));
        pool.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(halts_after_window(&class_with_pool(&pool)));
    }

    /// 只有窗口 API、没有终止调用：不报
    ///
    /// 实测 `blue_skies` 引用 `JFrame` 却在 dedicated server 上正常装载；
    /// 把它报成阻断就是纯误报。
    #[test]
    fn window_without_halt_is_ignored() {
        let mut pool = pool_utf8("javax/swing/JFrame");
        pool.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(!halts_after_window(&class_with_pool(&pool)));
    }

    /// 只有终止调用、没有窗口：不报
    ///
    /// `Runtime.halt` 在关闭钩子、watchdog 里都会用，与服务端能否启动无关。
    #[test]
    fn halt_without_window_is_ignored() {
        let mut pool = pool_utf8("halt");
        pool.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(!halts_after_window(&class_with_pool(&pool)));
    }

    /// 只是名字里含 `halt` 的词不算方法名
    ///
    /// 常量池项带长度前缀，`halted` / `asphalt` 的长度不同，不会被当成 `halt`。
    #[test]
    fn longer_word_containing_halt_is_not_the_method() {
        let mut pool = pool_utf8("javax/swing/JFrame");
        pool.extend_from_slice(&pool_utf8("halted"));
        pool.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(!halts_after_window(&class_with_pool(&pool)));
    }

    /// `JFileChooser` 不是顶层窗口，不参与判定
    ///
    /// 实测 `midnightlib` 引用它并在服务端正常装载。
    #[test]
    fn non_toplevel_swing_class_is_ignored() {
        let mut pool = pool_utf8("javax/swing/JFileChooser");
        pool.extend_from_slice(&pool_utf8("halt"));
        pool.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(!halts_after_window(&class_with_pool(&pool)));
    }

    /// 非 class 字节不给结论
    #[test]
    fn non_class_bytes_never_report_window_halt() {
        let mut raw = pool_utf8("javax/swing/JFrame");
        raw.extend_from_slice(&pool_utf8("halt"));
        raw.extend_from_slice(&pool_utf8("java/lang/Runtime"));
        assert!(!halts_after_window(&raw));
        assert!(!halts_after_window(&[]));
    }
}
