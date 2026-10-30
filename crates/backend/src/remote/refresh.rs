//! 一次远端同步：把对端的四份列表拉进本地缓存
//!
//! 只在**后台刷新进程**（`mcsup __remote-refresh`）与 `remote connect` 的成功
//! 路径上调用，前台读命令一律只读缓存——这正是「回车立刻出结果」的来源。
//!
//! 一帧失败即整体失败：半新半旧的缓存比整份旧缓存更误导（服务器列表来自新
//! 一轮、工作区列表停在上一轮，会让「服务器挂在不存在的工作区下」这种假象
//! 出现）。失败时保留上一份完整快照，只更新 `last_error`。
//!
//! 拉四帧不是原子的，因此写回前要确认这期间没人写过缓存（`generation`）：
//! 前台的写命令会顺手写穿透，慢刷新若照旧整份覆盖，就会把刚删掉的条目复活。

use super::cache::{RemoteCache, Snapshot, now_secs};
use super::link::RemoteLink;
use port::RepositoryError;
use protocol::remote::wire::{WireJava, WireServer, WireStatusEntry, WireWorkspace};
use protocol::remote::{Request, Response};

/// 拉取四份列表并写入缓存；返回写入的快照
///
/// # Errors
///
/// 任一次往返失败，或对端回了非预期帧时返回 [`RepositoryError`]（缓存保持
/// 上一次的完整内容）
pub fn refresh(link: &RemoteLink, cache: &RemoteCache) -> Result<Snapshot, RepositoryError> {
    // 先取代次再拉数据：这之后任何一次写穿透都会让本轮结果作废
    let expected = cache.generation();
    let workspaces = fetch_list::<WireWorkspace>(link, &Request::WorkspaceList, "workspaces")?;
    let servers = fetch_list::<WireServer>(link, &Request::ServerList, "servers")?;
    let javas = fetch_list::<WireJava>(link, &Request::JavaList, "javas")?;
    let statuses = fetch_statuses(link, &servers)?;

    let snapshot = Snapshot {
        workspaces,
        servers,
        javas,
        statuses,
        at: now_secs(),
    };
    if cache.store_if_current(&snapshot, expected)? {
        return Ok(snapshot);
    }
    // 期间有人写过更新的一份：丢弃本轮结果，但仍算「连得上」——
    // 清掉失败标记，避免刚被写穿透证明可用的远端又被标成离线
    cache.mark_synced(snapshot.at)?;
    Ok(snapshot)
}

/// 只重拉工作区列表并写穿透（远端写命令成功后调用）
///
/// 写命令已经知道对端动过什么，但列表口径（创建时间、当前工作区…）只有对端
/// 说得准，因此重拉一份而不是本地改一条。失败只记日志：写已经成功，缓存没
/// 跟上不该让命令报错——下一次后台刷新会补上。
///
/// # Errors
///
/// 往返失败或写盘失败时返回 [`RepositoryError`]
pub fn store_workspaces(link: &RemoteLink, cache: &RemoteCache) -> Result<(), RepositoryError> {
    let items = fetch_list::<WireWorkspace>(link, &Request::WorkspaceList, "workspaces")?;
    cache.store_workspaces(&items)
}

/// 只重拉服务器列表并写穿透，见 [`store_workspaces`]
///
/// # Errors
///
/// 往返失败或写盘失败时返回 [`RepositoryError`]
pub fn store_servers(link: &RemoteLink, cache: &RemoteCache) -> Result<(), RepositoryError> {
    let items = fetch_list::<WireServer>(link, &Request::ServerList, "servers")?;
    cache.store_servers(&items)
}

/// 只重拉 Java 列表并写穿透，见 [`store_workspaces`]
///
/// # Errors
///
/// 往返失败或写盘失败时返回 [`RepositoryError`]
pub fn store_javas(link: &RemoteLink, cache: &RemoteCache) -> Result<(), RepositoryError> {
    let items = fetch_list::<WireJava>(link, &Request::JavaList, "javas")?;
    cache.store_javas(&items)
}

/// 刷新失败记进元信息（不覆盖上一次成功的条目）
pub fn record_failure(cache: &RemoteCache, message: &str) {
    if let Err(err) = cache.mark_failed(message) {
        // 记不下来不该让刷新进程崩：失败原因本来就已经在 stderr 上了
        tracing::warn!(remote = cache.name().as_str(), %err, "failed to record sync failure");
    }
}

/// 拉一份列表帧
fn fetch_list<T: serde::de::DeserializeOwned + Clone>(
    link: &RemoteLink,
    request: &Request,
    what: &'static str,
) -> Result<Vec<T>, RepositoryError> {
    let response = link.expect(request, what)?;
    match response {
        Response::Workspaces { items } => cast(items, what),
        Response::Servers { items } => cast(items, what),
        Response::Javas { items } => cast(items, what),
        other => Err(RepositoryError::Corrupt(format!(
            "{what}.list returned an unexpected frame: {other:?}"
        ))),
    }
}

/// 帧里的载荷类型与期望不一致时按损坏上报
///
/// 分派表按帧变体收敛，三个 `list` 各自只可能回一种载荷；这里用
/// `serde_json` 往返做一次「不可能失败」的转换，避免为三份列表各写一个
/// 泛型特化。
#[allow(clippy::needless_pass_by_value)] // 入参来自 `match` 的按值绑定
fn cast<T: serde::de::DeserializeOwned + Clone>(
    items: Vec<impl serde::Serialize>,
    what: &'static str,
) -> Result<Vec<T>, RepositoryError> {
    let value = serde_json::to_value(&items)
        .map_err(|err| RepositoryError::Corrupt(format!("{what}: {err}")))?;
    serde_json::from_value(value)
        .map_err(|err| RepositoryError::Corrupt(format!("{what}.list payload: {err}")))
}

/// 服务器列表里每台的运行状态，一次 `StatusMany` 拉齐
///
/// 没有服务器时不发帧：空 id 列表的往返没有意义。
fn fetch_statuses(
    link: &RemoteLink,
    servers: &[WireServer],
) -> Result<Vec<(String, WireStatusEntry)>, RepositoryError> {
    if servers.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = servers.iter().map(|srv| srv.id.clone()).collect();
    let response = link.expect(&Request::StatusMany { ids }, "statuses")?;
    let items = match response {
        Response::Statuses { items } => items,
        other => {
            return Err(RepositoryError::Corrupt(format!(
                "status.many returned an unexpected frame: {other:?}"
            )));
        }
    };
    if items.len() != servers.len() {
        return Err(RepositoryError::Corrupt(format!(
            "status.many returned {} entries for {} servers",
            items.len(),
            servers.len()
        )));
    }
    Ok(servers
        .iter()
        .zip(items)
        .map(|(srv, entry)| (srv.name.clone(), entry))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::cache::RemoteCache;
    use crate::remote::testkit::PairingHarness;
    use port::RemoteName;

    fn cache(dir: &std::path::Path) -> RemoteCache {
        RemoteCache::new(dir, RemoteName::parse("box").expect("name"))
    }

    #[test]
    fn refresh_stores_what_the_peer_holds() {
        let harness = PairingHarness::spawn();
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());

        // 对端先建一个工作区与一台服务器
        let workspaces = crate::remote::RemoteWorkspaceRepository::new(
            harness.link(),
            port::Origin::Remote(RemoteName::parse("box").expect("name")),
        );
        let ws_dir = harness.agent_data.join("srv");
        workspaces.init(&ws_dir, Some("w1")).expect("init");

        let servers = crate::remote::RemoteServerRepository::new(
            harness.link(),
            port::Origin::Remote(RemoteName::parse("box").expect("name")),
        );
        servers
            .create(Some("w1"), "web", "1.21.1", "vanilla", Some(21))
            .expect("create");

        let snapshot = refresh(&harness.link(), &cache).expect("refresh");
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.servers.len(), 1);
        assert_eq!(snapshot.statuses.len(), 1, "每台服务器一条状态");
        assert_eq!(snapshot.statuses[0].0, "web");

        // 落到盘上的内容与返回值一致
        assert_eq!(cache.workspaces().expect("cached").len(), 1);
        assert_eq!(cache.servers().expect("cached").len(), 1);
        let state = cache.state().expect("state");
        assert_eq!(state.last_sync, Some(snapshot.at));
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn unreachable_peer_reports_error_and_keeps_previous_snapshot() {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = cache(dir.path());
        cache
            .store(&Snapshot {
                workspaces: Vec::new(),
                servers: Vec::new(),
                javas: Vec::new(),
                statuses: Vec::new(),
                at: 42,
            })
            .expect("store");

        let dead = RemoteLink::anonymous(
            "127.0.0.1:1",
            transport::Fingerprint::of_der(b"refresh-test"),
        );
        let err = refresh(&dead, &cache).expect_err("应当失败");
        record_failure(&cache, &err.to_string());

        let state = cache.state().expect("state");
        assert_eq!(state.last_sync, Some(42), "旧快照的时间戳保留");
        assert!(state.last_error.is_some());
        assert!(state.is_stale(now_secs()));
    }
}
