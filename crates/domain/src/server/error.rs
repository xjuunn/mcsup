use super::download::DownloadStatus;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid download status transition: {from:?} -> {to:?}")]
pub struct InvalidTransition {
    pub from: DownloadStatus,
    pub to: DownloadStatus,
}
