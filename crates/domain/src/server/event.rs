use crate::server::version::VersionId;

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainEvent {
    // 下载完成并校验通过
    JarDownloadCompleted { version: VersionId },
    // 下载或校验失败
    JarDownloadFailed { version: VersionId, reason: String },
}
