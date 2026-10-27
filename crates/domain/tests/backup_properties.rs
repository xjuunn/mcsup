//! 领域判定的属性测试：随机输入下必须恒成立的性质
//!
//! 单元测试用「我想到的例子」，属性测试用「随机构造的例子」。备份最危险的
//! 两类 bug——路径穿越与键空间塌缩——恰好都是「某个没想到的输入」触发的，
//! 所以用属性把整个输入空间一起钉住。
//!
//! 失败时 proptest 会把反例写进 `proptest-regressions/`：**那个文件必须一起
//! 提交**，否则下次同类输入不再被自动测到（见 `docs/testing.md` 门禁 G3）。

#![allow(clippy::expect_used)]

use domain::backup::{ContentHash, RelPath};
use proptest::prelude::*;

// ---- 策略 ----

/// POSIX 相对路径的任意分段（合法字符集内）
///
/// 排除 `.` / `..`：它们不是「名字」，领域本来就拒绝。生成器不该用它们去
/// 冒充合法输入，否则测的是生成器而不是实现。同理排除结尾点、`:` 与
/// Windows 设备名（`CON` / `NUL` / `COM1`…）：这些是领域要拒绝的危险形态，
/// 不该混进「合法路径」策略里。统一用 `s` 起头最省事——设备名都不以 `s`
/// 开头，扩展名前也不会留出结尾点。
fn segment() -> impl Strategy<Value = String> {
    (
        "[A-Za-z0-9_-]{1,20}",
        proptest::option::of("[A-Za-z0-9_-]{1,8}"),
    )
        .prop_map(|(stem, ext)| match ext {
            Some(ext) => format!("s{stem}.{ext}"),
            None => format!("s{stem}"),
        })
}

/// 任意层数的合法相对路径
fn rel_path() -> impl Strategy<Value = String> {
    proptest::collection::vec(segment(), 1..6).prop_map(|parts| parts.join("/"))
}

/// 带「危险」成分的路径：绝对路径、`..`、反斜杠、空段、盘符、ADS、设备名、
/// 结尾点与空格
fn hostile_path() -> impl Strategy<Value = String> {
    let pieces = prop_oneof![
        Just("/".to_owned()),
        Just("\\".to_owned()),
        Just("..".to_owned()),
        Just(".".to_owned()),
        Just("C:".to_owned()),
        Just("C:evil".to_owned()),
        Just("file:stream".to_owned()),
        Just("NUL".to_owned()),
        Just("CON.txt".to_owned()),
        Just("COM1".to_owned()),
        Just("trail.".to_owned()),
        Just("trail ".to_owned()),
        Just(String::new()),
        segment(),
    ];
    proptest::collection::vec(pieces, 1..8).prop_map(|parts| parts.join("/"))
}

// ---- RelPath ----

proptest! {
    /// 合法路径往返：`parse` 之后 `as_str` 与 `to_platform_path` 都不丢层
    #[test]
    fn rel_path_round_trips(raw in rel_path()) {
        let rel = RelPath::parse(&raw).expect("合法路径必须被接受");
        prop_assert_eq!(rel.as_str(), raw.as_str());
        let back = RelPath::from_path(&rel.to_platform_path())
            .expect("平台路径必须能再解析回去");
        prop_assert_eq!(back.as_str(), rel.as_str());
    }

    /// 解析成功的路径永不含 `..` / 绝对分量：这是路径穿越的唯一防线
    #[test]
    fn accepted_rel_path_never_escapes(raw in hostile_path()) {
        if let Ok(rel) = RelPath::parse(&raw) {
            for part in rel.as_str().split('/') {
                prop_assert!(part != "..", "路径穿越：{raw}");
                prop_assert!(!part.is_empty(), "空段：{raw}");
                prop_assert!(!part.contains('\\'), "反斜杠残留：{raw}");
                // Windows 上不可表示的形态一律不该被接受
                prop_assert!(!part.contains(':'), "盘符 / ADS 残留：{raw}");
                prop_assert!(
                    !part.ends_with('.') && !part.ends_with(' '),
                    "结尾点或空格会被 Win32 裁掉：{raw}"
                );
            }
            // 拼回平台路径后，起点仍然是相对路径
            prop_assert!(!rel.to_platform_path().is_absolute(), "{raw}");
        }
    }

    /// 拼接回平台路径的层数恒等于 POSIX 文本的段数
    #[test]
    fn platform_path_keeps_the_same_depth(raw in rel_path()) {
        let rel = RelPath::parse(&raw).expect("合法路径");
        let depth = rel.as_str().split('/').count();
        prop_assert_eq!(rel.to_platform_path().components().count(), depth);
    }

    /// 顶层项判定：`starts_with` 只看层级，不做前缀匹配
    #[test]
    fn starts_with_matches_whole_segments(top in segment(), rest in proptest::collection::vec(segment(), 0..3)) {
        let mut parts = vec![top.clone()];
        parts.extend(rest.iter().cloned());
        let raw = parts.join("/");
        let Ok(rel) = RelPath::parse(&raw) else { return Ok(()) };
        prop_assert!(rel.starts_with(&top), "{raw} 必须以顶层项 {top} 起头");
        // 前缀相同但层级不同：`worldx` 不属于 `world`
        let lookalike = format!("{top}x");
        prop_assert!(!rel.starts_with(&lookalike));
    }
}

// ---- 内容哈希 ----

proptest! {
    /// 哈希只接受小写十六进制 64 位：其它一律拒绝，不做宽容解析
    #[test]
    fn content_hash_accepts_only_lowercase_hex64(raw in ".*") {
        let accepted = ContentHash::parse(&raw).is_ok();
        let should = raw.len() == ContentHash::HEX_LEN
            && raw.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        prop_assert_eq!(accepted, should, "输入 = {:?}", raw);
    }

    /// 接受之后的往返稳定
    #[test]
    fn accepted_content_hash_round_trips(bytes in proptest::collection::vec(0u8..16, ContentHash::HEX_LEN)) {
        use std::fmt::Write as _;
        let mut raw = String::with_capacity(ContentHash::HEX_LEN);
        for byte in &bytes {
            write!(raw, "{byte:x}").expect("写入 String 不会失败");
        }
        let hash = ContentHash::parse(&raw).expect("64 位小写十六进制");
        prop_assert_eq!(hash.as_str(), raw.as_str());
        let reparsed = ContentHash::parse(hash.as_str()).expect("再解析");
        prop_assert_eq!(reparsed.as_str(), hash.as_str());
    }
}

#[cfg(test)]
mod strategy_self_check {
    use super::*;

    /// 策略本身必须是有效的：随机生成的合法路径都能被领域接受
    ///
    /// 没有这条自检，策略写错会让上面的属性全部「空转通过」——生成器永远
    /// 产出被拒的输入，属性就成了死代码。
    #[test]
    fn rel_path_strategy_generates_acceptable_inputs() {
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        runner
            .run(&rel_path(), |raw| {
                assert!(RelPath::parse(&raw).is_ok(), "策略产出了非法路径：{raw}");
                Ok(())
            })
            .expect("策略必须有效");
    }
}
