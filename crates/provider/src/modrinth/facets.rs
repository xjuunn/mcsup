//! Modrinth 搜索 facet 构造
//!
//! Modrinth 的 `facets` 是一个「数组的数组」：外层数组之间是 `AND`，内层数组
//! 之间是 `OR`，单个 facet 形如 `{type} {operation} {value}`（见
//! <https://docs.modrinth.com/api/operations/searchprojects/>）。
//!
//! 例如「支持 1.16.5 或 1.17.1 的 Forge 模组」编码为：
//!
//! ```text
//! [["versions:1.16.5","versions:1.17.1"],["categories:forge"],["project_type:mod"]]
//! ```
//!
//! # 为什么单独建模而不是让调用方拼字符串
//!
//! 手拼 facet 太容易出错：漏引号、把 `AND` 误写成 `OR`、值里带空格没转义。
//! 这里把「类型 / 操作 / 值」拆成结构，由 [`FacetTree::encode`] 统一序列化，
//! 把 JSON 组装与转义收敛到一处，并为每一种组合留出可测的纯函数入口。
//!
//! 构造语义与上游一致：每次 [`FacetTree::and`] / [`FacetTree::or`] 追加一组
//! 条件。空组不参与编码——空数组会被上游当成「必须匹配空集合」，等于恒定
//! 失败，因此这里直接丢弃空组。

use std::fmt::Write as _;

/// facet 比较操作
///
/// 上游支持的比较符以等号为主；`>=` / `<=` 等用于数值（下载量、关注数）与
/// 时间戳。字符串比较默认用 `=`（等价于 `:`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetOp {
    /// 等于（`=`，等价于 `:`）
    Eq,
    /// 不等于
    NotEq,
    /// 大于等于
    Gte,
    /// 大于
    Gt,
    /// 小于等于
    Lte,
    /// 小于
    Lt,
}

impl FacetOp {
    /// 编码为上游比较符
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Gte => ">=",
            Self::Gt => ">",
            Self::Lte => "<=",
            Self::Lt => "<",
        }
    }
}

/// 单条 facet 条件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facet {
    /// facet 类型（`project_type` / `versions` / `categories` …）
    pub kind: String,
    /// 比较操作
    pub op: FacetOp,
    /// 比较值
    pub value: String,
}

impl Facet {
    /// 构造「等于」条件（最常用）
    #[must_use]
    pub fn eq(kind: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            op: FacetOp::Eq,
            value: value.into(),
        }
    }

    /// 指定比较操作的构造
    #[must_use]
    pub fn with(kind: impl Into<String>, op: FacetOp, value: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            op,
            value: value.into(),
        }
    }

    /// 编码为 `{kind}{op}{value}`
    ///
    /// 形如 `project_type=mod`、`downloads>=1000`。**不能**在操作符两侧加
    /// 空格：上游按原文做条件解析，带空格会被当成无法识别的 facet。
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}{}{}", self.kind, self.op.as_str(), self.value)
    }
}

/// facet 组：一组 OR 条件，组间为 AND
///
/// 每一组内是「任一满足即可」（OR），组与组之间是「全部满足」（AND）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FacetTree {
    groups: Vec<Vec<Facet>>,
}

impl FacetTree {
    /// 空条件树（编码为 `[]`，表示不过滤）
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一组 OR 条件（组间 AND）
    ///
    /// 空组被忽略：空 `OR` 数组在上游等价于「不可能满足」，与「不加限制」的
    /// 语义相反，直接丢弃更符合调用方预期。
    #[must_use]
    pub fn and(mut self, facets: impl IntoIterator<Item = Facet>) -> Self {
        let group: Vec<Facet> = facets.into_iter().collect();
        if !group.is_empty() {
            self.groups.push(group);
        }
        self
    }

    /// 追加一组 OR 条件的便捷入口（组间 AND）
    ///
    /// 等价于 [`Self::and`]，只是入口名字强调这是「其中一个即可」。
    #[must_use]
    pub fn or(self, facets: impl IntoIterator<Item = Facet>) -> Self {
        self.and(facets)
    }

    /// 是否没有任何条件
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// 编码为上游 `facets` 查询参数值
    ///
    /// 输出形如 `[["project_type:mod"]]`；无任何条件时返回 `[]`。
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::from("[");
        for (gi, group) in self.groups.iter().enumerate() {
            if gi > 0 {
                out.push(',');
            }
            out.push('[');
            for (fi, facet) in group.iter().enumerate() {
                if fi > 0 {
                    out.push(',');
                }
                out.push('"');
                escape_into(&facet.encode(), &mut out);
                out.push('"');
            }
            out.push(']');
        }
        out.push(']');
        out
    }
}

/// 把字符串内容按 JSON 字符串规则转义后写入
///
/// facet 的值来自用户输入（版本号、分类名），必须防止引号 / 反斜杠 / 控制
/// 字符破坏 JSON 结构。这里只处理字符串内部转义，外层引号由调用方补。
fn escape_into(raw: &str, out: &mut String) {
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // 其余控制字符走 \u00XX
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_facet_encodes() {
        let tree = FacetTree::new().and([Facet::eq("project_type", "mod")]);
        assert_eq!(tree.encode(), r#"[["project_type=mod"]]"#);
    }

    /// 组内多个条件为 OR，编码进同一内层数组
    #[test]
    fn or_group_encodes_into_one_array() {
        let tree = FacetTree::new().and([
            Facet::eq("versions", "1.16.5"),
            Facet::eq("versions", "1.17.1"),
        ]);
        assert_eq!(tree.encode(), r#"[["versions=1.16.5","versions=1.17.1"]]"#);
    }

    /// 多个组之间是 AND，编码成多个内层数组
    #[test]
    fn and_groups_encode_into_separate_arrays() {
        let tree = FacetTree::new()
            .and([Facet::eq("categories", "forge")])
            .and([Facet::eq("versions", "1.17.1")])
            .and([Facet::eq("project_type", "mod")]);
        assert_eq!(
            tree.encode(),
            r#"[["categories=forge"],["versions=1.17.1"],["project_type=mod"]]"#
        );
    }

    /// 空组被忽略：不生成恒失败的 `[]` 内层数组
    #[test]
    fn empty_groups_are_dropped() {
        let tree = FacetTree::new()
            .and(Vec::<Facet>::new())
            .and([Facet::eq("project_type", "mod")])
            .and([]);
        assert_eq!(tree.encode(), r#"[["project_type=mod"]]"#);
    }

    /// 全空条件树编码为 `[]`
    #[test]
    fn empty_tree_encodes_to_empty_array() {
        assert_eq!(FacetTree::new().encode(), "[]");
        assert!(FacetTree::new().is_empty());
    }

    /// 比较符原样进入编码
    #[test]
    fn comparison_operators_encode() {
        assert_eq!(FacetOp::Eq.as_str(), "=");
        assert_eq!(FacetOp::NotEq.as_str(), "!=");
        assert_eq!(FacetOp::Gte.as_str(), ">=");
        assert_eq!(FacetOp::Gt.as_str(), ">");
        assert_eq!(FacetOp::Lte.as_str(), "<=");
        assert_eq!(FacetOp::Lt.as_str(), "<");

        let tree = FacetTree::new().and([Facet::with("downloads", FacetOp::Gte, "1000")]);
        assert_eq!(tree.encode(), r#"[["downloads>=1000"]]"#);
    }

    /// 值里的引号与反斜杠必须被转义，产出仍是合法 JSON
    #[test]
    fn values_are_escaped() {
        let tree = FacetTree::new().and([Facet::eq("categories", "a\"b\\c")]);
        let encoded = tree.encode();
        // 必须能被 JSON 解析器接受
        let parsed: serde_json::Value = serde_json::from_str(&encoded).expect("应为合法 JSON");
        let arr = parsed.as_array().expect("外层数组");
        let inner = arr[0].as_array().expect("内层数组");
        // 解析回来应与原值一致（转义无损）
        assert_eq!(
            inner[0].as_str().expect("facet 字符串"),
            "categories=a\"b\\c"
        );
    }

    /// 控制字符按 JSON 规则转义，不破坏结构
    #[test]
    fn control_chars_are_escaped() {
        let tree = FacetTree::new().and([Facet::eq("categories", "a\nb\tc")]);
        let encoded = tree.encode();
        let parsed: serde_json::Value = serde_json::from_str(&encoded).expect("应为合法 JSON");
        assert_eq!(
            parsed[0][0].as_str().expect("facet 字符串"),
            "categories=a\nb\tc"
        );
    }
}
