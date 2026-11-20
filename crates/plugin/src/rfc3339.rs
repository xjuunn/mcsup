//! RFC3339 UTC 时间戳
//!
//! 只做一件事：把 [`SystemTime`] 渲染成 `YYYY-MM-DDTHH:MM:SSZ`。事件信封的
//! `at` 用这个格式；引 `chrono` 只为一个格式化不值得。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前时刻
#[must_use]
pub fn now() -> String {
    format(SystemTime::now())
}

/// 格式化一个时刻
#[must_use]
pub fn format(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = rest / 3_600;
    let minute = rest % 3_600 / 60;
    let second = rest % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 1970-01-01 起的天数 → 公历日期（Howard Hinnant 的 `civil_from_days`）
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_epoch_and_known_instants() {
        assert_eq!(format(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(format(leap), "2000-02-29T00:00:00Z");
        let ts = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(format(ts), "2027-01-15T08:00:00Z");
    }

    #[test]
    fn now_has_wire_shape() {
        let text = now();
        assert_eq!(text.len(), 20, "{text}");
        assert!(text.ends_with('Z'));
        assert_eq!(text.as_bytes()[10], b'T');
    }
}
