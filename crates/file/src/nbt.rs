//! 最小 NBT 读取：只取 `level.dat` 里的 `DataVersion` 与 `Version.Name`
//!
//! 不追求完整 NBT 实现，但必须能正确跳过任意标签，否则 `DataVersion`
//! 只要不在开头就找不到。长度、深度都做上限，避免损坏文件把内存/栈打爆。

use domain::server::world::{DataVersion, WorldInfo};
use std::fmt;

const TAG_END: u8 = 0;
const TAG_BYTE: u8 = 1;
const TAG_SHORT: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_LONG: u8 = 4;
const TAG_FLOAT: u8 = 5;
const TAG_DOUBLE: u8 = 6;
const TAG_BYTE_ARRAY: u8 = 7;
const TAG_STRING: u8 = 8;
const TAG_LIST: u8 = 9;
const TAG_COMPOUND: u8 = 10;
const TAG_INT_ARRAY: u8 = 11;
const TAG_LONG_ARRAY: u8 = 12;

/// `level.dat` 最常见的根标签是 compound，过深嵌套视为损坏
const MAX_DEPTH: usize = 64;

/// NBT 解析失败
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NbtError {
    /// 数据在读完前结束
    Truncated,
    /// 字符串不是合法 UTF-8
    InvalidUtf8,
    /// 根标签不是 compound
    NotCompound(u8),
    /// 未知标签类型
    UnknownTag(u8),
    /// 负数长度或长度溢出
    BadLength,
    /// 嵌套超过上限
    TooDeep,
}

impl fmt::Display for NbtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("unexpected end of NBT data"),
            Self::InvalidUtf8 => f.write_str("invalid UTF-8 in NBT string"),
            Self::NotCompound(tag) => write!(f, "NBT root is not a compound (tag {tag})"),
            Self::UnknownTag(tag) => write!(f, "unknown NBT tag {tag}"),
            Self::BadLength => f.write_str("invalid NBT array/string length"),
            Self::TooDeep => f.write_str("NBT nesting too deep"),
        }
    }
}

/// 解析 `level.dat` 解压后的 NBT 字节
pub(crate) fn parse_level_dat(data: &[u8]) -> Result<WorldInfo, NbtError> {
    let mut reader = Reader::new(data);
    let root = reader.byte()?;
    if root != TAG_COMPOUND {
        return Err(NbtError::NotCompound(root));
    }
    // 根 compound 的名字对 level.dat 无意义，但仍要消费掉
    let _root_name = reader.string()?;
    let mut info = WorldInfo::default();
    walk_compound(&mut reader, &mut info, 0, false)?;
    Ok(info)
}

/// 递归遍历 compound；同时记录 `DataVersion` 与 `Version.Name`
///
/// 不跳过 compound 而是递归进入，才能到达 `Data` 里的 `DataVersion`。
fn walk_compound(
    reader: &mut Reader<'_>,
    info: &mut WorldInfo,
    depth: usize,
    in_version: bool,
) -> Result<(), NbtError> {
    if depth > MAX_DEPTH {
        return Err(NbtError::TooDeep);
    }
    loop {
        let tag = reader.byte()?;
        if tag == TAG_END {
            return Ok(());
        }
        let name = reader.string()?;
        match tag {
            TAG_INT if name == "DataVersion" => {
                info.data_version = Some(DataVersion::new(reader.int()?));
            }
            TAG_COMPOUND => {
                let nested_in_version = in_version || name == "Version";
                walk_compound(reader, info, depth + 1, nested_in_version)?;
            }
            TAG_STRING if in_version && name == "Name" => {
                info.version_name = Some(reader.string()?);
            }
            _ => skip_payload(reader, tag, depth + 1)?,
        }
    }
}

/// 跳过一个标签的负载（不关心内容）
fn skip_payload(reader: &mut Reader<'_>, tag: u8, depth: usize) -> Result<(), NbtError> {
    if depth > MAX_DEPTH {
        return Err(NbtError::TooDeep);
    }
    match tag {
        TAG_BYTE => reader.skip(1),
        TAG_SHORT => reader.skip(2),
        TAG_INT | TAG_FLOAT => reader.skip(4),
        TAG_LONG | TAG_DOUBLE => reader.skip(8),
        TAG_BYTE_ARRAY => {
            let len = reader.length()?;
            reader.skip(len)
        }
        TAG_STRING => {
            let _ = reader.string()?;
            Ok(())
        }
        TAG_LIST => {
            let element = reader.byte()?;
            let len = reader.length()?;
            for _ in 0..len {
                skip_payload(reader, element, depth + 1)?;
            }
            Ok(())
        }
        TAG_COMPOUND => loop {
            let inner = reader.byte()?;
            if inner == TAG_END {
                return Ok(());
            }
            let _ = reader.string()?;
            skip_payload(reader, inner, depth + 1)?;
        },
        TAG_INT_ARRAY => {
            let len = reader.length()?;
            reader.skip(mul(len, 4)?)
        }
        TAG_LONG_ARRAY => {
            let len = reader.length()?;
            reader.skip(mul(len, 8)?)
        }
        _ => Err(NbtError::UnknownTag(tag)),
    }
}

fn mul(len: usize, width: usize) -> Result<usize, NbtError> {
    len.checked_mul(width).ok_or(NbtError::BadLength)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], NbtError> {
        let end = self.pos.checked_add(len).ok_or(NbtError::BadLength)?;
        let slice = self.data.get(self.pos..end).ok_or(NbtError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<(), NbtError> {
        let _ = self.take(len)?;
        Ok(())
    }

    fn byte(&mut self) -> Result<u8, NbtError> {
        let byte = self
            .data
            .get(self.pos)
            .copied()
            .ok_or(NbtError::Truncated)?;
        self.pos += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, NbtError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn int(&mut self) -> Result<i32, NbtError> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn length(&mut self) -> Result<usize, NbtError> {
        let raw = self.int()?;
        usize::try_from(raw).map_err(|_| NbtError::BadLength)
    }

    fn string(&mut self) -> Result<String, NbtError> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| NbtError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_string(out: &mut Vec<u8>, text: &str) {
        let len = u16::try_from(text.len()).expect("short");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }

    fn named(out: &mut Vec<u8>, tag: u8, name: &str) {
        out.push(tag);
        push_string(out, name);
    }

    /// 复刻 level.dat 结构：root → Data → { `LastPlayed`, `DataVersion`, Version.Name }
    fn level_dat(data_version: i32, version_name: &str) -> Vec<u8> {
        let mut out = vec![TAG_COMPOUND];
        push_string(&mut out, "");
        named(&mut out, TAG_COMPOUND, "Data");

        named(&mut out, TAG_LONG, "LastPlayed");
        out.extend_from_slice(&1_700_000_000_i64.to_be_bytes());

        named(&mut out, TAG_INT, "DataVersion");
        out.extend_from_slice(&data_version.to_be_bytes());

        named(&mut out, TAG_COMPOUND, "Version");
        named(&mut out, TAG_STRING, "Name");
        push_string(&mut out, version_name);
        named(&mut out, TAG_INT, "Snapshot");
        out.extend_from_slice(&0_i32.to_be_bytes());
        out.push(TAG_END);

        // 放一个 list 和数组在末尾，确保跳读不会把结构读歪
        named(&mut out, TAG_LIST, "Enabled");
        out.push(TAG_STRING);
        out.extend_from_slice(&1_i32.to_be_bytes());
        push_string(&mut out, "vanilla");

        named(&mut out, TAG_INT_ARRAY, "Seed");
        out.extend_from_slice(&2_i32.to_be_bytes());
        out.extend_from_slice(&7_i32.to_be_bytes());
        out.extend_from_slice(&9_i32.to_be_bytes());

        out.push(TAG_END); // Data
        out.push(TAG_END); // root
        out
    }

    #[test]
    fn reads_data_version_and_version_name() {
        let info = parse_level_dat(&level_dat(4786, "26.1")).expect("parse");
        assert_eq!(info.data_version, Some(DataVersion::new(4786)));
        assert_eq!(info.version_name.as_deref(), Some("26.1"));
    }

    #[test]
    fn missing_data_version_is_none_not_error() {
        let mut out = vec![TAG_COMPOUND];
        push_string(&mut out, "");
        named(&mut out, TAG_COMPOUND, "Data");
        out.push(TAG_END);
        out.push(TAG_END);
        let info = parse_level_dat(&out).expect("parse");
        assert_eq!(info.data_version, None);
        assert_eq!(info.version_name, None);
    }

    #[test]
    fn truncated_and_non_compound_are_rejected() {
        assert_eq!(parse_level_dat(&[]), Err(NbtError::Truncated));
        assert_eq!(
            parse_level_dat(&[TAG_INT, 0, 0]),
            Err(NbtError::NotCompound(TAG_INT))
        );
    }

    #[test]
    fn runaway_nesting_is_rejected() {
        let mut out = vec![TAG_COMPOUND];
        push_string(&mut out, "");
        for _ in 0..(MAX_DEPTH + 2) {
            named(&mut out, TAG_COMPOUND, "x");
        }
        assert_eq!(parse_level_dat(&out), Err(NbtError::TooDeep));
    }
}
