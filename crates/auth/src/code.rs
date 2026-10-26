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

    /// 展示形态 `XXXX-XXXX`（给用户读）
    #[must_use]
    pub fn display_form(&self) -> String {
        format!("{}-{}", &self.0[..4], &self.0[4..])
    }

    /// 比较：常量时间，避免按字节提前返回泄漏前缀
    fn matches(&self, other: &Self) -> bool {
        crate::token::ct_eq(self.0.as_bytes(), other.0.as_bytes())
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

/// 当前有效的配对码及其剩余额度
///
/// 三态收敛在 [`Self::redeem`] 一处：过期、超次数、已用掉都返回同一个
/// [`PairingCodeError::Unusable`]——对猜码的人不该区分「过期了」还是
/// 「猜错太多次」，那只会告诉他离成功有多近。
#[derive(Debug, Clone)]
pub struct PairingCodeStore {
    window: PairingWindow,
    active: Option<ActiveCode>,
}

#[derive(Debug, Clone)]
struct ActiveCode {
    code: PairingCode,
    issued_at: u64,
    attempts: u32,
}

impl PairingCodeStore {
    /// 以给定窗口构造（尚无有效码）
    #[must_use]
    pub fn new(window: PairingWindow) -> Self {
        Self {
            window,
            active: None,
        }
    }

    /// 生成一枚新码并成为当前有效码（旧码立即作废）
    ///
    /// # Errors
    ///
    /// 系统随机源不可用时返回 [`PairingCodeError::Random`]
    pub fn issue(&mut self, now: u64) -> Result<PairingCode, PairingCodeError> {
        let code = PairingCode::generate()?;
        self.active = Some(ActiveCode {
            code: code.clone(),
            issued_at: now,
            attempts: 0,
        });
        Ok(code)
    }

    /// 当前有效码（供 serve 端展示；无有效码时为 `None`）
    #[must_use]
    pub fn current(&self, now: u64) -> Option<&PairingCode> {
        self.active
            .as_ref()
            .filter(|active| self.is_live(active, now))
            .map(|active| &active.code)
    }

    /// 兑换：成功即作废，失败计数并在达上限时作废
    ///
    /// # Errors
    ///
    /// 没有有效码 / 输入形态非法 / 码不对时返回 [`PairingCodeError`]
    pub fn redeem(&mut self, presented: &str, now: u64) -> Result<(), PairingCodeError> {
        // 先判定存活再取可变借用：过期与锁死都归到同一个 Unusable
        match self.active.as_ref() {
            Some(active) if self.is_live(active, now) => {}
            Some(_) => {
                self.active = None;
                return Err(PairingCodeError::Unusable);
            }
            None => return Err(PairingCodeError::Unusable),
        }
        let active = self.active.as_mut().ok_or(PairingCodeError::Unusable)?;

        let presented = PairingCode::parse(presented)?;
        if presented.matches(&active.code) {
            // 单次使用：成功即销毁，不给重放留窗口
            self.active = None;
            return Ok(());
        }

        active.attempts += 1;
        if active.attempts >= self.window.max_attempts {
            self.active = None;
            return Err(PairingCodeError::Unusable);
        }
        Err(PairingCodeError::Mismatch)
    }

    /// 作废当前码（`serve --stop` / 重新 issue 前的显式清理）
    pub fn clear(&mut self) {
        self.active = None;
    }

    /// 当前码是否只是**自然过期**（不是用掉、也不是猜错锁死）
    ///
    /// 三种失效原因对外的错误相同（都报 [`PairingCodeError::Unusable`]，避免
    /// 告诉猜码的人离成功有多近），但对内必须分开：过期可以轮换出新码继续
    /// 服务，而「已用掉」与「锁死」必须保持失效，否则限速就形同虚设。
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| !self.is_live(active, now))
    }

    /// 过期则轮换：发一枚新码并返回，未过期返回 `None`
    ///
    /// 只在自然过期时轮换（见 [`Self::expired`]）。常驻 agent 靠它维持
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

    /// 窗口参数
    #[must_use]
    pub fn window(&self) -> PairingWindow {
        self.window
    }

    /// 是否仍在有效期内
    fn is_live(&self, active: &ActiveCode, now: u64) -> bool {
        // 时钟回拨按「已过期」处理：宁可让用户重开一次，也不让码活得比预期久
        now.checked_sub(active.issued_at)
            .is_some_and(|elapsed| elapsed < self.window.ttl_secs)
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
    fn wrong_attempts_are_limited_and_lock_the_code() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        let wrong = "00000000";
        // 前 max_attempts-1 次报 Mismatch，第 max_attempts 次直接锁死
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
            "达上限后作废"
        );
        // 锁死之后即使给出正确的码也不再接受
        assert_eq!(
            store.redeem(&code.display_form(), T0),
            Err(PairingCodeError::Unusable)
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

    /// 只有自然过期才算「可轮换」：用掉与锁死都必须保持关闭
    #[test]
    fn only_natural_expiry_counts_as_expired() {
        let mut store = store();
        let code = store.issue(T0).expect("issue");
        assert!(!store.expired(T0), "有效期内不算过期");

        store.redeem(&code.display_form(), T0).expect("兑换");
        assert!(!store.expired(T0 + DEFAULT_TTL_SECS), "用掉不是过期");

        let mut locked = store_with_attempts();
        for _ in 0..DEFAULT_MAX_ATTEMPTS {
            let _ = locked.redeem("00000000", T0);
        }
        assert!(!locked.expired(T0 + DEFAULT_TTL_SECS), "锁死不是过期");
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
