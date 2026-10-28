use crate::config::{self, ConfigFile};
use crate::locale::Lang;
use anyhow::{Context, Result, anyhow};
use mcsup::backend::remote::{
    CombinedJavaRepository, CombinedServerRepository, CombinedWorkspaceRepository, RemoteLink,
    RemoteMember, RemoteMembers,
};
use mcsup::bootstrap::{Mcsup, McsupBuilder};
use mcsup::domain::server::version::ServerVariant;
use mcsup::port::{RemoteName, ServerJarSource};
use mcsup::service::{JavaService, RemoteVia};
use mcsup::transport::Fingerprint;
use std::path::PathBuf;
use std::sync::Arc;

/// 打开元数据仓库（配置目录下的 `mcsup/db`，一条记录一个 JSON 文件）
///
/// 所有命令从这里取库，禁止各自拼 `config::database_path()` + `open`。
pub(crate) fn open_store() -> Result<Arc<mcsup::storage::JsonStore>> {
    let path = config::database_path();
    let store = mcsup::storage::JsonStore::open(&path)
        .with_context(|| format!("打开元数据仓库 {}", path.display()))?;
    Ok(Arc::new(store))
}

/// 装配 `ServerRegistry`（服务器 + 工作目录两个仓库）
pub(crate) fn open_registry() -> Result<Arc<mcsup::service::ServerRegistry>> {
    let store = open_store()?;
    Ok(Arc::new(mcsup::service::ServerRegistry::new(
        store.clone(),
        store,
    )))
}

/// 平台 Java 安装根目录（`{data_dir}/mcsup/java`），不存在则创建
pub(crate) fn java_root() -> Result<PathBuf> {
    let root = config::platform_data_dir().join("java");
    std::fs::create_dir_all(&root).with_context(|| format!("创建 Java 目录 {}", root.display()))?;
    Ok(root)
}

/// 装配 `JavaService`
///
/// 源列表在此只组装不打网络；延迟探测排序由 service 在安装阶段
/// （[`mcsup::service::InstallStage::Probing`]）触发，本地命令因此零网络开销。
pub(crate) fn open_java_service() -> Result<JavaService> {
    let scanner = Arc::new(mcsup::java::HostJavaScanner::new());
    let registry = Arc::new(mcsup::provider::java::JavaSourceRegistry::all_sources());
    let downloader = Arc::new(build_downloader()?);
    let extractor = Arc::new(mcsup::file::CommandExtractor::new());
    let store = open_store()?;
    // Java 注册表用组合仓库：远端 JVM 出现在 `java list` 里（离线降级为本地）
    let cfg = ConfigFile::locate(None).load()?;
    let members = remote_members(&cfg)?;
    let repo: Arc<dyn mcsup::port::JavaRuntimeRepository> = if members.is_empty() {
        store.clone()
    } else {
        Arc::new(CombinedJavaRepository::new(store.clone(), members))
    };
    Ok(JavaService::new(
        repo,
        scanner,
        registry,
        downloader,
        extractor,
        java_root()?,
    ))
}

/// 组装下载器：HTTP 传输 + 断点续传范围请求
pub(crate) fn build_downloader() -> Result<mcsup::file::FileDownloader> {
    let bytes = Arc::new(mcsup::provider::http::HttpByteSource::new().context("初始化 HTTP 传输")?);
    Ok(mcsup::file::FileDownloader::new(bytes.clone()).with_range_source(bytes))
}

/// 已注册的服务端 JAR 来源（能力查询的数据源）
///
/// 直接复用库侧组合根的 [`mcsup::bootstrap::default_jar_sources`]：CLI 与库侧
/// **只有一份**变体注册表，新增变体不会出现「库支持、CLI 不支持」的漂移。
///
/// 构造不打网络，只准备客户端。
pub(crate) fn jar_sources() -> Result<Vec<Arc<dyn ServerJarSource>>> {
    mcsup::bootstrap::default_jar_sources().context("初始化服务端来源")
}

/// 可创建 / 可安装的变体集合：来源 `variant()` 去重，保持注册顺序
///
/// 在此基础上追加「非 JAR 来源」的变体：整合包由 `port::ModpackSource` 承载，
/// 没有 `ServerJarSource`，但同样应出现在 `server create` 的候选列表里。
pub(crate) fn available_variants() -> Result<Vec<ServerVariant>> {
    let sources = jar_sources()?;
    Ok(available_with_modpack(available_of(&sources)))
}

/// 在可用变体里补上整合包变体，并按「用户需要懂多少」排序
///
/// 排序口径（越不需要用户懂，越靠前）：
///
/// 1. `vanilla`：不用选加载器
/// 2. `modrinth-modpack`：加载器由整包决定，同样不用选
/// 3. 模组加载器（`fabric` / `forge` / `neoforge` / `quilt`）
/// 4. 服务端分支与代理（`paper` / `purpur` / `folia` / `leaf` / `velocity` …）
/// 5. 其余第三方变体保持注册顺序
///
/// 这样整合包一眼可见，且位置有明确理由，而不是随手提到前面。
pub(crate) fn available_with_modpack(mut available: Vec<ServerVariant>) -> Vec<ServerVariant> {
    if !available.contains(&ServerVariant::ModrinthModpack) {
        available.push(ServerVariant::ModrinthModpack);
    }
    sort_by_discovery_order(&mut available);
    available
}

/// 变体在「发现顺序」里的档位；越小越靠前
fn discovery_rank(variant: &ServerVariant) -> u8 {
    match variant {
        ServerVariant::Vanilla => 0,
        ServerVariant::ModrinthModpack => 1,
        ServerVariant::Fabric
        | ServerVariant::Forge
        | ServerVariant::NeoForge
        | ServerVariant::Quilt => 2,
        ServerVariant::Paper
        | ServerVariant::Purpur
        | ServerVariant::Folia
        | ServerVariant::Leaf
        | ServerVariant::Velocity
        | ServerVariant::Waterfall
        | ServerVariant::SpongeVanilla => 3,
        // `Unknown` 是历史数据兜底，不出现在可选列表里；`Custom` 与其余
        // 内置项保持注册顺序
        _ => 4,
    }
}

/// 按档位稳定排序（同档保持注册顺序）
fn sort_by_discovery_order(variants: &mut [ServerVariant]) {
    variants.sort_by_key(discovery_rank);
}

/// 变体在选择器里的展示标签
///
/// 只保留名字本身：列表靠排列顺序表达「需要懂多少」，不靠括号里的解释。
/// 整合包用短名 `modrinth`，与 `--variant modrinth` 别名一致。
#[must_use]
pub(crate) fn variant_label(variant: &ServerVariant) -> String {
    match variant {
        ServerVariant::ModrinthModpack => "modrinth".to_owned(),
        other => other.as_str().to_owned(),
    }
}

/// 展示标签 → 变体
///
/// 与 [`variant_label`] 互逆。候选里同一变体只有一种标签，因此按全名匹配即可。
#[must_use]
pub(crate) fn variant_from_label(
    picked: &str,
    available: &[ServerVariant],
) -> Option<ServerVariant> {
    available
        .iter()
        .find(|variant| variant_label(variant) == picked)
        .cloned()
}

/// 该变体是否由整合包承载（需要走整合包开服流程）
pub(crate) fn is_modpack(variant: &ServerVariant) -> bool {
    mcsup::service::is_modpack_variant(variant)
}

/// 整合包开服服务
pub(crate) fn modpack_service() -> Result<mcsup::service::ModpackService> {
    let data_dir = config::platform_data_dir();
    let mc = Mcsup::open(&data_dir).context("装配整合包服务")?;
    Ok(mc.modpack)
}

/// [`available_variants`] 的纯逻辑内核（可注入假来源测试）
pub(crate) fn available_of(sources: &[Arc<dyn ServerJarSource>]) -> Vec<ServerVariant> {
    let mut out: Vec<ServerVariant> = Vec::new();
    for source in sources {
        let variant = source.variant();
        if !out.contains(&variant) {
            out.push(variant);
        }
    }
    out
}

/// 变体 → 来源（[`available_of`] 的纯逻辑内核）
///
/// `Unknown` 是历史数据兜底（旧注册 / marker 缺失），回落 vanilla 来源；
/// 其余未注册变体返回 `None`，由调用方给出「可用变体」错误。
pub(crate) fn pick_source(
    variant: &ServerVariant,
    sources: &[Arc<dyn ServerJarSource>],
) -> Option<Arc<dyn ServerJarSource>> {
    let target = if *variant == ServerVariant::Unknown {
        &ServerVariant::Vanilla
    } else {
        variant
    };
    sources.iter().find(|s| &s.variant() == target).cloned()
}

/// 变体 → 来源；未注册变体报错并列出当前可用项
pub(crate) fn source_for(variant: &ServerVariant, lang: Lang) -> Result<Arc<dyn ServerJarSource>> {
    let sources = jar_sources()?;
    pick_source(variant, &sources).ok_or_else(|| {
        anyhow!(
            "{}",
            unsupported_message(variant, &available_of(&sources), lang)
        )
    })
}

/// 未支持 / 未知变体的统一错误文案（纯逻辑，入口与测试共用）
pub(crate) fn unsupported_message(
    variant: &ServerVariant,
    available: &[ServerVariant],
    lang: Lang,
) -> String {
    crate::i18n::pick(
        lang,
        format!(
            "暂未支持变体 {}；当前可用：{}",
            variant.as_str(),
            join_variants(available, lang)
        ),
        format!(
            "variant not supported yet: {}; currently available: {}",
            variant.as_str(),
            join_variants(available, lang)
        ),
    )
}

/// 可用变体拼接为 `vanilla、paper` / `vanilla, paper` 形式
pub(crate) fn join_variants(available: &[ServerVariant], lang: Lang) -> String {
    if available.is_empty() {
        return crate::i18n::text(lang, "（无）", "(none)").to_owned();
    }
    let sep = crate::i18n::text(lang, "、", ", ");
    available
        .iter()
        .map(ServerVariant::as_str)
        .collect::<Vec<_>>()
        .join(sep)
}

/// 读 `[remotes.*]` + 凭证存储，装配全部可达远端的成员表
///
/// 任何一台远端连不上都只是跳过（warn），不阻塞本地命令——离线是常态而非
/// 异常（`docs/remote.md`「设计原则 4」）。配置记录缺凭证同样跳过：可能是
/// 配对到一半被中断，入口命令会在用到该远端时报「未认证」。
pub(crate) fn remote_members(cfg: &mcsup::service::Config) -> Result<RemoteMembers> {
    let credentials = credential_store();
    let mut members = RemoteMembers::empty();
    for (name, record) in &cfg.remotes {
        if let Some(member) = build_member(name, record, &credentials)? {
            members.push(member);
        }
    }
    Ok(members)
}

/// 单台远端 → 成员；任何一步不满足都返回 `None`（跳过该台，不阻塞其余）
///
/// 过滤顺序：名字合法 → 接入方式已实现 → 有指纹 → 有凭证。拆出来是为了可测——
/// 过滤规则不碰文件系统，凭证经 trait 注入。
///
/// SSH 记录连的是**隧道地址**，而隧道每次命令才建：这里只装一个延迟解析器，
/// 不在这里起 ssh 子进程。装配发生在每个命令的开头，本地命令不该为远端付
/// 连接代价，离线远端也不该把本地命令卡在 SSH 超时上。
fn build_member(
    name: &str,
    record: &mcsup::service::RemoteRecord,
    credentials: &dyn mcsup::auth::CredentialStore,
) -> Result<Option<RemoteMember>> {
    let remote_name = match RemoteName::parse(name) {
        Ok(name) => name,
        Err(err) => {
            tracing::warn!(remote = %name, %err, "invalid remote name in config; skipping");
            return Ok(None);
        }
    };
    let Some(fingerprint_hex) = record.fingerprint.as_deref() else {
        tracing::warn!(remote = %name, "remote has no pinned fingerprint; skipping");
        return Ok(None);
    };
    let fingerprint = Fingerprint::parse(fingerprint_hex)
        .with_context(|| format!("解析远端 {name} 的证书指纹"))?;
    let credential = match credentials.get(name) {
        Ok(cred) => cred,
        Err(err) => {
            tracing::warn!(remote = %name, %err, "no usable credential; skipping");
            return Ok(None);
        }
    };
    let link = match record.via {
        RemoteVia::Direct => {
            RemoteLink::new(record.target.as_str(), fingerprint, &credential.token)
        }
        RemoteVia::Ssh => {
            let target = record.target.clone();
            RemoteLink::lazy(
                target.clone(),
                fingerprint,
                &credential.token,
                // 隧道在第一次真正发帧时才建；失败信息进传输层错误链
                move || {
                    crate::ssh::endpoint_for(&target).map_err(|err| {
                        mcsup::transport::TransportError::Io {
                            context: format!("open the ssh tunnel to {target}"),
                            source: std::io::Error::other(format!("{err:#}")),
                        }
                    })
                },
            )
        }
    };
    Ok(Some(RemoteMember::new(link, remote_name)))
}

/// 客户端凭证存储：优先系统钥匙串，无钥匙串时退到 `{data_dir}/credentials.toml`
fn credential_store() -> mcsup::auth::CredentialVault {
    open_credential_store()
}

/// [`credential_store`] 的公开口径：远端跟随日志等命令要按名读回 token
pub(crate) fn open_credential_store() -> mcsup::auth::CredentialVault {
    mcsup::auth::CredentialVault::open(config::platform_data_dir().join("credentials.toml"))
}

/// `open_mcsup` 的带远端版本：有已配对远端时注入组合仓库
///
/// 供 `McsupBuilder` 的三个仓库注入口使用；无远端时与 [`open_mcsup`]
/// 完全等价（组合仓库空远端透传，但省掉配置读取更干净）。
pub(crate) fn open_mcsup_with_remotes() -> Result<Mcsup> {
    let data_dir = config::platform_data_dir();
    let cfg = ConfigFile::locate(None).load()?;
    let members = remote_members(&cfg)?;
    if members.is_empty() {
        return open_mcsup();
    }
    let store = open_store()?;
    let combined_servers = Arc::new(CombinedServerRepository::new(
        store.clone(),
        members.clone(),
    ));
    // supervisor 路由：本地走默认 LocalSupervisor，远端走各自的 RPC；
    // 服务器列表必须用组合仓库（查来源的唯一真相源）
    let mut remote_supervisors = std::collections::HashMap::new();
    for member in members.all() {
        // 镜像仓库不暴露 link；经其 origin 重新构造一条等价连接不可取——
        // 直接在 RemoteMember 上保留 link。这里用 servers 镜像的 link 字段。
        remote_supervisors.insert(
            member.name.clone(),
            mcsup::backend::remote::RemoteSupervisor::new(member.servers_link()),
        );
    }
    let supervisor: Arc<dyn mcsup::port::SupervisorPort> =
        Arc::new(mcsup::backend::remote::RoutingSupervisor::new(
            Arc::new(mcsup::runtime::LocalSupervisor),
            remote_supervisors,
            combined_servers.clone(),
        ));
    // 备份路由：远端服务器的备份整段转发到对端（元数据帧），
    // 按 id 查组合仓库来源分派到对应远端
    let mut backup_executors = std::collections::HashMap::new();
    for member in members.all() {
        backup_executors.insert(
            member.name.clone(),
            mcsup::backend::remote::RemoteBackups::new(member.servers_link()),
        );
    }
    let remote_backups: Arc<dyn mcsup::port::RemoteBackupPort> = Arc::new(
        mcsup::backend::remote::RoutingBackups::new(backup_executors, combined_servers.clone()),
    );
    let mc = McsupBuilder::new()
        .store(store.clone())
        .supervisor(supervisor)
        .remote_backups(remote_backups)
        .workspace_repository(Arc::new(CombinedWorkspaceRepository::new(
            store.clone(),
            members.clone(),
        )))
        .server_repository(combined_servers)
        .java_repository(Arc::new(CombinedJavaRepository::new(
            store.clone(),
            members,
        )))
        .build(&data_dir)
        .context("装配 mcsup 组合根（含远端）")?;
    Ok(mc)
}

/// 装配 `ServerRegistry`（组合仓库版：本地 + 已配对远端）
pub(crate) fn open_registry_with_remotes() -> Result<Arc<mcsup::service::ServerRegistry>> {
    let store = open_store()?;
    let cfg = ConfigFile::locate(None).load()?;
    let members = remote_members(&cfg)?;
    let servers: Arc<dyn mcsup::port::ServerRepository> = if members.is_empty() {
        store.clone()
    } else {
        Arc::new(CombinedServerRepository::new(
            store.clone(),
            members.clone(),
        ))
    };
    let workspaces: Arc<dyn mcsup::port::WorkspaceRepository> = if members.is_empty() {
        store.clone()
    } else {
        Arc::new(CombinedWorkspaceRepository::new(store.clone(), members))
    };
    Ok(Arc::new(mcsup::service::ServerRegistry::new(
        servers, workspaces,
    )))
}

/// 装配组合仓库版 `WorkspaceService`（本地 + 已配对远端）
///
/// `workspace ls` / `server ls` 展示名字、`workspace use` 解析限定名都用它；
/// 写入仍委托本地（`current` 是本机状态）。
pub(crate) fn workspace_service_with_remotes() -> Result<mcsup::service::WorkspaceService> {
    let store = open_store()?;
    let cfg = ConfigFile::locate(None).load()?;
    let members = remote_members(&cfg)?;
    let workspaces: Arc<dyn mcsup::port::WorkspaceRepository> = if members.is_empty() {
        store.clone()
    } else {
        Arc::new(CombinedWorkspaceRepository::new(store.clone(), members))
    };
    Ok(mcsup::service::WorkspaceService::new(workspaces, store))
}

/// 库侧默认组合根（`mcsup::bootstrap`）：CLI 全部命令的统一装配口径。
///
/// 单一入口：路径来自 `config`，网络能力与默认实现全部来自库侧组合根，
/// CLI 不再自己拼 `JsonStore` / 下载器 / Java 服务。第三方宿主用同一个
/// 组合根即可获得与 CLI 完全一致的默认行为。
pub(crate) fn open_mcsup() -> Result<Mcsup> {
    let data_dir = config::platform_data_dir();
    Mcsup::open(&data_dir).context("装配 mcsup 组合根")
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use mcsup::auth::StoredCredential;
    use mcsup::service::RemoteRecord;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// 内存凭证存储：键值固定，避免测试碰真实数据目录
    struct MemCredentials(BTreeMap<String, StoredCredential>);

    impl mcsup::auth::CredentialStore for MemCredentials {
        fn put(
            &self,
            _name: &str,
            _cred: &StoredCredential,
        ) -> Result<(), mcsup::auth::CredentialError> {
            unimplemented!()
        }
        fn get(&self, name: &str) -> Result<StoredCredential, mcsup::auth::CredentialError> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| mcsup::auth::CredentialError::NotFound(name.to_owned()))
        }
        fn remove(&self, _name: &str) -> Result<(), mcsup::auth::CredentialError> {
            unimplemented!()
        }
        fn names(&self) -> Result<Vec<String>, mcsup::auth::CredentialError> {
            Ok(self.0.keys().cloned().collect())
        }
    }

    fn credential(token: &str) -> StoredCredential {
        StoredCredential {
            token: token.to_owned(),
            client_id: "0f0e0d0c-0b0a-49d8-a716-446655440000".to_owned(),
            role: "operator".to_owned(),
        }
    }

    fn record(target: &str, fingerprint: Option<&str>) -> RemoteRecord {
        RemoteRecord {
            target: target.to_owned(),
            via: RemoteVia::Direct,
            read_only: false,
            added_at: 1,
            fingerprint: fingerprint.map(str::to_owned),
            credential: None,
        }
    }

    const FPR: &str = "sha256:aaaaaaaabbbbbbbbccccccccddddddddeeeeeeeeffffffff0000111122223333";

    /// 正常记录 + 有凭证 → 一名成员
    #[test]
    fn valid_record_builds_member() {
        let creds = MemCredentials(BTreeMap::from([("box".to_owned(), credential("a"))]));
        let cfg = mcsup::service::Config {
            data_dir: None,
            remotes: BTreeMap::from([("box".to_owned(), record("127.0.0.1:7681", Some(FPR)))]),
        };
        let member = build_member("box", cfg.remotes.get("box").expect("rec"), &creds)
            .expect("build")
            .expect("member");
        assert_eq!(member.name.as_str(), "box");
    }

    /// 缺指纹 → 跳过（TOFU 未完成配对）
    #[test]
    fn missing_fingerprint_skips() {
        let creds = MemCredentials(BTreeMap::from([("box".to_owned(), credential("a"))]));
        let rec = record("127.0.0.1:7681", None);
        assert!(build_member("box", &rec, &creds).expect("build").is_none());
    }

    /// 缺凭证 → 跳过（配对到一半）
    #[test]
    fn missing_credential_skips() {
        let creds = MemCredentials(BTreeMap::new());
        let rec = record("127.0.0.1:7681", Some(FPR));
        assert!(build_member("box", &rec, &creds).expect("build").is_none());
    }

    /// ssh 接入也装配成成员：地址延迟解析，装配期不起 ssh 子进程
    #[test]
    fn ssh_via_builds_member_without_connecting() {
        let creds = MemCredentials(BTreeMap::from([("box".to_owned(), credential("a"))]));
        let mut rec = record("user@host", Some(FPR));
        rec.via = RemoteVia::Ssh;
        let member = build_member("box", &rec, &creds)
            .expect("build")
            .expect("member");
        assert_eq!(member.name.as_str(), "box");
        // 日志与错误信息里指代的是原始 SSH 目标，不是隧道端口
        assert_eq!(member.servers_link().target(), "user@host");
    }

    /// 非法远端名 → 跳过
    #[test]
    fn invalid_name_skips() {
        let creds = MemCredentials(BTreeMap::from([("b/ad".to_owned(), credential("a"))]));
        let rec = record("127.0.0.1:7681", Some(FPR));
        assert!(build_member("b/ad", &rec, &creds).expect("build").is_none());
    }

    /// 成员表空判定：过滤后一台不剩时走纯本地路径
    #[test]
    fn all_filtered_out_yields_empty_members() {
        let creds = Mutex::new(());
        let _ = creds; // 静默 unused：仅确认过滤逻辑组合
        let mut cfg = mcsup::service::Config::default();
        cfg.remotes.insert(
            "box".to_owned(),
            record("127.0.0.1:7681", None), // 缺指纹
        );
        let members = remote_members(&cfg).expect("members");
        assert!(members.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcsup::domain::server::version::{ServerVersion, VersionId};
    use mcsup::port::{ResolvedArtifact, SourceError};

    struct FakeSource {
        variant: ServerVariant,
    }

    #[async_trait::async_trait]
    impl ServerJarSource for FakeSource {
        fn variant(&self) -> ServerVariant {
            self.variant.clone()
        }

        fn name(&self) -> &'static str {
            "fake"
        }

        async fn versions(&self) -> Result<Vec<ServerVersion>, SourceError> {
            Ok(Vec::new())
        }

        async fn artifact(&self, version: &VersionId) -> Result<ResolvedArtifact, SourceError> {
            // 能力查询测试不触网：版本解析走 NotFound 即可
            Err(SourceError::NotFound(version.clone()))
        }
    }

    fn sources(variants: &[ServerVariant]) -> Vec<Arc<dyn ServerJarSource>> {
        variants
            .iter()
            .map(|v| Arc::new(FakeSource { variant: v.clone() }) as Arc<dyn ServerJarSource>)
            .collect()
    }

    #[test]
    fn available_variants_dedupe_in_registration_order() {
        let sources = sources(&[
            ServerVariant::Vanilla,
            ServerVariant::Paper,
            ServerVariant::Vanilla,
        ]);
        assert_eq!(
            available_of(&sources),
            vec![ServerVariant::Vanilla, ServerVariant::Paper]
        );
        assert!(available_of(&[]).is_empty());
    }

    #[test]
    fn pick_source_finds_by_variant_and_unknown_falls_back_to_vanilla() {
        let list = sources(&[ServerVariant::Vanilla, ServerVariant::Paper]);
        assert!(pick_source(&ServerVariant::Paper, &list).is_some());
        // Unknown → vanilla 兜底
        assert!(pick_source(&ServerVariant::Unknown, &list).is_some());
        let vanilla_only = sources(&[ServerVariant::Vanilla]);
        assert!(pick_source(&ServerVariant::Paper, &vanilla_only).is_none());
        assert!(pick_source(&ServerVariant::Paper, &[]).is_none());
    }

    #[test]
    fn unsupported_message_lists_available_variants() {
        let msg = unsupported_message(&ServerVariant::Paper, &[ServerVariant::Vanilla], Lang::Zh);
        assert!(msg.contains("paper"), "{msg}");
        assert!(msg.contains("vanilla"), "{msg}");
        let empty = unsupported_message(&ServerVariant::Paper, &[], Lang::Zh);
        assert!(empty.contains("无"), "{empty}");
    }

    #[test]
    fn modpack_sorts_right_after_vanilla() {
        let available = available_with_modpack(vec![
            ServerVariant::Paper,
            ServerVariant::Fabric,
            ServerVariant::Vanilla,
        ]);
        assert_eq!(
            available,
            vec![
                ServerVariant::Vanilla,
                ServerVariant::ModrinthModpack,
                ServerVariant::Fabric,
                ServerVariant::Paper,
            ]
        );
    }

    #[test]
    fn modpack_not_duplicated_when_already_available() {
        let available =
            available_with_modpack(vec![ServerVariant::Vanilla, ServerVariant::ModrinthModpack]);
        assert_eq!(
            available
                .iter()
                .filter(|v| **v == ServerVariant::ModrinthModpack)
                .count(),
            1
        );
    }

    #[test]
    fn variant_label_round_trips_through_lookup() {
        let available = available_with_modpack(vec![
            ServerVariant::Vanilla,
            ServerVariant::Fabric,
            ServerVariant::Paper,
        ]);
        for variant in &available {
            let label = variant_label(variant);
            assert_eq!(
                variant_from_label(&label, &available).as_ref(),
                Some(variant),
                "标签 {label} 无法回映"
            );
        }
        assert!(variant_label(&ServerVariant::ModrinthModpack).contains("modrinth"));
    }

    #[test]
    fn jar_sources_reports_vanilla_and_fabric_capability() {
        let available = available_variants().expect("来源装配");
        assert!(available.contains(&ServerVariant::Vanilla), "{available:?}");
        assert!(available.contains(&ServerVariant::Fabric), "{available:?}");
    }
}
