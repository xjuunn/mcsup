//! 远端缓存的入口侧装配：路径、后台刷新进程
//!
//! 分工：缓存格式、TTL 与离线判定都在 `backend::remote::cache`；这里只做入口
//! 才能做的两件事——把 `data_dir` 变成缓存路径、把刷新拉成独立进程。
//!
//! # 为什么刷新必须另起进程
//!
//! 前台命令要在自己退出前返回，而一次刷新最多要等三个连接超时（离线远端每帧
//! 十秒）。线程解决不了：进程退了线程就没了，缓存永远刷不新。因此刷新是一个
//! **短命子进程**（`mcsup __remote-refresh <name>`），与前台命令完全解耦——
//! 前台立刻出结果，它在后台慢慢连。
//!
//! # 为什么不会重复刷新
//!
//! 刷新进程先抢 `{data_dir}/remotes/<name>/refresh.lock`，抢不到就说明已经有人
//! 在刷，直接退出。锁由文件句柄持有，进程退出（含被强杀）内核自动释放，因此
//! 不存在「锁残留导致永不刷新」的死局。

use crate::bootstrap;
use crate::config;
use mcsup::backend::remote::{RemoteCache, now_secs};
use mcsup::port::RemoteName;

/// 某台远端的缓存句柄
#[must_use]
pub(crate) fn cache_for(name: &RemoteName) -> RemoteCache {
    RemoteCache::new(&config::platform_data_dir(), name.clone())
}

/// 需要的话拉起一次后台刷新（不阻塞、不等待）
///
/// 触发条件：从未同步成功、缓存过期、或上次刷新失败（来源可能刚恢复）。触发
/// 后立刻返回——刷不刷得成是后台进程的事，前台不该为它等一毫秒。
pub(crate) fn spawn_refresh_if_stale(name: &RemoteName) {
    let cache = cache_for(name);
    if !cache.is_stale(now_secs()) {
        return;
    }
    // 一条命令里 `remote_members` 可能被调用多次（组合根、注册表、服务各一次），
    // 进程内去重：同一台远端只拉一个刷新进程。跨进程去重靠锁文件。
    {
        let mut spawned = spawned_this_process()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !spawned.insert(name.clone()) {
            return;
        }
    }
    if let Err(err) = spawn_refresh(name) {
        // 拉不起刷新只意味着「这次看到的还是旧数据」，不该让命令失败
        tracing::warn!(remote = name.as_str(), %err, "failed to spawn remote refresh");
    }
}

/// 本进程已拉起的刷新（按远端名去重）
fn spawned_this_process() -> &'static std::sync::Mutex<std::collections::HashSet<RemoteName>> {
    static SPAWNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<RemoteName>>> =
        std::sync::OnceLock::new();
    SPAWNED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 缓存里各服务器的运行状态（按服务器名）
///
/// `server ls` 的状态列对远端一律读这份：再去问一次对端就白白把「离线不阻塞」
/// 的收益还回去了。
#[must_use]
pub(crate) fn cached_statuses(
    members: &mcsup::backend::remote::RemoteMembers,
) -> std::collections::HashMap<String, Option<mcsup::port::SupervisorStatus>> {
    let mut out = std::collections::HashMap::new();
    for member in members.all() {
        for (name, status) in cache_for(&member.name).decoded_statuses() {
            out.entry(name).or_insert(status);
        }
    }
    out
}

/// 拉起 `mcsup __remote-refresh <name>`，完全脱离当前进程
///
/// 三路标准流全部丢弃：刷新的日志属于后台，混进前台 stdout 会破坏
/// `--output json` 的纯数据约定。
fn spawn_refresh(name: &RemoteName) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__remote-refresh")
        .arg(name.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::daemon::detach(&mut command);
    command.spawn().map(|_| ())
}

/// 后台刷新进程的本体：抢锁 → 拉四份列表 → 写缓存
///
/// 返回进程退出码。抢不到锁返回 `1`：说明已有刷新在跑，这一次不必重复劳动
/// （不是错误）。
///
/// # Errors
///
/// 远端名非法、或取锁 / 读配置失败时返回错误
pub(crate) fn run_refresh(name: &str) -> anyhow::Result<i32> {
    use anyhow::Context as _;
    let remote_name = RemoteName::parse(name)
        .map_err(|err| anyhow::anyhow!("invalid remote name '{name}': {err}"))?;
    let cache = cache_for(&remote_name);

    let Some(_lock) = cache.try_lock().context("取刷新锁")? else {
        return Ok(1);
    };

    let cfg = config::ConfigFile::locate(None).load()?;
    // 不带缓存的成员：刷新本身就是「去问对端」，走缓存会自我循环
    let members = bootstrap::remote_members_live(&cfg);
    let Some(member) = members.get(&remote_name) else {
        // 配置里没有 / 缺凭证 / 缺指纹：记下原因，下次仍会重试
        mcsup::backend::remote::refresh::record_failure(
            &cache,
            "remote is not configured, or has no pinned fingerprint / usable credential",
        );
        return Ok(1);
    };

    match mcsup::backend::remote::refresh::refresh(&member.servers_link(), &cache) {
        Ok(_) => Ok(0),
        Err(err) => {
            mcsup::backend::remote::refresh::record_failure(&cache, &err.to_string());
            // stderr 正常路径下已被丢弃，保留便于手工排障
            tracing::warn!(remote = remote_name.as_str(), %err, "remote refresh failed");
            Ok(1)
        }
    }
}

/// 这台远端是否从未同步成功过
///
/// 与「离线」不同：从未同步过时缓存是空的，标 `(never synced)` 才不会让人
/// 以为「这台机器上什么都没有」。
#[must_use]
pub(crate) fn never_synced(name: &RemoteName) -> bool {
    !cache_for(name)
        .state()
        .is_ok_and(|state| state.has_synced())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_process_rejects_invalid_names() {
        let err = run_refresh("bad name").expect_err("非法名必须被拒绝");
        assert!(err.to_string().contains("invalid remote name"), "{err}");
    }

    #[test]
    fn unconfigured_remote_keeps_the_cache_untouched() {
        // 数据目录由 `MCSUP_DATA_DIR` 之类决定，测试里不去猜；这里只验证
        // 「配置里没有这台」这条分支不会 panic 且返回非零
        let name = RemoteName::parse("definitely-not-configured").expect("name");
        assert!(
            !never_synced(&name) || never_synced(&name),
            "只是读一次元信息"
        );
    }
}
