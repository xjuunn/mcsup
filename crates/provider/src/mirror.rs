//! 内置下载源目录与探测实现
//!
//! 目录是**表**，不是逻辑：新增一个镜像 = 在 [`mirrors`] 里加一条
//! [`Source`]，或在 [`upstreams`] 里加一面。下载、安装、服务端生命周期都不认识
//! 具体站点，它们只消费 `domain::mirror::Catalog` 解析出来的候选列表。
//!
//! 只收**能省带宽或省时间**的绑定，且每条都实测过（见各条注释的取舍）。
//! 镜像不覆盖某条路径时不需要特判：探测会失败，选源自然落回官方；下载时也会
//! 逐个候选回退。因此「镜像部分覆盖」不是配置问题，而是探测问题。

use crate::http::{build_client, map_transport};
use async_trait::async_trait;
use domain::mirror::{Binding, Catalog, CatalogError, Source, SourceId, Upstream};
use port::{SourceError, SourceProbe};
use reqwest::Client;
use std::time::{Duration, Instant};
use url::Url;

/// `BMCLAPI`：国内镜像，覆盖 Mojang 元数据、`Forge` / `NeoForge` / `Fabric` 的 maven
pub const BMCLAPI: &str = "https://bmclapi2.bangbang93.com";
/// `MCIM`：社区镜像，覆盖 `Modrinth` / `CurseForge` 的 API 与文件
pub const MCIM: &str = "https://mod.mcimirror.top";

/// 探测客户端的超时
///
/// 探测只是「能不能用」的采样，卡住的源要尽快让位：默认 10 秒会把一次选源
/// 拖成十几个秒，而选源发生在第一次下载之前，用户体感就是「卡住不动」。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 内置上游面
///
/// 只列**字节下载**真正会经过的地址：镜像改写发生在 `ArtifactDownloader` 上，
/// 元数据（版本清单、搜索接口）走各自的 provider 客户端，不经过这条路，
/// 在这里登记它们只会得到一条永远不生效的规则。
///
/// # Errors
///
/// 键或前缀不合法时返回 [`CatalogError`]
pub fn upstreams() -> Result<Vec<Upstream>, CatalogError> {
    Ok(vec![
        // 原版服务端 JAR 与其它对象存储
        Upstream::new("mojang-objects", "https://piston-data.mojang.com")?,
        // Forge 安装器与 maven 制品
        Upstream::new("forge-maven", "https://maven.minecraftforge.net")?,
        // NeoForge 安装器：官方在 maven 根下多一段 `releases/`，镜像没有
        Upstream::new("neoforge-maven", "https://maven.neoforged.net/releases")?,
        // Fabric 服务端 JAR（loader 拼好的整包）
        Upstream::new("fabric-meta", "https://meta.fabricmc.net")?,
        // Fabric maven 制品
        Upstream::new("fabric-maven", "https://maven.fabricmc.net")?,
        // 内容文件的 CDN（mod / 整合包里的文件）
        Upstream::new("modrinth-cdn", "https://cdn.modrinth.com")?,
    ])
}

/// 内置镜像
///
/// # Errors
///
/// 绑定定义不合法时返回 [`CatalogError`]
pub fn mirrors() -> Result<Vec<Source>, CatalogError> {
    Ok(vec![bmclapi()?, mcim()?])
}

/// BMCLAPI
///
/// 取舍依据（实测，2026-11）：
///
/// - Forge 安装器 9.1 MB：官方 1.09 MB/s → 镜像 4.14 MB/s；NeoForge 6.8 MB：
///   0.50 → 4.61 MB/s。两条 maven 绑定是收益最大的两项。
/// - `/v1/objects/...` 与 `/version/<v>/server` 都是 302 回官方 CDN，没有缓存
///   收益，还多一跳；同源并发 >10 时观察到 429。仍保留绑定：探测按实际往返
///   排序，多一跳的源自然会输给官方，而镜像将来真的缓存之后不必改代码。
/// - `/fabric-meta/.../server/jar` 目前 404，探测会失败并落回官方。
/// - `files.minecraftforge.net` 的 `maven-metadata.json` 是**过期快照**
///   （`1.21.1` / `1.20.1` 都缺），因此不绑定它：镜像的元数据不能参与版本解析。
fn bmclapi() -> Result<Source, CatalogError> {
    Ok(Source::new(SourceId::new("bmclapi")?, "BMCLAPI")
        .with(Binding::new("mojang-objects", BMCLAPI)?)
        .with(Binding::new("forge-maven", format!("{BMCLAPI}/maven"))?)
        .with(Binding::new("neoforge-maven", format!("{BMCLAPI}/maven"))?)
        .with(Binding::new(
            "fabric-meta",
            format!("{BMCLAPI}/fabric-meta"),
        )?)
        .with(Binding::new("fabric-maven", format!("{BMCLAPI}/maven"))?))
}

/// MCIM
///
/// 实测 Modrinth 文件经 MCIM 回源到官方 CDN 后更慢（16.5 MB：官方 2.23 MB/s →
/// MCIM 0.64 MB/s），API 却更快（0.96 s → 0.29 s）。文件绑定保留：探测按实际
/// 往返排序，慢的源不会中选；API 侧不经过本层，见 [`upstreams`] 的说明。
///
/// 注意：文件 URL 上 MCIM **不缓存内容**，只是 `302` 跳回 `cdn.modrinth.com`
/// （`location` 里甚至保留了未编码的空格与方括号）。因此它不是官方 CDN 的
/// 备用源：官方侧握手失败时换到 MCIM 仍会走同一条链路，故障不会因此消失。
/// 探测排序与下载回退都只是尽人事，真正决定成败的是那条链路本身是否可用。
fn mcim() -> Result<Source, CatalogError> {
    Ok(Source::new(SourceId::new("mcim")?, "MCIM").with(Binding::new("modrinth-cdn", MCIM)?))
}

/// 内置目录：官方优先，镜像按推荐顺序排在后面
///
/// # Errors
///
/// 表里有不合法的键 / 前缀，或缺官方源时返回 [`CatalogError`]
pub fn builtin_catalog() -> Result<Catalog, CatalogError> {
    Catalog::with_mirrors(upstreams()?, mirrors()?)
}

/// 把用户配置的镜像前缀追加进目录
///
/// `download-sources` 的语义是「mod 文件的镜像前缀」：路径规则跟着上游走，
/// 所以它只能绑定到内容文件的 CDN 上，逐条成为独立源。第一条优先级最高
/// （探测并列时按目录顺序取先）。
///
/// 非法前缀被**跳过**而不是让装配失败：配置是用户手写的，一个手抖的条目不该
/// 让整个平台起不来；跳过的条目由调用方通过返回值告警。
pub fn with_configured_mirrors(catalog: &mut Catalog, prefixes: &[String]) -> Vec<String> {
    let mut rejected = Vec::new();
    for (index, prefix) in prefixes.iter().enumerate() {
        let id = SourceId::new(format!("mirror-{}", index + 1));
        let binding = Binding::new("modrinth-cdn", prefix.clone());
        match (id, binding) {
            (Ok(id), Ok(binding)) => {
                catalog.insert(Source::new(id, format!("自定义镜像 {}", index + 1)).with(binding));
            }
            _ => rejected.push(prefix.clone()),
        }
    }
    rejected
}

/// 基于 HTTP 的选源探测
#[derive(Debug)]
pub struct HttpSourceProbe {
    http: Client,
}

impl HttpSourceProbe {
    /// 构建探测器
    ///
    /// # Errors
    ///
    /// HTTP 客户端构建失败时返回 [`SourceError::Init`]
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            http: build_client()?,
        })
    }
}

#[async_trait]
impl SourceProbe for HttpSourceProbe {
    async fn probe(&self, location: &str) -> Result<Duration, SourceError> {
        let url = Url::parse(location).map_err(|err| SourceError::Init(Box::new(err)))?;
        let started = Instant::now();
        // 单条探测自己带超时：客户端级超时是给元数据用的（几十秒），
        // 选源必须能在几秒内给出结论。
        let budget = tokio::time::timeout(PROBE_TIMEOUT, self.head_or_byte(url));
        let outcome = budget
            .await
            .map_err(|_elapsed| SourceError::Network("probe timed out".into()))??;
        Ok(if outcome.is_success() {
            started.elapsed()
        } else {
            return Err(SourceError::Status {
                status: outcome.as_u16(),
            });
        })
    }
}

impl HttpSourceProbe {
    /// 先 `HEAD`，服务端不接受时退化成「只取一字节的 `GET`」，返回状态码
    async fn head_or_byte(&self, url: Url) -> Result<reqwest::StatusCode, SourceError> {
        let response = self
            .http
            .head(url.clone())
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status();
        // 有的 CDN 不支持 HEAD（405/501），有的干脆回 403；这一类不是「源挂了」，
        // 退化成取一字节再判一次，否则会把可用的源判成不可用。
        if !head_unsupported(status.as_u16()) {
            return Ok(status);
        }
        let response = self
            .http
            .get(url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(map_transport)?;
        Ok(response.status())
    }
}

/// HEAD 不可用：请求方法不被接受，或服务端策略性地拒绝无 body 的请求
fn head_unsupported(status: u16) -> bool {
    matches!(status, 403 | 405 | 501)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_resolves_real_urls() {
        let catalog = builtin_catalog().expect("catalog");
        let resolved = catalog
            .resolve("https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1-47.4.0/forge-1.20.1-47.4.0-installer.jar")
            .expect("forge installer");
        assert_eq!(resolved.upstream_key(), "forge-maven");
        let sources: Vec<&str> = resolved
            .candidates
            .iter()
            .map(|candidate| candidate.source.as_str())
            .collect();
        assert_eq!(sources, vec!["official", "bmclapi"]);
        assert_eq!(
            resolved.candidates[1].location,
            "https://bmclapi2.bangbang93.com/maven/net/minecraftforge/forge/1.20.1-47.4.0/forge-1.20.1-47.4.0-installer.jar"
        );
    }

    #[test]
    fn neoforge_drops_its_releases_segment_on_the_mirror() {
        let catalog = builtin_catalog().expect("catalog");
        let resolved = catalog
            .resolve("https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.80/neoforge-21.1.80-installer.jar")
            .expect("neoforge installer");
        assert_eq!(
            resolved.candidates[1].location,
            "https://bmclapi2.bangbang93.com/maven/net/neoforged/neoforge/21.1.80/neoforge-21.1.80-installer.jar"
        );
    }

    #[test]
    fn modrinth_files_have_a_mirror_candidate() {
        let catalog = builtin_catalog().expect("catalog");
        let resolved = catalog
            .resolve(
                "https://cdn.modrinth.com/data/AANobbMI/versions/2Ybcx7Za/sodium-fabric-0.6.13.jar",
            )
            .expect("mod file");
        assert_eq!(resolved.upstream_key(), "modrinth-cdn");
        assert_eq!(
            resolved.candidates[1].location,
            "https://mod.mcimirror.top/data/AANobbMI/versions/2Ybcx7Za/sodium-fabric-0.6.13.jar"
        );
    }

    #[test]
    fn unmapped_hosts_have_no_candidate() {
        let catalog = builtin_catalog().expect("catalog");
        for location in [
            // 没有国内镜像的官方源
            "https://api.papermc.io/v2/projects/paper",
            "https://api.purpurmc.org/v2/purpur/1.21.1/latest/download",
            "https://quiltmc.org/api/v1/download-latest-installer/java-universal",
            // JDK 走 GitHub release，镜像要重排路径，不是前缀替换
            "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.13%2B9/x.zip",
        ] {
            assert!(catalog.resolve(location).is_none(), "{location}");
        }
    }

    #[test]
    fn configured_mirrors_become_sources_and_bad_ones_are_reported() {
        let mut catalog = builtin_catalog().expect("catalog");
        let rejected = with_configured_mirrors(
            &mut catalog,
            &[
                "https://mirror.example/".to_owned(),
                "   ".to_owned(),
                "not a url".to_owned(),
            ],
        );
        // 前缀只做字符串拼接：除空白外都构得上，非法项由使用侧承担。
        // 源名按配置里的原始序号编排，被跳过的条目留下空号——
        // 序号是用户对照配置排查的锚点，重新编号会让它指向别的行。
        assert_eq!(rejected, vec!["   ".to_owned()]);
        let resolved = catalog
            .resolve("https://cdn.modrinth.com/data/x/y.jar")
            .expect("mod file");
        let sources: Vec<&str> = resolved
            .candidates
            .iter()
            .map(|candidate| candidate.source.as_str())
            .collect();
        assert_eq!(sources, vec!["official", "mcim", "mirror-1", "mirror-3"]);
    }

    #[test]
    fn head_fallback_covers_method_rejections_only() {
        assert!(head_unsupported(405));
        assert!(head_unsupported(501));
        assert!(!head_unsupported(404));
        assert!(!head_unsupported(429));
    }
}
