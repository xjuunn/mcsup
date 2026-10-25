//! `SpongeVanilla` 官方 Maven 元数据
//!
//! `SpongeVanilla` 发布在 Sponge 自己的 Nexus Maven 仓库里，元数据是
//! **XML**（`maven-metadata.xml`），不是 JSON——本项目其他来源都吃 JSON，这里
//! 必须做 XML 解析。
//!
//! 端点（均已实测可达）：
//!
//! ```text
//! GET {repo}/org/spongepowered/spongevanilla/maven-metadata.xml
//! GET {repo}/org/spongepowered/spongevanilla/{ver}/spongevanilla-{ver}.jar
//! GET {repo}/org/spongepowered/spongevanilla/{ver}/spongevanilla-{ver}.jar.sha1
//! ```
//!
//! # 为什么不用现成的 XML 库
//!
//! 需要的只是「把 `<version>…</version>` 全部取出来，外加 `<latest>` /
//! `<release>` 两个单值」。为此引一个完整 XML 解析器（`quick-xml` /
//! `roxmltree`）进依赖树，收益不抵成本：本项目的 XML 面只有这一处，且字段
//! 形状固定。这里做的是**严格但最小**的扫描：只认 `<tag>值</tag>` 形式，不处理
//! 命名空间、属性（`<versions>` 之外无属性）、CDATA、嵌套。
//!
//! # 顺序
//!
//! Maven 元数据**不保证** `<versions>` 的顺序（Nexus 通常按插入序，即旧在前，
//! 但这是实现细节而非契约）。因此 [`Metadata::versions`] 原样保留上游顺序，
//! 「最新」一律以 `<latest>` / `<release>` 为准，不靠位置推断。
//!
//! # 版本串形状
//!
//! `{mc}-{sponge}`，如 `26.3-21.0.0-RC2755`。`26.3` 是 MC 版本，后半是 Sponge
//! 实现版本。反查 MC 版本见 [`mc_of`]。

/// `maven-metadata.xml` 解析结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Metadata {
    /// `<latest>`：上游认为的最新版本
    pub(crate) latest: Option<String>,
    /// `<release>`：上游认为的最新**正式**版本
    pub(crate) release: Option<String>,
    /// `<versions>` 里的全部版本，保持上游顺序
    pub(crate) versions: Vec<String>,
}

/// 解析 `<tag>值</tag>` 形式的单个值
///
/// `tag` 需为字面量标签名（不含 `<` / `>`）。取第一处匹配，值会去掉首尾空白。
fn single_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim().to_owned())
}

/// 取得所有 `<version>…</version>`（**不含** `<latest>` / `<release>`，它们
/// 虽然也带 version 字样但标签名不同）
fn all_versions(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(found) = xml[cursor..].find("<version>") {
        let start = cursor + found + "<version>".len();
        let Some(end_offset) = xml[start..].find("</version>") else {
            break;
        };
        let value = xml[start..start + end_offset].trim();
        if !value.is_empty() {
            out.push(value.to_owned());
        }
        cursor = start + end_offset + "</version>".len();
    }
    out
}

impl Metadata {
    /// 解析 `maven-metadata.xml`
    ///
    /// 不要求 XML 合法：只提取认得的标签，认不出的原样跳过。这比引入完整 XML
    /// 解析器更宽容，也更符合「上游多给字段不影响我」的惯例。
    #[must_use]
    pub(crate) fn parse(xml: &str) -> Self {
        Self {
            latest: single_tag(xml, "latest").filter(|v| !v.is_empty()),
            release: single_tag(xml, "release").filter(|v| !v.is_empty()),
            versions: all_versions(xml),
        }
    }
}

/// 版本串 → MC 版本（`26.3-21.0.0-RC2755` → `26.3`）
///
/// 约定是 `{mc}-{sponge}`，第一个 `-` 之前是 MC 版本。极老的 Sponge 版本串
/// 可能没有 `-`（纯 MC 版本），此时整串即 MC 版本。
#[must_use]
pub(crate) fn mc_of(version: &str) -> &str {
    version.split_once('-').map_or(version, |(mc, _)| mc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/sponge_metadata.xml");

    #[test]
    fn parses_latest_release_and_versions() {
        let meta = Metadata::parse(FIXTURE);
        assert_eq!(meta.latest.as_deref(), Some("26.3-21.0.0-RC2755"));
        assert_eq!(meta.release.as_deref(), Some("26.3-21.0.0-RC2755"));
        assert_eq!(meta.versions.len(), 5);
        assert_eq!(
            meta.versions.first().map(String::as_str),
            Some("26.3-21.0.0-RC2724")
        );
        assert_eq!(
            meta.versions.last().map(String::as_str),
            Some("26.3-21.0.0-RC2755")
        );
    }

    /// `<latest>` / `<release>` 的值本身是版本列表成员，不该被重复计入
    #[test]
    fn latest_and_release_are_not_counted_as_versions() {
        let meta = Metadata::parse(FIXTURE);
        let latest = meta.latest.clone().expect("latest");
        // 该值确实在列表里（它是成员，不是额外条目）
        assert_eq!(
            meta.versions.iter().filter(|v| **v == latest).count(),
            1,
            "{:?}",
            meta.versions
        );
    }

    #[test]
    fn missing_tags_are_none_not_error() {
        let meta = Metadata::parse("<metadata></metadata>");
        assert_eq!(meta.latest, None);
        assert_eq!(meta.release, None);
        assert!(meta.versions.is_empty());
        let meta = Metadata::parse("");
        assert_eq!(meta, Metadata::default());
    }

    /// 空 `<version></version>` 被丢弃
    #[test]
    fn empty_version_entries_are_skipped() {
        let meta =
            Metadata::parse("<versions><version></version><version>1.2</version></versions>");
        assert_eq!(meta.versions, vec!["1.2"]);
    }

    /// 未闭合的 `<version>` 不 panic，已解析的保留
    #[test]
    fn truncated_input_keeps_what_it_can() {
        let meta = Metadata::parse("<versions><version>a</version><version>b");
        assert_eq!(meta.versions, vec!["a"]);
    }

    /// 忽略上游新增字段
    #[test]
    fn ignores_unknown_tags() {
        let xml = "<metadata><groupId>g</groupId><latest>2</latest><versions><version>1</version><version>2</version></versions><newThing>x</newThing></metadata>";
        let meta = Metadata::parse(xml);
        assert_eq!(meta.latest.as_deref(), Some("2"));
        assert_eq!(meta.versions, vec!["1", "2"]);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let meta = Metadata::parse("<latest>\n  2.0  \n</latest>");
        assert_eq!(meta.latest.as_deref(), Some("2.0"));
    }

    /// MC 版本取第一个 `-` 之前
    #[test]
    fn mc_version_is_the_prefix_before_first_dash() {
        assert_eq!(mc_of("26.3-21.0.0-RC2755"), "26.3");
        assert_eq!(mc_of("1.21.1-14.0.0-beta"), "1.21.1");
        assert_eq!(mc_of("1.21.1"), "1.21.1");
        assert_eq!(mc_of(""), "");
    }
}
