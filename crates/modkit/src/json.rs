//! 容错 JSON 解析
//!
//! 上游元数据文件由各家构建工具生成，合规程度参差。实测存在两类问题：
//!
//! 1. **字符串里有裸控制字符**：`description` 直接写换行，JSON 规范要求转义
//!    成 `\n`。严格解析器报 `Invalid control character`。
//! 2. **UTF-8 BOM**：部分 Windows 工具会写 BOM，`serde_json` 不接受。
//!
//! 处理策略是**先修补再解析**，而不是「解析失败就当没有元数据」：元数据里
//! 的 `depends` 决定依赖闭包，丢掉会让用户以为这个 mod 没有依赖。
//!
//! 修补只针对字符串字面量内部的控制字符，不动结构：`{` `}` `,` `:` 等
//! 语法字符若被错误地写进字符串，说明文件已无法可靠还原，此时仍返回错误。

/// 解析 JSON，容忍字符串内的裸控制字符与 BOM
///
/// 返回 `None` 表示文件无法可靠解析（不是合法 JSON，修补也救不回来）。
/// 调用方应当把它当作「元数据认不出」而非错误：mod 本身可能完全正常。
#[must_use]
pub(crate) fn parse_lenient(raw: &[u8]) -> Option<serde_json::Value> {
    // 快路径：合规则不修补
    if let Ok(value) = serde_json::from_slice(raw) {
        return Some(value);
    }
    let text = decode_without_bom(raw)?;
    let repaired = escape_control_chars_in_strings(&text);
    serde_json::from_str(&repaired).ok()
}

/// 去掉 UTF-8 BOM 并转成字符串
fn decode_without_bom(raw: &[u8]) -> Option<String> {
    let stripped = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    String::from_utf8(stripped.to_vec()).ok()
}

/// 把字符串字面量内部的裸控制字符转义
///
/// 只处理 `< 0x20` 的控制字符；已转义的 `\n` 是两个字符（`\` 与 `n`），
/// 不会被误伤。字符串边界按未转义的双引号切分。
fn escape_control_chars_in_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if !in_string {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
            continue;
        }
        if escaped {
            // 上一个字符是反斜杠：这个字符属于转义序列，原样保留
            escaped = false;
            out.push(ch);
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
                out.push(ch);
            }
            '"' => {
                in_string = false;
                out.push(ch);
            }
            c if (c as u32) < 0x20 => {
                // 裸控制字符：转成合法转义
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    other => {
                        // 其余控制字符用 \\u00XX
                        use std::fmt::Write as _;
                        let _ = write!(out, "\\u{:04x}", other as u32);
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_json_unchanged() {
        let raw = br#"{"id":"a","depends":{"minecraft":"1.21.1"}}"#;
        let value = parse_lenient(raw).expect("valid json");
        assert_eq!(value["id"], "a");
    }

    #[test]
    fn repairs_bare_newline_inside_string() {
        // 这是 Debugify 的真实形态：description 里直接换行
        let raw = b"{\"id\":\"debugify\",\"description\":\"Fixes bugs\n\nLicense:\nLGPL\"}";
        let value = parse_lenient(raw).expect("repaired");
        assert_eq!(value["id"], "debugify");
        assert!(
            value["description"]
                .as_str()
                .unwrap()
                .contains("Fixes bugs")
        );
    }

    #[test]
    fn repairs_bare_tab_and_cr() {
        let raw = b"{\"d\":\"a\tb\rc\"}";
        let value = parse_lenient(raw).expect("repaired");
        assert_eq!(value["d"], "a\tb\rc");
    }

    #[test]
    fn strips_utf8_bom() {
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice(br#"{"id":"a"}"#);
        assert_eq!(parse_lenient(&raw).expect("bom ok")["id"], "a");
    }

    #[test]
    fn keeps_escaped_sequences_intact() {
        // 已转义的 \\n 不能被二次转义成字面量反斜杠+n
        let raw = br#"{"d":"line1\nline2"}"#;
        let value = parse_lenient(raw).expect("valid");
        assert_eq!(value["d"], "line1\nline2");
    }

    #[test]
    fn does_not_repair_structural_breakage() {
        // 缺少闭合括号：修补救不回来，必须返回 None
        let raw = br#"{"id":"a""#;
        assert!(parse_lenient(raw).is_none());
    }

    #[test]
    fn handles_quote_inside_string_via_escape() {
        let raw = br#"{"d":"say \"hi\" now"}"#;
        let value = parse_lenient(raw).expect("valid");
        assert_eq!(value["d"], "say \"hi\" now");
    }

    #[test]
    fn non_utf8_returns_none() {
        let raw = [0xFF, 0xFE, 0x00];
        assert!(parse_lenient(&raw).is_none());
    }
}
