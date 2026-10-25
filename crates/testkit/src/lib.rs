//! 测试基础设施：契约夹具、故障注入、假实现与断言
//!
//! 本 crate 只作 dev-dependency，不进 `mcsup` 门面、不发布。
//!
//! 分五块，对应五个测试接缝：
//!
//! | 模块 | 接缝 | 用途 |
//! | --- | --- | --- |
//! | [`cas_contract`] | `port::ContentStore` | 「任何 CAS 实现都必须满足」的契约 |
//! | [`fault`] | `SupervisorPort` / `ArtifactDownloader` / `ContentStore` | 故障注入 |
//! | [`fixture`] | 工作区 / 服务器目录 / 注册表记录 | 服务层装配样板 |
//! | [`http`] | HTTP 上游 | 回环假服务，替代真实网络 |
//! | [`mod@assert`] | 断言本身 | 失败信息自带「期望 / 实际 / 现场」 |
//!
//! 设计约束：本 crate 不知道 `service` / `runtime` / `storage` 的存在。
//! 它们是交付这些 trait 的对象，由各自的 `tests/` 决定怎么装配；`testkit`
//! 只提供「实现必须满足什么」和「失败时怎么把话说清楚」。
//!
//! 测试基础设施里的 panic 点全部是「测试环境本身坏了」（锁中毒、tempdir
//! 建不出来）或「断言失败」，不是业务错误；逐条写 `# Panics` 只是噪音，
//! 这里整 crate 豁免。

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    // 测试代码里 `mutex.lock().expect(..)` 的短链条远比拆成 let 绑定易读
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]

pub mod assert;
pub mod fault;
pub mod fixture;
pub mod http;

pub use assert::{Failure, assert_invariant, list_tree};
pub use fault::builders::*;
pub use fault::cas_contract::{self, InMemoryCas};
pub use fault::download::{DownloadCall, FailingDownloader, InjectFailure};
pub use fault::store::{CasOp, FailingContentStore};
pub use fault::supervisor::{CommandLog, FakeSupervisor};
pub use fixture::{
    MemServerRepo, ServerFixture, ServiceFixture, SingleWorkspaceRepo, assert_absent,
    assert_present, server_meta,
};
pub use http::{HttpServer, Response};
