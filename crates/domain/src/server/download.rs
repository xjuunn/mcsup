use crate::server::error::InvalidTransition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    Pending,
    Downloading,
    Verifying,
    Ready,
    Failed,
}

impl DownloadStatus {
    /// 判定一次状态转换是否合法
    ///
    /// # Errors
    ///
    /// 当 `next` 不是 `self` 的合法后继状态时返回 [`InvalidTransition`],
    /// 例如跳过 `Verifying` 直接从 `Downloading` 到 `Ready`。
    pub fn transition(self, next: Self) -> Result<Self, InvalidTransition> {
        use DownloadStatus::{Downloading, Failed, Pending, Ready, Verifying};
        let allowed = matches!(
            (self, next),
            (Pending, Downloading)
                | (Downloading, Verifying | Failed)
                | (Verifying, Ready | Failed)
        );
        if allowed {
            Ok(next)
        } else {
            Err(InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DownloadStatus::{Downloading, Failed, Pending, Ready, Verifying};

    #[test]
    fn happy_path_reaches_read() {
        assert_eq!(Pending.transition(Downloading), Ok(Downloading));
        assert_eq!(Downloading.transition(Verifying), Ok(Verifying));
        assert_eq!(Verifying.transition(Ready), Ok(Ready));
    }

    #[test]
    fn failure_is_reachable_from_both_stages() {
        assert_eq!(Downloading.transition(Failed), Ok(Failed));
        assert_eq!(Verifying.transition(Failed), Ok(Failed));
    }

    #[test]
    fn cannot_skip_verification() {
        assert!(Downloading.transition(Ready).is_err());
    }

    #[test]
    fn terminal_and_backward_transitions_are_rejected() {
        assert!(Ready.transition(Downloading).is_err());
        assert!(Failed.transition(Pending).is_err());
        assert!(Pending.transition(Pending).is_err());
    }
}
