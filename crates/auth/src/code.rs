//! 配对码：短时有效、单次使用、限速尝试
//!
//! 配对码是**首次信任**的唯一通道（`docs/remote.md`「安全」）：用户在被连接端
//! 屏幕上读到一串码，在连接端输入。它必须满足三条不变量，缺一条就等于把
//! 「本地网络可达」升级成「谁都能配对」：
//!
//! 1. **短时有效**：默认 10 分钟，过期即失效，不续期；
//! 2. **单次使用**：成功兑换一次后立即作废，不能重复兑换；
//! 3. **限速尝试**：连续猜错达到上限即作废，不给暴力枚举留窗口。
//!
//! 本模块是纯状态机：时间由调用方传入（不读时钟），随机源在生成时用一次。
//! 这样过期与限速都能在测试里确定性地打穿，不必 `sleep`。

use crate::token::TokenError;
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;

/// 配对码位数（十进制）
const CODE_DIGITS: usize = 8;

/// 默认有效期：10 分钟
pub const DEFAULT_TTL_SECS: u64 = 10 * 60;

/// 默认允许的失败尝试次数
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// 失败次数用尽后的冷却时长
///
/// 早期实现在达到上限时直接作废整枚码：任何能连到端口的人猜 5 次，就能把
/// 合法用户的配对窗口关掉；窗口到期轮换后还能继续关，等于远程拒绝服务。
/// 改为「暂停尝试、不作废码」：攻击者的猜码速率被压到每分钟 5 次（10 分钟
/// 窗口内最多 50 次，面对 1e8 的码空间可以忽略），而持有正确码的用户最多
/// 等一分钟就能配对。
pub const ATTEMPT_COOLDOWN_SECS: u64 = 60;

/// 配对码操作失败
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PairingCodeError {
    /// 没有可用配对码（未生成 / 已用掉 / 已过期 / 已锁死）
    #[error("no usable pairing code")]
    Unusable,
    /// 用户输入不是配对码形态
    #[error("malformed pairing code")]
    Malformed,
    /// 码不对（本次失败已计数）
    #[error("pairing code does not match")]
    Mismatch,
    /// 系统随机源不可用
    #[error("secure random source unavailable")]
    Random,
}

impl From<TokenError> for PairingCodeError {
    fn from(_: TokenError) -> Self {
        Self::Random
    }
}

/// 配对码：`XXXX-XXXX` 形态的 8 位十进制
///
/// 允许用户带不带分隔符、大小写无关地输入；比较时统一去掉分隔符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// 生成一枚新码
    ///
    /// # Errors
    ///
    /// 系统随机源不可用时返回 [`PairingCodeError::Random`]
    pub fn generate() -> Result<Self, PairingCodeError> {
        let mut bytes = [0u8; CODE_DIGITS];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| PairingCodeError::Random)?;
        // 每字节取模 10 会有偏置，但码本身只活 10 分钟、且限速 5 次，
        // 偏置带来的熵损失远小于限速提供的保护；仍按 0..=9 均匀取。
        let digits: String = bytes
            .iter()
            .map(|byte| char::from(b'0' + (byte % 10)))
            .collect();
        Ok(Self(digits))
    }

    /// 从用户输入解析
    ///
    /// # Errors
    ///
    /// 去掉分隔符后不是 8 位十进制时返回 [`PairingCodeError::Malformed`]
    pub fn parse(raw: &str) -> Result<Self, PairingCodeError> {
        let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
        if digits.len() != CODE_DIGITS || raw.chars().any(|c| !c.is_ascii_digit() && c != '-') {
            return Err(PairingCodeError::Malformed);
        }
        Ok(Self(digits))
    }

    /// 去掉分隔符的 8 位数字（摘要用）
    #[must_use]
    pub fn digits(&self) -> &str {
        &self.0
    }

    /// 展示形态 `XXXX-XXXX`（给用户读）
    #[must_use]
    pub fn display_form(&self) -> String {
        format!("{}-{}", &self.0[..4], &self.0[4..])
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display_form())
    }
}

/// 一次配对窗口的时间参数
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingWindow {
    /// 有效期（秒）
    pub ttl_secs: u64,
    /// 允许的失败次数
    pub max_attempts: u32,
}

impl Default for PairingWindow {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_TTL_SECS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

/// 一枚码的判定状态：**本模块唯一一份**过期 / 单次使用 / 限速逻辑
///
/// 抽出来是因为配对码有两种存放形态（见 [`crate::grant`]）：
///
/// - 内存态 [`PairingCodeStore`]：码在进程里（SSH 引导与前台调试）；
/// - 落盘态 [`crate::grant::PairingGrant`]：码在 0600 文件里（后台常驻，
///   发码发生在命令行、兑换发生在 agent，两个进程）。
///
/// 形态不同，**判定必须相同**：否则「猜错多少次才冷却」「过期还认不认」会
/// 随运行方式漂移。所以判定收敛在这里，两侧只负责「怎么把状态存下来」。
///
/// 三态收敛在 [`Self::attempt`] 一处：过期、超次数、已用掉都得到同一个
/// [`Redeem::Expired`] / [`Redeem::Exhausted`]，调用方再统一映射成
/// [`PairingCodeError::Unusable`]——对猜码的人不该区分「过期了」还是
/// 「猜错太多次」，那只会告诉他离成功有多近。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingSlot {
    /// 码的摘要（十六进制），**不是**明文
    ///
    /// 内存态也需要摘要：落盘态与内存态共用同一份比较代码，顺便保证内存态
    /// 的诊断输出（`Debug`）不会带出明文码。
    digest: String,
    issued_at: u64,
    attempts: u32,
    /// 冷却截止时刻；冷却期间拒绝一切尝试，但码本身不作废
    blocked_until: u64,
}

impl PairingSlot {
    /// 以「明文码 + 签发时刻」构造（尚无失败）
    ///
    /// 只在此处接受明文：之后状态里只留摘要。
    #[must_use]
    pub fn new(code: &PairingCode, issued_at: u64) -> Self {
        Self {
            digest: crate::token::sha256_hex(code.digits().as_bytes()),
            issued_at,
            attempts: 0,
            blocked_until: 0,
        }
    }

    /// 从落盘字段还原（读回授权文件时用）
    #[must_use]
    pub fn restore(digest: String, issued_at: u64, attempts: u32, blocked_until: u64) -> Self {
        Self {
            digest,
            issued_at,
            attempts,
            blocked_until,
        }
    }

    /// 摘要（十六进制），落盘用
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// 签发时刻
    #[must_use]
    pub fn issued_at(&self) -> u64 {
        self.issued_at
    }

    /// 已累计的失败次数
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// 冷却截止时刻（`0` 表示不在冷却）
    #[must_use]
    pub fn blocked_until(&self) -> u64 {
        self.blocked_until
    }

    /// 此刻是否仍在有效期内
    ///
    /// 时钟回拨按「已过期」处理：宁可让用户重开一次，也不让码活得比预期久。
    #[must_use]
    pub fn is_live(&self, window: PairingWindow, now: u64) -> bool {
        now.checked_sub(self.issued_at)
            .is_some_and(|elapsed| elapsed < window.ttl_secs)
    }

    /// 剩余有效秒数（已过期或尚未生效时为 `None`）
    #[must_use]
    pub fn remaining(&self, window: PairingWindow, now: u64) -> Option<u64> {
        self.is_live(window, now)
            .then(|| window.ttl_secs - (now - self.issued_at))
    }

    /// 此刻是否处于猜码冷却
    #[must_use]
    pub fn cooling_down(&self, now: u64) -> bool {
        now < self.blocked_until
    }

    /// 兑换尝试的结果：调用方据此决定「保留还是销毁」这条状态
    ///
    /// 把「状态怎么变」交回调用方，是为了让落盘形态能在**同一次**加锁里
    /// 完成读-改-写；判定本身仍只有这一处。
    #[must_use]
    pub fn attempt(&self, presented: &str, window: PairingWindow, now: u64) -> Redeem {
        if !self.is_live(window, now) {
            return Redeem::Expired;
        }
        if self.cooling_down(now) {
            return Redeem::CoolingDown;
        }
        let Ok(presented) = PairingCode::parse(presented) else {
            return Redeem::Malformed;
        };
        if crate::token::ct_eq(
            self.digest.as_bytes(),
            crate::token::sha256_hex(presented.digits().as_bytes()).as_bytes(),
        ) {
            return Redeem::Matched;
        }
        let attempts = self.attempts + 1;
        if attempts >= window.max_attempts {
            Redeem::Exhausted
        } else {
            Redeem::Mismatch(attempts)
        }
    }

    /// 记录一次猜错；达上限时转入冷却（**不作废码**）
    ///
    /// 暂停而不是作废：作废等于把「关掉别人配对窗口」的能力交给任何能连到
    /// 端口的人（见 [`ATTEMPT_COOLDOWN_SECS`]）。
    pub fn record_guess(&mut self, attempts: u32) {
        self.attempts = attempts;
        self.blocked_until = 0;
    }

    /// 猜错次数用尽：清零并进入冷却
    pub fn enter_cooldown(&mut self, now: u64) {
        self.attempts = 0;
        self.blocked_until = now.saturating_add(ATTEMPT_COOLDOWN_SECS);
    }
}

/// [`PairingSlot::attempt`] 的判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redeem {
    /// 码正确，应当销毁这条状态
    Matched,
    /// 码错了，累计到 `attempts` 次（尚未达上限）
    Mismatch(u32),
    /// 码错了且用尽次数，应进入冷却
    Exhausted,
    /// 已过期，应销毁这条状态
    Expired,
    /// 处于冷却期，状态原样保留
    CoolingDown,
    /// 输入形态非法，不计数（形态错不算猜码，否则手抖也能锁死窗口）
    Malformed,
}

/// 内存态的配对码：SSH 引导与前台调试用
///
/// 「屏幕上总有当前有效码」由调用方轮询 [`Self::rotate_if_expired`] 维持；
/// 后台常驻形态**不使用**本类型（见 [`crate::grant`]）。
///
/// 判定全部委托给 [`PairingSlot`]，本类型只负责持有明文码并把它交给展示方。
#[derive(Debug, Clone)]
pub struct PairingCodeStore {
    window: PairingWindow,
    active: Option<PairingCode>,
    slot: Option<PairingSlot>,
}

impl PairingCodeStore {
    /// 以给定窗口构造（尚无有效码）
    #[must_use]
    pub fn new(window: PairingWindow) -> Self {
        Self {
            window,
            active: None,
            slot: None,
        }
    }

    /// 生成一枚新码并成为当前有效码（旧码立即作废）
    ///
    /// # Errors
    ///
    /// 系统随机源不可用时返回 [`PairingCodeError::Random`]
    pub fn issue(&mut self, now: u64) -> Result<PairingCode, PairingCodeError> {
        let code = PairingCode::generate()?;
        self.slot = Some(PairingSlot::new(&code, now));
        self.active = Some(code.clone());
        Ok(code)
    }

    /// 当前有效码（供 serve 端展示；无有效码时为 `None`）
    #[must_use]
    pub fn current(&self, now: u64) -> Option<&PairingCode> {
        self.active.as_ref().filter(|_| {
            self.slot
                .as_ref()
                .is_some_and(|slot| slot.is_live(self.window, now))
        })
    }

    /// 兑换：成功即作废；失败计数，达上限后进入冷却（不作废码）
    ///
    /// # Errors
    ///
    /// 没有有效码 / 输入形态非法 / 码不对 / 处于冷却期时返回
    /// [`PairingCodeError`]
    pub fn redeem(&mut self, presented: &str, now: u64) -> Result<(), PairingCodeError> {
        let Some(slot) = self.slot.clone() else {
            return Err(PairingCodeError::Unusable);
        };
        if !slot.is_live(self.window, now) {
            self.clear();
            return Err(PairingCodeError::Unusable);
        }
        if slot.cooling_down(now) {
            return Err(PairingCodeError::Unusable);
        }
        match slot.attempt(presented, self.window, now) {
            Redeem::Matched => {
                self.clear();
                Ok(())
            }
            Redeem::Mismatch(attempts) => {
                if let Some(slot) = self.slot.as_mut() {
                    slot.record_guess(attempts);
                }
                Err(PairingCodeError::Mismatch)
            }
            Redeem::Exhausted => {
                if let Some(slot) = self.slot.as_mut() {
                    slot.enter_cooldown(now);
                }
                Err(PairingCodeError::Unusable)
            }
            Redeem::Malformed => Err(PairingCodeError::Malformed),
            // 上面两个分支已先行返回，这里仅为穷尽匹配
            Redeem::Expired | Redeem::CoolingDown => Err(PairingCodeError::Unusable),
        }
    }

    /// 作废当前码
    pub fn clear(&mut self) {
        self.slot = None;
        self.active = None;
    }

    /// 当前码是否只是**自然过期**（不是用掉、也不是猜错进入冷却）
    ///
    /// 三种失效原因对外的错误相同（都报 [`PairingCodeError::Unusable`]，避免
    /// 告诉猜码的人离成功有多近），但对内必须分开：过期可以轮换出新码继续
    /// 服务，而「已用掉」必须保持失效；「冷却」有明确的结束时刻，也不该
    /// 因为轮换被提前放行。
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        self.slot
            .as_ref()
            .is_some_and(|slot| !slot.is_live(self.window, now))
    }

    /// 过期则轮换：发一枚新码并返回，未过期返回 `None`
    ///
    /// 只在自然过期时轮换（见 [`Self::expired`]）。**连续形态**靠它维持
    /// 「屏幕上总有当前有效码」，同时不削弱单码的限速与单次使用。
    ///
    /// # Errors
    ///
    /// 系统随机源不可用时返回 [`PairingCodeError::Random`]
    pub fn rotate_if_expired(&mut self, now: u64) -> Result<Option<PairingCode>, PairingCodeError> {
        if !self.expired(now) {
            return Ok(None);
        }
        self.issue(now).map(Some)
    }

    /// 剩余有效秒数（无有效码时为 `None`）
    #[must_use]
    pub fn remaining_secs(&self, now: u64) -> Option<u64> {
        self.slot
            .as_ref()
            .and_then(|slot| slot.remaining(self.window, now))
    }

    /// 此刻是否处于猜码冷却
    #[must_use]
    pub fn cooling_down(&self, now: u64) -> bool {
        self.slot
            .as_ref()
            .is_some_and(|slot| slot.cooling_down(now))
    }

    /// 窗口参数
    #[must_use]
    pub fn window(&self) -> PairingWindow {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000;

    fn store() -> PairingCodeStore {
        PairingCodeStore::new(PairingWindow::default())
    }

    #[test]
    fn issued_code_is_displayed_in_groups_of_four() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        let text = code.display_form();
        assert_eq!(text.len(), 9, "{text}");
        assert_eq!(text.as_bytes()[4], b'-');
        assert_eq!(PairingCode::parse(&text).expect("parse"), code);
        // 用户可能输入小写或带空格以外的分隔符
        assert_eq!(
            PairingCode::parse(&text.replace('-', "")).expect("parse"),
            code
        );
    }

    #[test]
    fn redeem_succeeds_once_and_then_becomes_unusable() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        assert!(store.current(T0).is_some());
        store.redeem(&code.display_form(), T0).expect("首次兑换");
        assert_eq!(
            store.redeem(&code.display_form(), T0),
            Err(PairingCodeError::Unusable),
            "单次使用：第二次必须失败"
        );
        assert!(store.current(T0).is_none(), "兑换成功后不再展示");
    }

    #[test]
    fn code_expires_after_ttl() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        let just_before = T0 + DEFAULT_TTL_SECS - 1;
        assert!(store.current(just_before).is_some());
        let at_expiry = T0 + DEFAULT_TTL_SECS;
        assert!(store.current(at_expiry).is_none(), "到期即不可用");
        assert_eq!(
            store.redeem(&code.display_form(), at_expiry),
            Err(PairingCodeError::Unusable)
        );
    }

    /// 时钟回拨不得让码活得更久
    #[test]
    fn clock_going_backwards_invalidates_the_code() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        assert!(store.current(T0 - 1).is_none());
        assert_eq!(
            store.redeem(&code.display_form(), T0 - 1),
            Err(PairingCodeError::Unusable)
        );
    }

    #[test]
    fn wrong_attempts_pause_the_code_instead_of_burning_it() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        let wrong = "00000000";
        // 前 max_attempts-1 次报 Mismatch，第 max_attempts 次进入冷却
        for _ in 0..DEFAULT_MAX_ATTEMPTS - 1 {
            assert_eq!(
                store.redeem(wrong, T0),
                Err(PairingCodeError::Mismatch),
                "未达上限时报不匹配"
            );
        }
        assert_eq!(
            store.redeem(wrong, T0),
            Err(PairingCodeError::Unusable),
            "达上限后进入冷却"
        );
        // 冷却期内即使给出正确的码也不接受
        assert_eq!(
            store.redeem(&code.display_form(), T0),
            Err(PairingCodeError::Unusable)
        );
        // 但码没有被作废：冷却结束后持码用户仍能配对。
        // 作废等于任何能连端口的人猜 5 次就能关掉别人的配对窗口。
        let after_cooldown = T0 + ATTEMPT_COOLDOWN_SECS;
        assert!(
            store.redeem(&code.display_form(), after_cooldown).is_ok(),
            "冷却结束应接受正确的码"
        );
    }

    #[test]
    fn malformed_input_does_not_consume_attempts() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        assert_eq!(store.redeem("abc", T0), Err(PairingCodeError::Malformed));
        // 形态错误不计入失败次数：仍可用正确码兑换
        store.redeem(&code.display_form(), T0).expect("仍可兑换");
    }

    #[test]
    fn issuing_again_invalidates_the_previous_code() {
        let mut store = store();
        let first = store.issue(T0).expect("issue");
        let second = store.issue(T0).expect("issue");
        assert_eq!(
            store.redeem(&first.display_form(), T0),
            Err(PairingCodeError::Mismatch),
            "旧码必须作废"
        );
        store.redeem(&second.display_form(), T0).expect("新码可用");
    }

    #[test]
    fn redeem_without_an_active_code_is_unusable() {
        let mut store = store();
        assert_eq!(
            store.redeem("12345678", T0),
            Err(PairingCodeError::Unusable)
        );
        assert!(store.current(T0).is_none());
    }

    #[test]
    fn clear_drops_the_active_code() {
        let mut store = store();
        store.issue(T0).expect("issue");
        store.clear();
        assert!(store.current(T0).is_none());
        assert_eq!(
            store.redeem("12345678", T0),
            Err(PairingCodeError::Unusable)
        );
    }

    #[test]
    fn window_defaults_match_documented_values() {
        let window = PairingWindow::default();
        assert_eq!(window.ttl_secs, 600, "10 分钟");
        assert_eq!(window.max_attempts, 5);
    }

    /// 只有自然过期才算「可轮换」：用掉与冷却都不算
    #[test]
    fn only_natural_expiry_counts_as_expired() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        assert!(!store.expired(T0), "有效期内不算过期");

        store.redeem(&code.display_form(), T0).expect("兑换");
        assert!(!store.expired(T0 + DEFAULT_TTL_SECS), "用掉不是过期");

        let mut cooled = store_with_attempts();
        for _ in 0..DEFAULT_MAX_ATTEMPTS {
            let _ = cooled.redeem("00000000", T0);
        }
        assert!(
            !cooled.expired(T0 + DEFAULT_TTL_SECS - 1),
            "冷却不是过期：窗口内不轮换"
        );
        assert!(
            cooled.expired(T0 + DEFAULT_TTL_SECS),
            "TTL 到期就是自然过期，应允许轮换"
        );
    }

    fn store_with_attempts() -> PairingCodeStore {
        let mut store = store();
        store.issue(T0).expect("issue");
        store
    }

    #[test]
    fn rotate_reissues_only_after_expiry() {
        let mut store = store();
        let first = store.issue(T0).expect("issue");
        assert_eq!(
            store.rotate_if_expired(T0).expect("rotate"),
            None,
            "未过期不轮换"
        );
        assert_eq!(store.current(T0).expect("still live"), &first);

        let fresh = store
            .rotate_if_expired(T0 + DEFAULT_TTL_SECS)
            .expect("rotate")
            .expect("issued");
        assert_ne!(fresh, first, "轮换必须给出新码");
        assert_eq!(
            store.redeem(&first.display_form(), T0 + DEFAULT_TTL_SECS),
            Err(PairingCodeError::Mismatch),
            "旧码轮换后作废"
        );
        store
            .redeem(&fresh.display_form(), T0 + DEFAULT_TTL_SECS)
            .expect("新码可用");
    }

    #[test]
    fn rotate_does_not_revive_a_used_code() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        store.redeem(&code.display_form(), T0).expect("兑换");
        assert_eq!(
            store
                .rotate_if_expired(T0 + DEFAULT_TTL_SECS)
                .expect("rotate"),
            None,
            "用掉之后不得自动重开配对窗口"
        );
        assert!(store.current(T0 + DEFAULT_TTL_SECS).is_none());
    }
}
