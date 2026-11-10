//! 外部输入的时长上限
//!
//! 宽限、等待时长既来自 CLI 参数，也来自远端帧。远端客户端可以传
//! `u64::MAX`：`Duration::from_secs(u64::MAX)` 本身合法，但 `Instant + 它`
//! 会溢出 panic（release 下 `panic = "abort"`，整进程退出），或者让一个
//! 「格式正确」的请求把线程锁死几百年。这不是风格问题，是可用性边界。
//!
//! 上限放领域层而不是各入口：CLI、远端分发、supervisor 三个入口必须用同一
//! 份判定，各自 `min` 一遍迟早分叉。入口只负责把用户输入映射成用法错误。

use std::time::Duration;

/// 停机宽限上限：一小时
///
/// 再长的宽限没有运维意义：JVM 一分钟内没退出，等一小时也只是拖长停机窗口。
pub const MAX_STOP_GRACE: Duration = Duration::from_secs(60 * 60);

/// 日志长轮询单次等待上限：30 秒
///
/// 客户端按秒级轮询增量；单次等待过长只会让 Ctrl+C 变迟钝，并占用对端连接。
pub const MAX_LOG_WAIT: Duration = Duration::from_secs(30);

/// 备份前等待 `save-all` 落盘的上限：30 秒
pub const MAX_FLUSH_WAIT: Duration = Duration::from_secs(30);

/// 单次日志请求的行数上限
///
/// 远端帧里的 `lines` 是 `usize`，对端可以传 `usize::MAX`。日志扫描按行
/// 收集，行数不夹紧会让一个格式正确的请求把 agent 内存打满。
pub const MAX_LOG_LINES: usize = 10_000;

/// 日志尾部 / 单次增量读取的字节上限：8 MiB
///
/// 行数不是唯一维度：一行可以不换行地写到几 GB，只夹行数仍会把整个文件读进
/// 内存。超过上限时只保留末尾部分，日志是排障数据，截断优于拖垮进程。
pub const MAX_LOG_TAIL_BYTES: usize = 8 * 1024 * 1024;

/// 把外部给的日志行数夹到 [`MAX_LOG_LINES`]
#[must_use]
pub fn log_lines(lines: usize) -> usize {
    lines.min(MAX_LOG_LINES)
}

/// 把外部给的停机宽限秒数夹到 [`MAX_STOP_GRACE`]
#[must_use]
pub fn stop_grace(secs: u64) -> Duration {
    Duration::from_secs(secs).min(MAX_STOP_GRACE)
}

/// 把外部给的日志等待毫秒数夹到 [`MAX_LOG_WAIT`]
#[must_use]
pub fn log_wait(ms: u64) -> Duration {
    Duration::from_millis(ms).min(MAX_LOG_WAIT)
}

/// 把外部给的落盘等待毫秒数夹到 [`MAX_FLUSH_WAIT`]
#[must_use]
pub fn flush_wait(ms: u64) -> Duration {
    Duration::from_millis(ms).min(MAX_FLUSH_WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn stop_grace_is_capped_for_absurd_input() {
        assert_eq!(stop_grace(0), Duration::ZERO);
        assert_eq!(stop_grace(30), Duration::from_secs(30));
        assert_eq!(stop_grace(u64::MAX), MAX_STOP_GRACE);
        assert_eq!(stop_grace(MAX_STOP_GRACE.as_secs()), MAX_STOP_GRACE);
    }

    #[test]
    fn log_and_flush_waits_are_capped() {
        assert_eq!(log_wait(u64::MAX), MAX_LOG_WAIT);
        assert_eq!(log_wait(1_500), Duration::from_millis(1_500));
        assert_eq!(flush_wait(u64::MAX), MAX_FLUSH_WAIT);
        assert_eq!(flush_wait(0), Duration::ZERO);
    }

    /// 夹紧后的时长加在 `Instant` 上不会溢出
    ///
    /// 这正是未夹紧时最直接的后果：`u64::MAX` 秒加到当前时刻直接 panic。
    #[test]
    fn capped_waits_never_overflow_a_deadline() {
        let now = Instant::now();
        let _ = now + stop_grace(u64::MAX);
        let _ = now + log_wait(u64::MAX);
        let _ = now + flush_wait(u64::MAX);
    }
}
