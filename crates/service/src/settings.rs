//! 服务属性编排：键与值的解析（domain）→ 取值校验（Java 运行时存在性）
//! → 持久化（注册表 / storage）
//!
//! 属性影响启动行为（`java-version` 选运行时、`ram` 定堆上限），因此校验
//! 收敛在此：绑定了本机没有的大版本会在 `server start` 时才暴露就太晚了。

use crate::java::JavaService;
use crate::registry::{RegisteredServer, ServerRegistry, ServerRegistryError};
use domain::java::JavaMajor;
use domain::server::settings::{InvalidSettingValue, ServerSettings, SettingKey};
use port::ScannedJava;
use thiserror::Error;

/// 服务属性设置失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SettingsError {
    /// 注册表失败（含服务器不存在）
    #[error(transparent)]
    Registry(#[from] ServerRegistryError),
    /// Java 运行时查询（列出 / 扫描）失败
    #[error(transparent)]
    Java(#[from] crate::JavaError),
    /// 值不满足该键的格式（domain 解析失败）
    #[error(transparent)]
    InvalidValue(#[from] InvalidSettingValue),
    /// 绑定的大版本没有任何已注册运行时
    #[error("Java {major} is not registered")]
    UnknownJavaMajor {
        /// 服务器名
        name: String,
        /// 未注册的大版本
        major: JavaMajor,
    },
}

impl SettingsError {
    /// 面向用户展示的分类；结构化错误不带文案，入口按此渲染提示
    #[must_use]
    pub fn kind(&self) -> Option<SettingsErrorKind<'_>> {
        let kind = match self {
            Self::Registry(_) | Self::Java(_) | Self::InvalidValue(_) => return None,
            Self::UnknownJavaMajor { name, major } => SettingsErrorKind::UnknownJavaMajor {
                name,
                major: *major,
            },
        };
        Some(kind)
    }
}

/// [`SettingsError::kind`] 的结果：只带判定与参数，文案由入口渲染
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum SettingsErrorKind<'a> {
    /// 绑定的大版本未注册
    UnknownJavaMajor {
        /// 服务器名
        name: &'a str,
        /// 未注册的大版本
        major: JavaMajor,
    },
}

/// 一次设置的结果
#[derive(Debug, Clone)]
pub struct SettingChange {
    /// 被设置的键
    pub key: SettingKey,
    /// 写入值的展示口径（规范化后，如 `2G` → `2048M`）
    pub value: String,
    /// 设置后的完整属性集
    pub settings: ServerSettings,
}

/// 服务属性读写：校验 + 持久化
pub struct ServerSettingsService {
    registry: std::sync::Arc<ServerRegistry>,
    java: JavaService,
}

impl std::fmt::Debug for ServerSettingsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSettingsService")
            .field("registry", &"…")
            .field("java", &"…")
            .finish_non_exhaustive()
    }
}

impl ServerSettingsService {
    /// 组合注册表与 Java 服务
    #[must_use]
    pub fn new(registry: std::sync::Arc<ServerRegistry>, java: JavaService) -> Self {
        Self { registry, java }
    }

    /// 服务器当前属性（含注册信息，入口展示 / 交互默认值用）
    ///
    /// # Errors
    ///
    /// 服务器不存在或注册表失败时返回 [`SettingsError`]
    pub fn read(&self, name: &str) -> Result<(RegisteredServer, ServerSettings), SettingsError> {
        let server = self.registry.get(name)?;
        Ok((server.clone(), server.meta.settings))
    }

    /// 设置一项：解析并校验值，持久化后返回结果
    ///
    /// `java-version` 会触发 Java 首次扫描（回调与 `java list` 同一套，
    /// 入口用于打印扫描进度），并要求该大版本确有已注册运行时。
    ///
    /// # Errors
    ///
    /// 服务器不存在、值非法、大版本未注册或持久化失败时返回 [`SettingsError`]
    pub fn set(
        &self,
        name: &str,
        key: SettingKey,
        raw: &str,
        on_first_scan: &mut dyn FnMut(),
        on_found: &mut dyn FnMut(&ScannedJava),
    ) -> Result<SettingChange, SettingsError> {
        let server = self.registry.get(name)?;
        let mut settings = server.meta.settings;
        settings.apply(key, raw)?;
        // `java-version` 要求该大版本确有已注册运行时（缺失时绑定只会在
        // start 时才暴露，太晚）；校验通过才落库
        if key == SettingKey::JavaVersion
            && let Some(major) = settings.java_major
        {
            let runtimes = self.java.list_or_scan(on_first_scan, on_found)?;
            if !runtimes.iter().any(|rt| rt.major == major) {
                return Err(SettingsError::UnknownJavaMajor {
                    name: name.to_owned(),
                    major,
                });
            }
        }
        self.registry.update_settings(name, &settings)?;
        Ok(SettingChange {
            key,
            value: settings
                .value_of(key)
                .unwrap_or_else(|| raw.trim().to_owned()),
            settings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::java::{JavaRuntime, JavaVendor};
    use domain::server::meta::{InstalledJar, ServerName};
    use domain::server::version::{ServerVariant, VersionId};
    use domain::workspace::{Workspace, WorkspaceId, WorkspaceName};
    use port::{
        ArchiveExtractor, ArtifactDownloader, CancellationToken, DownloadError, DownloadRequest,
        DownloadedArtifact, ExtractError, JavaPackageRequest, JavaRuntimeRepository, JavaScanner,
        JavaSourceRegistryPort, ProgressSink, RepositoryError, ServerRepository, SourceError,
        WorkspaceRepository,
    };
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    struct MemServers {
        items: Mutex<Vec<domain::server::meta::ServerMeta>>,
    }

    impl ServerRepository for MemServers {
        fn create(&self, meta: &domain::server::meta::ServerMeta) -> Result<(), RepositoryError> {
            self.items.lock().expect("lock").push(meta.clone());
            Ok(())
        }
        fn get(
            &self,
            name: &ServerName,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            self.items
                .lock()
                .expect("lock")
                .iter()
                .find(|m| &m.name == name)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<domain::server::meta::ServerMeta>, RepositoryError> {
            Ok(self.items.lock().expect("lock").clone())
        }
        fn update_core(
            &self,
            _name: &ServerName,
            _version: &VersionId,
            _jar: &InstalledJar,
            _required_java_major: Option<JavaMajor>,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
        fn update_settings(
            &self,
            name: &ServerName,
            settings: &ServerSettings,
            updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            let mut items = self.items.lock().expect("lock");
            let meta = items
                .iter_mut()
                .find(|m| &m.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))?;
            meta.settings = settings.clone();
            meta.updated_at = updated_at;
            Ok(meta.clone())
        }
        fn remove(&self, _name: &ServerName) -> Result<(), RepositoryError> {
            unreachable!("not used in tests")
        }
        fn remove_by_workspace(&self, _workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink(
            &self,
            _name: &ServerName,
            _workspace_id: &WorkspaceId,
            _rel_dir: &Path,
            _updated_at: i64,
        ) -> Result<domain::server::meta::ServerMeta, RepositoryError> {
            unreachable!("not used in tests")
        }
    }

    struct MemWorkspaces {
        item: Mutex<Option<Workspace>>,
    }

    impl WorkspaceRepository for MemWorkspaces {
        fn create(&self, ws: &Workspace) -> Result<(), RepositoryError> {
            *self.item.lock().expect("lock") = Some(ws.clone());
            Ok(())
        }
        fn get(&self, id: &WorkspaceId) -> Result<Workspace, RepositoryError> {
            self.item
                .lock()
                .expect("lock")
                .clone()
                .filter(|ws| &ws.id == id)
                .ok_or_else(|| RepositoryError::NotFound(id.as_str().to_owned()))
        }
        fn get_by_name(&self, name: &WorkspaceName) -> Result<Workspace, RepositoryError> {
            self.item
                .lock()
                .expect("lock")
                .clone()
                .filter(|ws| &ws.name == name)
                .ok_or_else(|| RepositoryError::NotFound(name.as_str().to_owned()))
        }
        fn list(&self) -> Result<Vec<Workspace>, RepositoryError> {
            Ok(self
                .item
                .lock()
                .expect("lock")
                .clone()
                .into_iter()
                .collect())
        }
        fn current(&self) -> Result<Option<Workspace>, RepositoryError> {
            Ok(self.item.lock().expect("lock").clone())
        }
        fn set_current(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn remove(&self, _id: &WorkspaceId) -> Result<(), RepositoryError> {
            Ok(())
        }
        fn relink_path(
            &self,
            _id: &WorkspaceId,
            _path: &Path,
            _updated_at: i64,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    struct MemJavaRepo {
        runtimes: Mutex<Vec<JavaRuntime>>,
    }

    impl JavaRuntimeRepository for MemJavaRepo {
        fn upsert(&self, runtime: &JavaRuntime) -> Result<(), RepositoryError> {
            self.runtimes.lock().expect("lock").push(runtime.clone());
            Ok(())
        }
        fn list(&self) -> Result<Vec<JavaRuntime>, RepositoryError> {
            Ok(self.runtimes.lock().expect("lock").clone())
        }
        fn get_by_path(&self, path: &Path) -> Result<JavaRuntime, RepositoryError> {
            self.runtimes
                .lock()
                .expect("lock")
                .iter()
                .find(|r| r.path == path)
                .cloned()
                .ok_or_else(|| RepositoryError::NotFound(path.display().to_string()))
        }
        fn remove(&self, path: &Path) -> Result<(), RepositoryError> {
            let mut list = self.runtimes.lock().expect("lock");
            let before = list.len();
            list.retain(|r| r.path != path);
            if list.len() == before {
                return Err(RepositoryError::NotFound(path.display().to_string()));
            }
            Ok(())
        }
        fn scan_state(&self) -> Result<Option<i64>, RepositoryError> {
            Ok(Some(1))
        }
        fn mark_scanned(&self, _at: i64) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    struct NoopScanner;

    impl JavaScanner for NoopScanner {
        fn scan(
            &self,
            _on_found: &mut dyn FnMut(ScannedJava),
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
    }

    struct NoopJavaRegistry;

    #[async_trait::async_trait]
    impl JavaSourceRegistryPort for NoopJavaRegistry {
        fn source_names(&self) -> Vec<&'static str> {
            vec!["noop"]
        }
        async fn ranked(&self) -> Arc<dyn JavaSourceRegistryPort> {
            Arc::new(NoopJavaRegistry)
        }
        async fn list_packages(
            &self,
            _request: &JavaPackageRequest,
        ) -> Result<Vec<port::JavaPackage>, SourceError> {
            Ok(Vec::new())
        }
        async fn resolve_binary(
            &self,
            _package: &port::JavaPackage,
        ) -> Result<port::ResolvedJava, SourceError> {
            Err(SourceError::NotFound(VersionId::new("none")))
        }
    }

    struct NoopDownloader;

    #[async_trait::async_trait]
    impl ArtifactDownloader for NoopDownloader {
        async fn download(
            &self,
            _request: &DownloadRequest,
            _progress: Option<&dyn ProgressSink>,
            _cancel: &CancellationToken,
        ) -> Result<DownloadedArtifact, DownloadError> {
            Err(DownloadError::Cancelled)
        }
    }

    struct NoopExtractor;

    impl ArchiveExtractor for NoopExtractor {
        fn is_extractable(&self, _archive: &Path) -> bool {
            false
        }
        fn extract(&self, _archive: &Path, _dest: &Path) -> Result<(), ExtractError> {
            Ok(())
        }
    }

    fn runtime(path: &Path, major: i64) -> JavaRuntime {
        JavaRuntime {
            id: format!("id-{major}"),
            path: path.to_path_buf(),
            major: JavaMajor::parse(major).expect("major"),
            version: format!("{major}.0.0"),
            vendor: JavaVendor::Temurin,
            managed: false,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    /// 建好 workspace + 服务器 + Java 运行时的服务
    fn fixture(runtimes: Vec<JavaRuntime>) -> (ServerSettingsService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws_path = tmp.path().join("ws");
        std::fs::create_dir_all(ws_path.join("servers")).expect("mkdir");
        let ws = Workspace {
            id: WorkspaceId::parse("550e8400-e29b-41d4-a716-446655440001").expect("uuid"),
            name: WorkspaceName::parse("ws01").expect("valid"),
            path: ws_path,
            is_current: true,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        let registry = Arc::new(ServerRegistry::new(
            Arc::new(MemServers {
                items: Mutex::new(Vec::new()),
            }),
            Arc::new(MemWorkspaces {
                item: Mutex::new(Some(ws)),
            }),
        ));
        registry
            .create(
                None,
                "alpha",
                &VersionId::new("1.21.1"),
                ServerVariant::Vanilla,
                None,
            )
            .expect("create");
        let java = JavaService::new(
            Arc::new(MemJavaRepo {
                runtimes: Mutex::new(runtimes),
            }),
            Arc::new(NoopScanner),
            Arc::new(NoopJavaRegistry),
            Arc::new(NoopDownloader),
            Arc::new(NoopExtractor),
            PathBuf::from("/nonexistent-java-root"),
        );
        (ServerSettingsService::new(registry, java), tmp)
    }

    fn set(
        svc: &ServerSettingsService,
        key: SettingKey,
        raw: &str,
    ) -> Result<SettingChange, SettingsError> {
        svc.set("alpha", key, raw, &mut || {}, &mut |_| {})
    }

    #[test]
    fn set_ram_persists_and_normalizes_value() {
        let (svc, _tmp) = fixture(Vec::new());
        let change = set(&svc, SettingKey::Ram, "2G").expect("set ram");
        assert_eq!(change.value, "2048M");
        assert_eq!(change.settings.ram_mib, Some(2048));

        let (_server, settings) = svc.read("alpha").expect("read");
        assert_eq!(settings.ram_mib, Some(2048));
    }

    #[test]
    fn set_autostart_accepts_true_false() {
        let (svc, _tmp) = fixture(Vec::new());
        assert!(
            set(&svc, SettingKey::Autostart, "true")
                .expect("on")
                .settings
                .autostart
        );
        assert!(
            !set(&svc, SettingKey::Autostart, "false")
                .expect("off")
                .settings
                .autostart
        );
    }

    #[test]
    fn set_java_version_requires_registered_major() {
        let (svc, _tmp) = fixture(vec![runtime(Path::new("/opt/java21"), 21)]);
        let change = set(&svc, SettingKey::JavaVersion, "21").expect("bound");
        assert_eq!(
            change.settings.java_major,
            Some(JavaMajor::parse(21).expect("21"))
        );

        let err = set(&svc, SettingKey::JavaVersion, "17").expect_err("unregistered");
        assert!(matches!(err, SettingsError::UnknownJavaMajor { major, .. }
            if major == JavaMajor::parse(17).expect("17")));
        // 校验失败不落库：绑定保持 21
        let (_server, settings) = svc.read("alpha").expect("read");
        assert_eq!(settings.java_major, Some(JavaMajor::parse(21).expect("21")));
    }

    #[test]
    fn set_invalid_value_is_usage_level_and_changes_nothing() {
        let (svc, _tmp) = fixture(Vec::new());
        assert!(matches!(
            set(&svc, SettingKey::Ram, "abc"),
            Err(SettingsError::InvalidValue(_))
        ));
        assert!(matches!(
            set(&svc, SettingKey::Autostart, "yes"),
            Err(SettingsError::InvalidValue(_))
        ));
        let (_server, settings) = svc.read("alpha").expect("read");
        assert_eq!(settings, ServerSettings::default());
    }

    #[test]
    fn set_unknown_server_is_registry_error() {
        let (svc, _tmp) = fixture(Vec::new());
        let err = svc
            .set("ghost", SettingKey::Ram, "2G", &mut || {}, &mut |_| {})
            .expect_err("missing");
        assert!(matches!(err, SettingsError::Registry(_)));
    }
}
