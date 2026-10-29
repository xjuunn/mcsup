//! 按需配对授权：落盘的**一次性**配对码
//!
//! 配对码有两种运行形态（`docs/remote.md`「安全」），差别只在**谁决定发码**：
//!
//! - **连续**（[`crate::code::PairingCodeStore`]）：SSH 引导与前台调试。屏幕上
//!   总有当前码，过期自动轮换。会话本身有时限，码跟着会话走。
//! - **按需**（本模块）：后台常驻。**启动时不持有任何码**，只有管理员在本机
//!   显式要求时才生成一枚，到期即无码。
//!
//! 为什么按需形态必须落盘：后台进程与管理员是两个进程，发码发生在命令行、
//! 兑换发生在 agent。把码留在 agent 内存里就需要一条本机 IPC 通道（新依赖、
//! Windows 命名管道 DACL、macOS 只给 pid/uid），而同一个数据目录本来就有两个
//! 0600 文件靠「每次读盘」协作（名册与凭证）。配对码没有理由例外。
//!
//! 落盘只存**摘要**（与 token 同一口径）：拿到文件不足以兑换；管理员丢了码就
//! 重发一枚，旧码立即失效。
//!
//! 判定不在这里。短时有效 / 单次使用 / 限速冷却三条不变量全部由
//! [`PairingSlot`] 回答（见 `crate::code` 的说明），本模块只负责「码存在哪、
//! 怎么原子地读-改-写」。
//!
//! 并发：发码与兑换都是读-改-写，用**同一把跨进程文件锁**串行化。锁挂在文件
//! 句柄上（Unix `flock` / Windows `LockFileEx`），进程崩溃即释放——不用「文件
//! 存在即持锁」，那需要靠 pid 存活探测回收，回收窗口里会误删别人刚建的锁。

use crate::code::{PairingCode, PairingCodeError, PairingSlot, PairingWindow, Redeem};
use crate::credential::{CredentialError, read_private, write_private};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 按需形态的默认有效期：5 分钟
///
/// 比连续形态的 10 分钟短：按需发码时管理员就在机器旁边，窗口只需覆盖「抄到
/// 另一台机器并输进去」这段时间。窗口越短，暴露面越小。
pub const DEFAULT_ON_DEMAND_TTL_SECS: u64 = 5 * 60;

/// 允许的最短有效期（`--ttl` 下限）
pub const MIN_TTL_SECS: u64 = 30;

/// 允许的最长有效期（`--ttl` 上限）
///
/// 超过这个值就等于退化成「长期有效码」，与本模块的目的相反，因此拒绝而不是
/// 静默夹紧：静默夹紧会让用户以为自己设了一个小时的窗口。
pub const MAX_TTL_SECS: u64 = 60 * 60;

/// 单次等锁上限
///
/// 发码与兑换都是毫秒级操作，10 秒只可能意味着别的进程卡住了。
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 授权文件读写失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GrantError {
    /// 读写失败或文件权限过宽（复用凭证文件的判定）
    #[error(transparent)]
    Store(#[from] CredentialError),
    /// 系统随机源不可用
    #[error(transparent)]
    Random(#[from] PairingCodeError),
    /// `--ttl` 不在 [`MIN_TTL_SECS`, `MAX_TTL_SECS`] 之间
    #[error("pairing code lifetime must be {MIN_TTL_SECS}..={MAX_TTL_SECS} seconds, got {0}")]
    InvalidTtl(u64),
    /// 跨进程锁在超时内没拿到（超时值见 `LOCK_TIMEOUT`）
    #[error("pairing grant is locked by another process: {0}")]
    LockBusy(String),
    /// 锁文件本身读写失败
    #[error("pairing grant lock {path}: {source}")]
    LockIo {
        /// 锁文件路径
        path: PathBuf,
        /// 底层错误
        #[source]
        source: std::io::Error,
    },
}

/// 一枚已发码的对外信息：**不含码本身**
///
/// 码只在 [`PairingGrant::issue`] 的返回值里出现一次，之后任何读取路径
/// （`serve status`、日志、JSON 输出）都只能看到这个结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantStatus {
    /// 剩余有效秒数
    pub remaining_secs: u64,
    /// 是否处于猜码冷却
    pub cooling_down: bool,
}

/// 落盘形态
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct GrantRecord {
    /// `sha256(8 位十进制码)` 的十六进制
    digest: String,
    /// 生成时刻（Unix 秒）
    issued_at: u64,
    /// 过期时刻（Unix 秒）
    expires_at: u64,
    /// 已累计的失败次数
    attempts: u32,
    /// 冷却截止时刻（Unix 秒）；`0` 表示不在冷却
    blocked_until: u64,
    /// 生成者线索（`<uid>/<pid>`），只用于审计
    issued_by: String,
}

impl GrantRecord {
    /// 由判定状态 + TTL 组装
    fn from_slot(slot: &PairingSlot, ttl_secs: u64, issued_by: &str) -> Self {
        Self {
            digest: slot.digest().to_owned(),
            issued_at: slot.issued_at(),
            expires_at: slot.issued_at().saturating_add(ttl_secs),
            attempts: slot.attempts(),
            blocked_until: slot.blocked_until(),
            issued_by: issued_by.to_owned(),
        }
    }

    /// 还原成判定状态
    fn slot(&self) -> PairingSlot {
        PairingSlot::restore(
            self.digest.clone(),
            self.issued_at,
            self.attempts,
            self.blocked_until,
        )
    }

    /// 落盘时的窗口：`ttl` 由 `expires-at - issued-at` 决定
    ///
    /// 记录里存的是绝对过期时刻而不是 TTL，读回时两边相减还原。这样即使本
    /// crate 的默认 TTL 将来变了，已发的码仍按发码时的约定过期。
    fn window(&self, fallback: PairingWindow) -> PairingWindow {
        PairingWindow {
            ttl_secs: self.expires_at.saturating_sub(self.issued_at),
            max_attempts: fallback.max_attempts,
        }
    }
}

/// 按需配对授权：`{data_dir}/remote/grant.toml`
///
/// 两端（发码的命令行与兑换的 agent）都从这里取路径，禁止各自拼路径。
#[derive(Debug, Clone)]
pub struct PairingGrant {
    path: PathBuf,
    lock_path: PathBuf,
    window: PairingWindow,
}

impl PairingGrant {
    /// 以 `{data_dir}/remote/` 为根构造（不创建文件）
    #[must_use]
    pub fn new(data_dir: &Path, window: PairingWindow) -> Self {
        let dir = data_dir.join("remote");
        Self {
            path: dir.join("grant.toml"),
            lock_path: dir.join("grant.lock"),
            window,
        }
    }

    /// 授权文件路径
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 窗口参数（`max_attempts`；`ttl_secs` 由每次发码的 `--ttl` 覆盖）
    #[must_use]
    pub fn window(&self) -> PairingWindow {
        self.window
    }

    /// 生成并落盘一枚新码；旧码立即作废
    ///
    /// 返回值是**唯一一次**能看到明文码的时机，由命令行打印给用户。
    ///
    /// # Errors
    ///
    /// TTL 非法、随机源不可用、锁被占用或写盘失败时返回 [`GrantError`]
    pub fn issue(&self, now: u64, ttl_secs: u64) -> Result<PairingCode, GrantError> {
        if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&ttl_secs) {
            return Err(GrantError::InvalidTtl(ttl_secs));
        }
        let code = PairingCode::generate()?;
        let record = GrantRecord::from_slot(&PairingSlot::new(&code, now), ttl_secs, &issued_by());
        let _guard = self.lock()?;
        self.store(Some(&record))?;
        Ok(code)
    }

    /// 当前是否有可用码（供 `serve status` 与 agent 自检）
    ///
    /// 只回答「有没有、还剩多久」，**不返回码**：码一旦离开发码命令就不该再
    /// 被读到。
    ///
    /// # Errors
    ///
    /// 文件读取失败或权限过宽时返回 [`GrantError`]
    pub fn status(&self, now: u64) -> Result<Option<GrantStatus>, GrantError> {
        let Some(record) = self.load()? else {
            return Ok(None);
        };
        let window = record.window(self.window);
        let slot = record.slot();
        let Some(remaining_secs) = slot.remaining(window, now) else {
            return Ok(None);
        };
        Ok(Some(GrantStatus {
            remaining_secs,
            cooling_down: slot.cooling_down(now),
        }))
    }

    /// 兑换：成功即销毁；失败计数，达上限后进入冷却（不作废码）
    ///
    /// 判定委托给 [`PairingSlot::attempt`]，与内存态逐条同源。
    ///
    /// # Errors
    ///
    /// 没有可用码 / 形态非法 / 码不对 / 冷却中 / 读写失败时返回错误
    pub fn redeem(&self, presented: &str, now: u64) -> Result<(), GrantError> {
        let _guard = self.lock()?;
        let Some(record) = self.load()? else {
            return Err(PairingCodeError::Unusable.into());
        };
        let window = record.window(self.window);
        let mut slot = record.slot();
        match slot.attempt(presented, window, now) {
            Redeem::Matched => {
                // 单次使用：成功即销毁，不给重放留窗口
                self.store(None)?;
                Ok(())
            }
            Redeem::Mismatch(attempts) => {
                slot.record_guess(attempts);
                self.store(Some(&GrantRecord::from_slot(
                    &slot,
                    window.ttl_secs,
                    &record.issued_by,
                )))?;
                Err(PairingCodeError::Mismatch.into())
            }
            Redeem::Exhausted => {
                slot.enter_cooldown(now);
                self.store(Some(&GrantRecord::from_slot(
                    &slot,
                    window.ttl_secs,
                    &record.issued_by,
                )))?;
                Err(PairingCodeError::Unusable.into())
            }
            Redeem::Expired => {
                // 过期码直接清掉：留着只会让下一次 `status` 多一次歧义
                self.store(None)?;
                Err(PairingCodeError::Unusable.into())
            }
            Redeem::CoolingDown => Err(PairingCodeError::Unusable.into()),
            Redeem::Malformed => Err(PairingCodeError::Malformed.into()),
        }
    }

    /// 显式作废当前码（`remote code --clear`）
    ///
    /// 本来就没有码视为成功（幂等）。
    ///
    /// # Errors
    ///
    /// 锁被占用或删除失败时返回 [`GrantError`]
    pub fn clear(&self) -> Result<(), GrantError> {
        let _guard = self.lock()?;
        self.store(None)
    }

    /// 读回落盘记录；文件不存在时为 `None`
    fn load(&self) -> Result<Option<GrantRecord>, GrantError> {
        let Some(text) = read_private(&self.path)? else {
            return Ok(None);
        };
        // 损坏按「没有码」处理（fail closed）：坏文件不该变成绕过配对的路径
        match toml::from_str::<GrantRecord>(&text) {
            Ok(record) => Ok(Some(record)),
            Err(_err) => {
                // 本层不引 `tracing`（auth 只做判定，日志由调用方决定记不记）
                Ok(None)
            }
        }
    }

    /// 原子写入；`None` 表示删除
    fn store(&self, record: Option<&GrantRecord>) -> Result<(), GrantError> {
        let Some(record) = record else {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(GrantError::Store(CredentialError::Io {
                    path: self.path.clone(),
                    source: err,
                })),
            };
        };
        let text = toml::to_string_pretty(record).map_err(|err| {
            GrantError::Store(CredentialError::Corrupt {
                path: self.path.clone(),
                reason: err.to_string(),
            })
        })?;
        write_private(&self.path, &text)?;
        Ok(())
    }

    /// 取跨进程锁
    ///
    /// 锁文件永不删除：删掉会让等待者在新旧 inode 上各持一把锁。内容只是排障
    /// 线索，不参与互斥判定。
    fn lock(&self) -> Result<GrantGuard, GrantError> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GrantError::LockIo {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|source| GrantError::LockIo {
                path: self.lock_path.clone(),
                source,
            })?;
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        let mut delay = std::time::Duration::from_millis(2);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    let _ = file.set_len(0);
                    let _ = writeln!(&file, "{}", std::process::id());
                    return Ok(GrantGuard { _file: file });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(GrantError::LockBusy(format!(
                            "waited {LOCK_TIMEOUT:?} for {}",
                            self.lock_path.display()
                        )));
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(GrantError::LockIo {
                        path: self.lock_path.clone(),
                        source,
                    });
                }
            }
        }
    }
}

/// 锁句柄：关闭（含进程崩溃）即释放
struct GrantGuard {
    /// 只用于保活
    _file: std::fs::File,
}

/// 发码者线索（`<uid>/<pid>`），只进审计字段
///
/// 只用于排障与审计，不参与任何判定，因此拿不到时写成 `-` 而不报错。
fn issued_by() -> String {
    format!("{}/{}", process_owner(), std::process::id())
}

/// 进程属主（Unix 取 uid，其余平台取用户名）
fn process_owner() -> String {
    #[cfg(unix)]
    {
        // 不引 libc：`id -u` 是 POSIX 必备工具，读失败就当未知
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map_or_else(|| "-".to_owned(), |text| text.trim().to_owned())
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "-".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000;

    fn grant(dir: &Path) -> PairingGrant {
        PairingGrant::new(dir, PairingWindow::default())
    }

    fn temp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    /// 没有发码就是没有码：启动即无码是本次设计的核心目的
    #[test]
    fn nothing_is_granted_until_asked() {
        let dir = temp();
        let grant = grant(dir.path());
        assert_eq!(grant.status(T0).expect("status"), None);
        assert!(
            matches!(
                grant.redeem("0000-0000", T0),
                Err(GrantError::Random(PairingCodeError::Unusable))
            ),
            "无码时兑换必须失败"
        );
    }

    #[test]
    fn issued_code_is_redeemable_once() {
        let dir = temp();
        let grant = grant(dir.path());
        let code = grant.issue(T0, DEFAULT_ON_DEMAND_TTL_SECS).expect("issue");
        assert_eq!(
            grant.status(T0).expect("status"),
            Some(GrantStatus {
                remaining_secs: DEFAULT_ON_DEMAND_TTL_SECS,
                cooling_down: false
            })
        );
        grant.redeem(&code.display_form(), T0).expect("redeem");
        assert_eq!(grant.status(T0).expect("status"), None, "成功即销毁");
        assert!(
            grant.redeem(&code.display_form(), T0).is_err(),
            "同一枚码不得重复兑换"
        );
    }

    /// 文件里不得出现明文码：泄漏文件不等于泄漏访问权
    #[test]
    fn file_holds_digest_not_code() {
        let dir = temp();
        let grant = grant(dir.path());
        let code = grant.issue(T0, DEFAULT_ON_DEMAND_TTL_SECS).expect("issue");
        let text = std::fs::read_to_string(grant.path()).expect("read");
        assert!(!text.contains(code.digits()), "授权文件不得含明文码");
        assert!(text.contains(&crate::token::sha256_hex(code.digits().as_bytes())));
    }

    #[test]
    fn expired_code_is_unusable_and_cleaned() {
        let dir = temp();
        let grant = grant(dir.path());
        let code = grant.issue(T0, MIN_TTL_SECS).expect("issue");
        let after = T0 + MIN_TTL_SECS;
        assert_eq!(grant.status(after).expect("status"), None);
        assert!(grant.redeem(&code.display_form(), after).is_err());
        assert!(!grant.path().exists(), "过期码应被清掉");
    }

    /// 时钟回拨不能让码活得比预期久
    #[test]
    fn clock_rollback_counts_as_expired() {
        let dir = temp();
        let grant = grant(dir.path());
        let code = grant.issue(T0, DEFAULT_ON_DEMAND_TTL_SECS).expect("issue");
        assert!(grant.redeem(&code.display_form(), T0 - 1).is_err());
        assert_eq!(grant.status(T0 - 1).expect("status"), None);
    }

    /// 猜错达上限后进入冷却，且冷却期间正确码也拒；冷却结束后仍可兑换
    #[test]
    fn repeated_guesses_cool_down_without_burning_the_code() {
        let dir = temp();
        let grant = grant(dir.path());
        let code = grant.issue(T0, MAX_TTL_SECS).expect("issue");
        // 第 `max_attempts` 次猜错即转入冷却，所以前 `max-1` 次报 mismatch
        for _ in 0..crate::code::DEFAULT_MAX_ATTEMPTS - 1 {
            assert!(
                matches!(
                    grant.redeem("0000-0000", T0),
                    Err(GrantError::Random(PairingCodeError::Mismatch))
                ),
                "未达上限时应报 mismatch"
            );
        }
        assert!(
            matches!(
                grant.redeem("0000-0000", T0),
                Err(GrantError::Random(PairingCodeError::Unusable))
            ),
            "达上限后应进入冷却"
        );
        assert_eq!(
            grant.status(T0).expect("status").map(|s| s.cooling_down),
            Some(true)
        );
        let after = T0 + crate::code::ATTEMPT_COOLDOWN_SECS;
        grant.redeem(&code.display_form(), after).expect("redeem");
    }

    #[test]
    fn reissuing_invalidates_the_previous_code() {
        let dir = temp();
        let grant = grant(dir.path());
        let first = grant.issue(T0, DEFAULT_ON_DEMAND_TTL_SECS).expect("issue");
        let second = grant
            .issue(T0, DEFAULT_ON_DEMAND_TTL_SECS)
            .expect("reissue");
        assert_ne!(first, second);
        assert!(
            grant.redeem(&first.display_form(), T0).is_err(),
            "旧码重发后立即失效"
        );
        grant.redeem(&second.display_form(), T0).expect("新码可用");
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = temp();
        let grant = grant(dir.path());
        grant.clear().expect("clear empty");
        let code = grant.issue(T0, DEFAULT_ON_DEMAND_TTL_SECS).expect("issue");
        grant.clear().expect("clear");
        assert_eq!(grant.status(T0).expect("status"), None);
        assert!(grant.redeem(&code.display_form(), T0).is_err());
    }

    #[test]
    fn ttl_is_bounded() {
        let dir = temp();
        let grant = grant(dir.path());
        assert!(matches!(
            grant.issue(T0, MIN_TTL_SECS - 1),
            Err(GrantError::InvalidTtl(_))
        ));
        assert!(matches!(
            grant.issue(T0, MAX_TTL_SECS + 1),
            Err(GrantError::InvalidTtl(_))
        ));
        grant.issue(T0, MIN_TTL_SECS).expect("lower bound ok");
        grant.issue(T0, MAX_TTL_SECS).expect("upper bound ok");
    }

    /// 过期时刻按发码时的 TTL 记，不随默认值漂移
    #[test]
    fn expiry_follows_issue_time_ttl() {
        let dir = temp();
        let grant = grant(dir.path());
        grant.issue(T0, MIN_TTL_SECS).expect("issue");
        let status = grant.status(T0).expect("status").expect("live");
        assert_eq!(status.remaining_secs, MIN_TTL_SECS);
    }

    /// 坏文件按无码处理：损坏不得成为绕过配对的路径
    #[test]
    fn corrupt_file_fails_closed() {
        let dir = temp();
        let grant = grant(dir.path());
        std::fs::create_dir_all(dir.path().join("remote")).expect("mkdir");
        // 内容坏掉以外还要权限正确：权限宽的会被上层直接拒绝读
        write_private(grant.path(), "not a grant").expect("write");
        assert_eq!(grant.status(T0).expect("status"), None);
        assert!(grant.redeem("1234-5678", T0).is_err());
    }

    /// 并发发码：结果必须只剩一枚有效码
    #[test]
    fn concurrent_issues_leave_exactly_one_code() {
        let dir = temp();
        let grant = grant(dir.path());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let grant = grant.clone();
                std::thread::spawn(move || grant.issue(T0, DEFAULT_ON_DEMAND_TTL_SECS))
            })
            .collect();
        let codes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("join"))
            .collect::<Result<Vec<_>, _>>()
            .expect("issue");
        let live: Vec<_> = codes
            .iter()
            .filter(|code| grant.redeem(&code.display_form(), T0).is_ok())
            .collect();
        assert_eq!(live.len(), 1, "并发发码只应留一枚可用");
    }

    /// 形态非法的输入不计数：手抖不该把窗口锁死
    #[test]
    fn malformed_input_does_not_count_as_a_guess() {
        let dir = temp();
        let grant = grant(dir.path());
        let code = grant.issue(T0, MAX_TTL_SECS).expect("issue");
        for _ in 0..crate::code::DEFAULT_MAX_ATTEMPTS {
            assert!(
                matches!(
                    grant.redeem("not-a-code", T0),
                    Err(GrantError::Random(PairingCodeError::Malformed))
                ),
                "形态非法应报 malformed"
            );
        }
        assert_eq!(
            grant.status(T0).expect("status").map(|s| s.cooling_down),
            Some(false),
            "形态非法不得推进冷却"
        );
        grant
            .redeem(&code.display_form(), T0)
            .expect("still redeemable");
    }
}
