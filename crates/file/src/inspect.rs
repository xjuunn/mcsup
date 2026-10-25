//! 读取世界与核心的数据版本：`level.dat`（gzip + NBT）与 `version.json`
//! （jar 内 zip 条目）

use crate::nbt;
use domain::server::props::Properties;
use domain::server::world::{DataVersion, WorldInfo};
use flate2::read::GzDecoder;
use port::{CoreInspector, InspectError};
use std::fs::File;
use std::io::{Read, Take};
use std::path::Path;
use zip::ZipArchive;
use zip::result::ZipError;

const LEVEL_DAT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const VERSION_JSON_MAX_BYTES: u64 = 64 * 1024;

/// 由文件系统读取世界与核心元数据
#[derive(Debug, Clone, Copy, Default)]
pub struct FileCoreInspector;

impl FileCoreInspector {
    /// 构建检查器
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CoreInspector for FileCoreInspector {
    fn world_info(&self, server_dir: &Path) -> Result<Option<WorldInfo>, InspectError> {
        let level_dat = server_dir.join(level_name(server_dir)?).join("level.dat");
        if !level_dat.is_file() {
            return Ok(None);
        }
        let bytes = read_capped(&level_dat, LEVEL_DAT_MAX_BYTES)?;
        let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
            gunzip(&level_dat, &bytes)?
        } else {
            bytes
        };
        nbt::parse_level_dat(&raw)
            .map(Some)
            .map_err(|err| InspectError::parse(&level_dat, err.to_string()))
    }

    fn jar_data_version(&self, jar: &Path) -> Result<Option<DataVersion>, InspectError> {
        let file = File::open(jar).map_err(|err| InspectError::io(jar, err))?;
        let mut archive =
            ZipArchive::new(file).map_err(|err| InspectError::parse(jar, zip_message(&err)))?;
        let mut entry = match archive.by_name("version.json") {
            Ok(entry) => entry,
            Err(ZipError::FileNotFound) => return Ok(None),
            Err(err) => return Err(InspectError::parse(jar, zip_message(&err))),
        };
        if entry.size() > VERSION_JSON_MAX_BYTES {
            return Err(InspectError::parse(jar, "version.json too large"));
        }
        let mut text = String::new();
        capped(&mut entry, VERSION_JSON_MAX_BYTES)
            .read_to_string(&mut text)
            .map_err(|err| InspectError::io(jar, err))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|err| InspectError::parse(jar, err.to_string()))?;
        let Some(raw) = value
            .get("world_version")
            .and_then(serde_json::Value::as_i64)
        else {
            return Ok(None);
        };
        let raw = i32::try_from(raw)
            .map_err(|_| InspectError::parse(jar, "world_version out of range"))?;
        Ok(Some(DataVersion::new(raw)))
    }
}

/// `server.properties` 的 `level-name`，缺省 `world`
///
/// 世界目录名由服务端属性决定，写死 `world` 会在自定义目录时漏检。
/// 该属性是纯文本，也可能是手改过的 `../../x`：只保留单层目录名，
/// 越界的值按缺省处理，绝不让它在 `server_dir` 之外拼出路径。
fn level_name(server_dir: &Path) -> Result<String, InspectError> {
    let path = server_dir.join("server.properties");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok("world".to_owned()),
        Err(err) => return Err(InspectError::io(&path, err)),
    };
    let props = Properties::parse(&text);
    let name = props
        .get("level-name")
        .map(str::trim)
        .filter(|name| is_single_segment(name))
        .unwrap_or("world");
    Ok(name.to_owned())
}

/// 是否单层目录名：非空、非 `.` / `..`、不含分隔符与盘符
fn is_single_segment(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

fn read_capped(path: &Path, limit: u64) -> Result<Vec<u8>, InspectError> {
    let file = File::open(path).map_err(|err| InspectError::io(path, err))?;
    let mut bytes = Vec::new();
    capped(file, limit)
        .read_to_end(&mut bytes)
        .map_err(|err| InspectError::io(path, err))?;
    if too_large(bytes.len(), limit) {
        return Err(InspectError::parse(path, "file too large"));
    }
    Ok(bytes)
}

/// 解压 gzip，并限制解压后大小（防压缩炸弹）
fn gunzip(path: &Path, compressed: &[u8]) -> Result<Vec<u8>, InspectError> {
    let mut raw = Vec::new();
    capped(GzDecoder::new(compressed), LEVEL_DAT_MAX_BYTES)
        .read_to_end(&mut raw)
        .map_err(|err| InspectError::io(path, err))?;
    if too_large(raw.len(), LEVEL_DAT_MAX_BYTES) {
        return Err(InspectError::parse(
            path,
            "decompressed level.dat too large",
        ));
    }
    Ok(raw)
}

fn capped(reader: impl Read, limit: u64) -> Take<impl Read> {
    reader.take(limit + 1)
}

fn too_large(len: usize, limit: u64) -> bool {
    match u64::try_from(len) {
        Ok(len) => len > limit,
        Err(_) => true,
    }
}

fn zip_message(err: &ZipError) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_name_defaults_when_missing_or_unsafe() {
        let tmp = tempfile::tempdir().expect("tmp");
        let props = tmp.path().join("server.properties");
        for raw in [
            "level-name=../../etc",
            "level-name=/etc",
            "level-name=..",
            "level-name=",
        ] {
            std::fs::write(&props, format!("{raw}\n")).expect("write");
            assert_eq!(
                level_name(tmp.path()).expect("level name"),
                "world",
                "{raw} 必须按缺省处理"
            );
        }
        std::fs::write(&props, "level-name=my.world\n").expect("write");
        assert_eq!(level_name(tmp.path()).expect("level name"), "my.world");
    }

    /// 越界 `level-name` 不得让世界探测读到服务器目录之外的文件
    #[test]
    fn world_info_ignores_escaping_level_name() {
        let root = tempfile::tempdir().expect("tmp");
        let outside = root.path().join("outside");
        let server = root.path().join("server");
        std::fs::create_dir_all(outside.join("world")).expect("mkdir");
        std::fs::create_dir_all(&server).expect("mkdir");
        // 服务器目录外放一个存在的 level.dat；探测若跟随越界路径就会读到它
        std::fs::write(outside.join("world").join("level.dat"), b"junk").expect("write");
        std::fs::write(
            server.join("server.properties"),
            "level-name=../outside/world\n",
        )
        .expect("write");

        let info = FileCoreInspector::new()
            .world_info(&server)
            .expect("缺省路径不存在时应返回 None 而不是解析外部文件");
        assert!(info.is_none(), "got {info:?}");
    }
}
