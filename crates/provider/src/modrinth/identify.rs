//! 按文件哈希反查项目：内容识别与上游服务端归属的共同底座
//!
//! 两个上层需求指向同一个上游调用链（`POST /version_files` → `GET /projects`）：
//!
//! - [`port::content::ContentSource::by_hash`]：`mods/` 里这个 jar 是哪个项目的
//!   哪个版本（用于列表展示与更新判定）
//! - [`port::mod_::ModMetadataSource::server_support`]：上游对它的服务端归属
//!   怎么标注（用于 `mod inspect` 的联网兜底）
//!
//! 两条链路只在这里写一份，避免「同一个哈希查两次、两份结论」的分叉。判定
//! 语义留在各自的调用方：这里只回答「上游说了什么」。
//!
//! # 为什么按哈希而不是文件名
//!
//! 文件名由发布者随手起（`CobbleFurnies-fabric-1.2.jar` 里没有 modid），而
//! SHA1 是内容指纹。上游只对**发布过**的文件有记录：作者自己编译、第三方站
//! 转载的 jar 永远查不到，调用方必须如实报「未知」。

use super::client::{HashAlgorithm, ModrinthSource};
use super::manifest::{Project, Version};
use async_trait::async_trait;
use domain::mod_::SideSupport;
use port::SourceError;
use port::mod_::{ModMetadataSource, UpstreamSide};
use std::collections::{HashMap, HashSet};

/// 批量哈希查询的分批上限（上游约束，与整合包安装同口径）
const HASH_QUERY_BATCH: usize = 100;

/// 一次反查命中的三元组：哈希 + 版本 +（可能缺失的）项目详情
///
/// 项目详情只用于展示与侧归属判定，缺失不影响「这是哪个项目的哪个版本」这个
/// 主结论；但**查询失败**会如实冒泡——静默降级会让调用方拿到半份结果却当成
/// 完整结果。
pub(crate) struct Identified {
    /// 命中的文件哈希（上游原样返回）
    pub hash: String,
    /// 命中的版本
    pub version: Version,
    /// 项目详情；批量接口没返回该项时为 `None`
    pub project: Option<Project>,
}

impl ModrinthSource {
    /// 批量把文件哈希翻译成「版本 + 项目」
    ///
    /// 未命中的哈希不出现；同一项目的多个文件只会触发一次项目详情查询。
    ///
    /// # Errors
    ///
    /// 上游不可达、状态码异常或响应无法解析时返回 [`SourceError`]
    pub(crate) async fn identify(&self, hashes: &[String]) -> Result<Vec<Identified>, SourceError> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Identified> = Vec::new();
        for chunk in hashes.chunks(HASH_QUERY_BATCH) {
            // 上游按十六进制串比对，统一小写避免大小写不同的同一哈希漏命中
            let wanted: Vec<String> = chunk.iter().map(|hash| hash.to_ascii_lowercase()).collect();
            let versions = self
                .versions_from_hashes(&wanted, HashAlgorithm::Sha1)
                .await?;
            if versions.is_empty() {
                continue;
            }
            let project_ids: Vec<String> = versions
                .values()
                .map(|version| version.project_id.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let projects = self.projects(&project_ids).await?;
            let by_id: HashMap<&str, &Project> = projects
                .iter()
                .map(|project| (project.id.as_str(), project))
                .collect();
            for (hash, version) in versions {
                let project = by_id
                    .get(version.project_id.as_str())
                    .map(|project| (*project).clone());
                out.push(Identified {
                    hash,
                    version,
                    project,
                });
            }
        }
        // 上游按哈希键存，返回顺序不保证；排序让结果可复现，也便于调用方按哈希
        // 二分回查
        out.sort_by(|left, right| left.hash.cmp(&right.hash));
        Ok(out)
    }
}

#[async_trait]
impl ModMetadataSource for ModrinthSource {
    fn name(&self) -> &'static str {
        "modrinth"
    }

    async fn server_support(&self, hashes: &[String]) -> Result<Vec<UpstreamSide>, SourceError> {
        let mut out: Vec<UpstreamSide> = Vec::new();
        for found in self.identify(hashes).await? {
            let Some(project) = found.project.as_ref() else {
                continue;
            };
            // 只有上游明确给出服务端结论的条目才算「查到了」：字段缺失时按
            // 「上游没有信息」处理（不返回），调用方保持 `Unknown`。把「没回填」
            // 当成「可选」会让风险落到 `Ok`，用户会以为已经查过了。
            let Some(server) = side_of(project.server_side.as_deref()) else {
                continue;
            };
            let client = side_of(project.client_side.as_deref()).unwrap_or(SideSupport::Unknown);
            out.push(UpstreamSide {
                hash: found.hash,
                client,
                server,
                slug: project.slug.clone(),
            });
        }
        Ok(out)
    }
}

/// 上游 `required` / `optional` / `unsupported` → 侧归属
///
/// 认不出的取值（含字段缺失）返回 `None`：调用方据此保持「未知」，绝不把
/// 「上游没表态」折叠成「可用」。
fn side_of(raw: Option<&str>) -> Option<SideSupport> {
    match raw? {
        "required" => Some(SideSupport::Required),
        "optional" => Some(SideSupport::Optional),
        "unsupported" => Some(SideSupport::Unsupported),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_of_maps_known_values() {
        assert_eq!(side_of(Some("required")), Some(SideSupport::Required));
        assert_eq!(side_of(Some("optional")), Some(SideSupport::Optional));
        assert_eq!(side_of(Some("unsupported")), Some(SideSupport::Unsupported));
    }

    /// 认不出与缺失都返回 `None`：调用方不得据此判定「可用」
    #[test]
    fn side_of_rejects_unknown_and_missing() {
        assert_eq!(side_of(None), None);
        assert_eq!(side_of(Some("")), None);
        assert_eq!(side_of(Some("sometimes")), None);
    }
}
