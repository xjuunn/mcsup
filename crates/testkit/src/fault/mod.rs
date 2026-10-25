//! 故障注入与契约夹具
//!
//! 三块互相独立的接缝：
//!
//! - [`supervisor`]：可控的 `SupervisorPort`，记录发出的每一行控制台命令
//! - [`download`]：按调用序号注入失败的 `ArtifactDownloader`
//! - [`store`]：按调用序号注入失败的 `ContentStore`
//!
//! 失败注入只做一件事：**第 N 次调用失败**。这种「单点故障」足以打穿
//! 所有错误分支，又不会让测试变成对随机行为的猜测。

pub mod builders;
pub mod cas_contract;
pub mod download;
pub mod store;
pub mod supervisor;
