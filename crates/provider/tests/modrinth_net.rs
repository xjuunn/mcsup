//! Modrinth 官方 API 的端到端契约测试
//!
//! 默认不跑；设 `MCSUP_NET_TESTS=1` 手动执行。
//!
//! 只断言「上游形态与我们的模型相符」这一层：字段都在、能解析、主文件可取。
//! 具体某个项目 / 版本的细节会随上游变动，不做硬编码断言。

#![allow(clippy::expect_used, reason = "联网契约测试，失败直接 panic 才有用")]

use provider::modrinth::{
    Facet, FacetTree, HashAlgorithm, ModrinthSource, ProjectType, SearchIndex, SearchQuery,
    VersionFilter, file_checksum, primary_file,
};

fn enabled() -> bool {
    std::env::var("MCSUP_NET_TESTS").is_ok_and(|v| v == "1")
}

/// 官方源默认构造可用
#[tokio::test]
async fn default_source_points_at_official() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    assert_eq!(source.base_url(), "https://api.modrinth.com/v2");
}

/// 标签端点可达且非空
#[tokio::test]
async fn tags_are_reachable() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    assert!(!source.loaders().await.expect("loaders").is_empty());
    assert!(!source.game_versions().await.expect("games").is_empty());
    assert!(!source.categories().await.expect("categories").is_empty());
    let types = source.project_types().await.expect("types");
    // 本项目要下载的六类资源都应在上游的类型集合里
    for kind in [
        ProjectType::Mod,
        ProjectType::Modpack,
        ProjectType::ResourcePack,
        ProjectType::DataPack,
        ProjectType::Shader,
        ProjectType::Plugin,
    ] {
        assert!(
            types.iter().any(|tag| tag == kind.as_str()),
            "上游缺少类型 {}",
            kind.as_str()
        );
    }
}

/// 按类型搜索：每种资源都能搜出结果
#[tokio::test]
async fn search_each_project_type() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    for kind in [
        ProjectType::Mod,
        ProjectType::Modpack,
        ProjectType::ResourcePack,
        ProjectType::DataPack,
        ProjectType::Shader,
        ProjectType::Plugin,
    ] {
        let query = SearchQuery::new()
            .facets(FacetTree::new().and([Facet::eq("project_type", kind.as_str())]))
            .index(SearchIndex::Downloads)
            .page(0, 1);
        let results = source.search(&query).await.expect("search");
        // datapack 上游没有独立项目类型：可当数据包用的 mod 会以 `project_type=mod`
        // 命中，因此这里只断言「能搜到」，不断言命中的类型等于请求的类型
        assert!(results.total_hits > 0, "{} 应有结果", kind.as_str());
        // datapack 与 plugin 在上游都不是独立项目类型：可当数据包 / 插件用的
        // mod 会以 `project_type=mod` 命中。这两类只断言命中项确实声明了该用途，
        // 其余类型断言主类型一致。
        let hit = &results.hits[0];
        let multi_use = |tag: &str| hit.all_project_types.iter().any(|t| t == tag);
        match kind {
            ProjectType::DataPack => assert!(multi_use("datapack"), "{:?}", hit.all_project_types),
            ProjectType::Plugin => assert!(multi_use("plugin"), "{:?}", hit.all_project_types),
            other => assert_eq!(hit.project_type(), other),
        }
    }
}

/// 项目 → 版本 → 主文件 → 哈希，一条链路走得通
#[tokio::test]
async fn project_versions_chain() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    // Fabric API：跨版本长期存在，字段齐全
    let project = source.project("fabric-api").await.expect("project");
    assert_eq!(project.project_type(), ProjectType::Mod);

    let versions = source
        .project_versions(
            &project.id,
            &VersionFilter::new().loaders(["fabric"]).without_changelog(),
        )
        .await
        .expect("versions");
    assert!(!versions.is_empty());

    let first = &versions[0];
    assert_eq!(first.project_id, project.id);
    let file = primary_file(first).expect("主文件");
    assert!(!file.filename.is_empty());
    assert!(file.url.starts_with("https://"));
    assert!(file.size > 0);
    assert!(file_checksum(file).is_some(), "应有 sha1 可用于校验");

    // 单版本详情与列表里的一致
    let detail = source.version(&first.id).await.expect("version");
    assert_eq!(detail.id, first.id);

    // 批量接口能查回来
    let batch = source
        .versions(std::slice::from_ref(&first.id))
        .await
        .expect("batch");
    assert_eq!(batch.len(), 1);
}

/// 按哈希反查：取一个真实版本的 sha1 再查回来
#[tokio::test]
async fn hash_lookup_round_trip() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    let project = source.project("fabric-api").await.expect("project");
    let versions = source
        .project_versions(&project.id, &VersionFilter::new())
        .await
        .expect("versions");
    let file = primary_file(&versions[0]).expect("主文件");
    let sha1 = file.hashes.sha1.clone().expect("sha1");

    let found = source
        .version_from_hash(&sha1, HashAlgorithm::Sha1)
        .await
        .expect("from hash");
    assert!(
        found
            .files
            .iter()
            .any(|f| f.hashes.sha1.as_deref() == Some(&sha1))
    );

    let batch = source
        .versions_from_hashes(std::slice::from_ref(&sha1), HashAlgorithm::Sha1)
        .await
        .expect("batch");
    assert!(batch.contains_key(&sha1));
}

/// 依赖图：端点可达并能解析（上游对未被依赖的项目返回空图，因此不断言非空）
#[tokio::test]
async fn dependency_graph() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    let graph = source
        .project_dependencies("fabric-api")
        .await
        .expect("deps");
    // 出现在版本依赖里的项目必须带 id
    assert!(graph.projects.iter().all(|p| !p.id.is_empty()));
}

/// 团队成员：能取到项目作者名
#[tokio::test]
async fn team_members_lookup() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    let project = source.project("fabric-api").await.expect("project");
    let members = source
        .team_members(&project.team)
        .await
        .expect("team members");
    assert!(!members.is_empty());
    assert!(
        members
            .iter()
            .all(|m| m.team_id.as_deref() == Some(project.team.as_str()))
    );
    assert!(members.iter().any(|m| !m.user.username.is_empty()));
}

/// 不存在的项目归 404
#[tokio::test]
async fn unknown_project_is_error() {
    if !enabled() {
        return;
    }
    let source = ModrinthSource::new().expect("source");
    let err = source
        .project("definitely-not-a-real-project-xyz-123")
        .await
        .expect_err("应报 404");
    assert!(!err.retryable(), "{err:?}");
}
