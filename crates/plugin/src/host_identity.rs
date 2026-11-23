//! 宿主构建标识：常驻宿主是否仍在跑「当前这个可执行文件」
//!
//! 插件宿主是常驻进程，二进制被替换（重新编译、`cargo install`、升级安装包）
//! 不会影响已经在跑的那个进程：它继续执行旧镜像里的代码。命令行这一侧因此
//! 无法靠「宿主可达」判断一切正常——可达只说明有人监听，不说明监听者是不是
//! 当前构建。
//!
//! 判定只需三样东西，都是宿主自己在启动时就能记下的：编译进二进制的版本、
//! 自身可执行文件路径、以及它的字节数与修改时间。命令行拿自己的同一组值来比，
//! 三种不一致各对应一种成因（见 [`HostStaleness`]），用户提示才能说清是
//! 「版本换了」还是「同一个路径上的文件被替换了」。
//!
//! 判定本身是纯函数（[`BuildFingerprint::staleness`]），不碰 IO；取自身标识的
//! 动作在 [`BuildFingerprint::current`]，失败一律降级成「字段未知」而不是报错：
//! 拿不到元数据时宁可判「一致」，也不能因此把用户可用的宿主杀掉。

use port::plugin::HostError;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 一组构建标识
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildFingerprint {
    /// 编译进二进制的版本（`CARGO_PKG_VERSION`）
    version: String,
    /// 可执行文件路径；取不到时为 `None`
    exe: Option<PathBuf>,
    /// 可执行文件字节数
    len: Option<u64>,
    /// 可执行文件最后修改时间（Unix 秒）
    mtime_secs: Option<u64>,
}

impl BuildFingerprint {
    /// 当前进程的构建标识
    ///
    /// 宿主在启动时调用一次并保留：之后即使磁盘上的文件被替换，它报告仍是
    /// 「我启动时是什么」，这正是判定过期所需的基准。
    #[must_use]
    pub fn current() -> Self {
        match std::env::current_exe() {
            Ok(exe) => Self::of_exe(&exe),
            Err(_) => Self {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Self::default()
            },
        }
    }

    /// 指定可执行文件的构建标识；元数据读不到时只保留路径
    #[must_use]
    pub fn of_exe(exe: &Path) -> Self {
        let meta = std::fs::metadata(exe).ok();
        let (len, mtime_secs) = match &meta {
            Some(meta) => (
                Some(meta.len()),
                meta.modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|elapsed| elapsed.as_secs()),
            ),
            None => (None, None),
        };
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            exe: Some(exe.to_path_buf()),
            len,
            mtime_secs,
        }
    }

    /// 编译进二进制的版本
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 可执行文件路径
    #[must_use]
    pub fn exe(&self) -> Option<&Path> {
        self.exe.as_deref()
    }

    /// 可执行文件字节数
    #[must_use]
    pub fn exe_len(&self) -> Option<u64> {
        self.len
    }

    /// 可执行文件最后修改时间（Unix 秒）
    #[must_use]
    pub fn mtime_secs(&self) -> Option<u64> {
        self.mtime_secs
    }

    /// 序列化成控制帧的载荷
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "exe": self.exe.as_ref().map(|path| path.to_string_lossy().to_string()),
            "len": self.len,
            "mtime_secs": self.mtime_secs,
        })
    }

    /// 从控制帧载荷还原
    ///
    /// 只要求 `version`：路径与元数据缺失时按「未知」处理，不因此报错——旧宿主
    /// 不认这个控制方法时调用方本来就该走别的分支。
    ///
    /// # Errors
    ///
    /// 载荷不是对象或缺少 `version` 时返回 `invalid_input`
    pub fn from_json(value: &Value) -> Result<Self, HostError> {
        let object = value
            .as_object()
            .ok_or_else(|| HostError::invalid_input("host identity must be an object"))?;
        let version = object
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| HostError::invalid_input("host identity is missing `version`"))?;
        Ok(Self {
            version: version.to_owned(),
            exe: object.get("exe").and_then(Value::as_str).map(PathBuf::from),
            len: object.get("len").and_then(Value::as_u64),
            mtime_secs: object.get("mtime_secs").and_then(Value::as_u64),
        })
    }

    /// 与「当前构建」比较
    ///
    /// 顺序固定为「版本 → 路径 → 文件元数据」：越靠前的差异越容易解释，用户
    /// 看到的提示才不会把「升级到了另一个版本」说成「文件被替换」。
    #[must_use]
    pub fn staleness(&self, local: &Self) -> HostStaleness {
        if self.version != local.version {
            return HostStaleness::VersionMismatch {
                host: self.version.clone(),
                local: local.version.clone(),
            };
        }
        // 一边拿不到路径时不据此判定，交给元数据比较
        if let (Some(host), Some(local)) = (&self.exe, &local.exe)
            && host != local
        {
            return HostStaleness::DifferentExe {
                host: host.clone(),
                local: local.clone(),
            };
        }
        let changed_len =
            matches!((self.len, local.len), (Some(host), Some(local)) if host != local);
        let changed_mtime = matches!(
            (self.mtime_secs, local.mtime_secs),
            (Some(host), Some(local)) if host != local
        );
        if (changed_len || changed_mtime)
            && let Some(path) = local.exe.clone().or_else(|| self.exe.clone())
        {
            return HostStaleness::BinaryReplaced { path };
        }
        HostStaleness::Current
    }
}

/// 比较结果
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostStaleness {
    /// 宿主跑的就是当前构建
    Current,
    /// 编译版本不同：宿主是另一个版本的二进制
    VersionMismatch {
        /// 宿主报告的版本
        host: String,
        /// 当前可执行文件的版本
        local: String,
    },
    /// 同一个版本，但不是同一个可执行文件
    DifferentExe {
        /// 宿主自己的路径
        host: PathBuf,
        /// 当前可执行文件路径
        local: PathBuf,
    },
    /// 同一路径上的文件在宿主启动之后被替换
    BinaryReplaced {
        /// 被替换的路径
        path: PathBuf,
    },
}

impl HostStaleness {
    /// 是否「不一致」
    #[must_use]
    pub fn is_stale(&self) -> bool {
        !matches!(self, Self::Current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(version: &str, exe: &str, len: u64, mtime_secs: u64) -> BuildFingerprint {
        BuildFingerprint {
            version: version.to_owned(),
            exe: Some(PathBuf::from(exe)),
            len: Some(len),
            mtime_secs: Some(mtime_secs),
        }
    }

    #[test]
    fn identical_fingerprints_are_current() {
        let host = fingerprint("0.1.0", "/opt/mcsup", 6_544_384, 1_763_000_000);
        assert_eq!(host.staleness(&host.clone()), HostStaleness::Current);
        assert!(!host.staleness(&host).is_stale());
    }

    #[test]
    fn version_difference_is_reported_before_anything_else() {
        let host = fingerprint("0.1.0", "/opt/mcsup", 1, 1);
        let local = fingerprint("0.2.0", "/opt/mcsup", 1, 1);
        assert_eq!(
            host.staleness(&local),
            HostStaleness::VersionMismatch {
                host: "0.1.0".to_owned(),
                local: "0.2.0".to_owned(),
            }
        );
    }

    #[test]
    fn other_executable_at_same_version_is_stale() {
        let host = fingerprint("0.1.0", "/home/u/.cargo/bin/mcsup", 10, 100);
        let local = fingerprint("0.1.0", "/repo/target/debug/mcsup", 10, 100);
        assert_eq!(
            host.staleness(&local),
            HostStaleness::DifferentExe {
                host: PathBuf::from("/home/u/.cargo/bin/mcsup"),
                local: PathBuf::from("/repo/target/debug/mcsup"),
            }
        );
    }

    #[test]
    fn replaced_binary_on_the_same_path_is_stale() {
        // 宿主 11-20 启动，二进制 11-23 被重新编译：路径与版本都没变，
        // 只有元数据变了，这正是「升级后旧宿主一直活着」的形态。
        let host = fingerprint(
            "0.1.0",
            "/home/u/.cargo/bin/mcsup",
            6_544_384,
            1_763_000_000,
        );
        let local = fingerprint(
            "0.1.0",
            "/home/u/.cargo/bin/mcsup",
            6_900_000,
            1_763_500_000,
        );
        assert_eq!(
            host.staleness(&local),
            HostStaleness::BinaryReplaced {
                path: PathBuf::from("/home/u/.cargo/bin/mcsup"),
            }
        );
        // 只有大小相同、mtime 不同也算替换（重新编译常会改变大小，但不可依赖）
        let mtime_only = fingerprint(
            "0.1.0",
            "/home/u/.cargo/bin/mcsup",
            6_544_384,
            1_763_500_000,
        );
        assert!(host.staleness(&mtime_only).is_stale());
    }

    #[test]
    fn unknown_metadata_never_reports_stale() {
        // 取不到元数据（权限、非普通文件）时宁可判一致：杀掉用户可用的宿主
        // 比漏掉一次自愈更糟。
        let host = BuildFingerprint {
            version: "0.1.0".to_owned(),
            exe: None,
            len: None,
            mtime_secs: None,
        };
        let local = fingerprint("0.1.0", "/home/u/.cargo/bin/mcsup", 10, 100);
        assert_eq!(host.staleness(&local), HostStaleness::Current);
    }

    #[test]
    fn json_round_trip_keeps_every_field() {
        let identity = fingerprint("0.1.0", "/home/u/.cargo/bin/mcsup", 42, 7);
        let wire = identity.to_json();
        assert_eq!(BuildFingerprint::from_json(&wire).expect("parse"), identity);
    }

    #[test]
    fn json_without_version_is_rejected() {
        let err = BuildFingerprint::from_json(&json!({ "exe": "/x" })).expect_err("must fail");
        assert_eq!(err.code, port::plugin::ErrorCode::InvalidInput);
    }
}
