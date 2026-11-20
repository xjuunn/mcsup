//! 最小 base64（标准字母表，带 `=` 填充）
//!
//! 键值与密钥是字节，而 JSON 只方便承载文本。引一个依赖不值得：编解码是
//! 纯函数、输入来自插件，必须可测且不得 panic。解码拒绝非法字符与错误长度，
//! 调用方把它映射成 `invalid_input`。

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 编码
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[(triple >> 18) as usize & 0x3f]));
        out.push(char::from(ALPHABET[(triple >> 12) as usize & 0x3f]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[(triple >> 6) as usize & 0x3f])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[triple as usize & 0x3f])
        } else {
            '='
        });
    }
    out
}

/// 解码
///
/// # Errors
///
/// 出现非字母表字符、长度不是 4 的倍数或填充位置非法时返回错误说明
// 每段 6 位，右移后仍在 u8 范围内
#[allow(clippy::cast_possible_truncation)]
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 length must be a multiple of 4".to_owned());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let chunk = &bytes[index..index + 4];
        let mut values = [0_u32; 4];
        let mut padding = 0;
        for (slot, byte) in chunk.iter().enumerate() {
            values[slot] = if *byte == b'=' {
                if slot < 2 {
                    return Err("base64 padding is only allowed at the end".to_owned());
                }
                padding += 1;
                0
            } else {
                if padding > 0 {
                    return Err("base64 data after padding".to_owned());
                }
                value_of(*byte)?
            };
        }
        if padding > 0 && index + 4 < bytes.len() {
            return Err("base64 data after padding".to_owned());
        }
        let triple = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push(((triple >> 16) & 0xFF) as u8);
        if padding < 2 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if padding == 0 {
            out.push((triple & 0xFF) as u8);
        }
        index += 4;
    }
    Ok(out)
}

fn value_of(byte: u8) -> Result<u32, String> {
    match byte {
        b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
        b'a'..=b'z' => Ok(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Ok(u32::from(byte - b'0') + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        other => Err(format!("invalid base64 character: {:?}", char::from(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_all_padding_lengths() {
        for input in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0_u8, 255, 128, 1][..],
        ] {
            assert_eq!(decode(&encode(input)).expect("decode"), input);
        }
    }

    #[test]
    fn matches_known_vectors() {
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_malformed_input() {
        for raw in ["Zg=", "Z===", "=Zg=", "Zg==Zg==", "Zg$=", "中"] {
            assert!(decode(raw).is_err(), "{raw}");
        }
    }
}
