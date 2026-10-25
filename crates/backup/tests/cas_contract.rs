//! `CasStore` 必须满足 `ContentStore` 契约
//!
//! 契约本体在 `testkit::cas_contract`：那份断言不随实现走。这里只负责
//! 「把真实实现装到契约上」，一行装配、零断言——一旦有人为了让某个实现
//! 通过而改契约，别的实现会立刻红，语义漂移无处隐藏。

use backup::CasStore;
use std::sync::Arc;
use testkit::cas_contract;

/// 文件系统 CAS 满足全部契约条目
#[test]
fn cas_store_satisfies_content_store_contract() {
    cas_contract::run(|root| Arc::new(CasStore::new(root)));
}
