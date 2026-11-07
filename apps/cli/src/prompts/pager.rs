//! 结果列表的翻页项
//!
//! 两个向导共用：`server create --variant modrinth-modpack` 的整合包检索与
//! `server mod add` 的 mod 检索。同一份实现保证两处的翻页手感一致——文案、
//! 出现条件、排列顺序都只有一处定义，改一处不会让另一处落后。
//!
//! 只做「往列表末尾接两项」，不认识网络也不认识业务类型。

use crate::prompts::ScopedChoice;

/// 下一页的列表项文案
///
/// 前缀是箭头，不可能出现在项目标题里，因此翻页项不会与真实命中重名——
/// 调用方按标签反查命中项时不会误判。
pub(crate) const NEXT: &str = "→ 下一页";

/// 上一页的列表项文案
pub(crate) const PREV: &str = "← 上一页";

/// 给一页结果接上翻页项：命中项在前，上一页在上、下一页在下
///
/// 上一页放在列表末尾之前、下一页放在最末：返回上一页更贴近刚才看过的
/// 列表，继续向下翻页放在最远处。是否出现只由 `offset + len` 与 `total`
/// 决定，与网络无关，因此是纯函数，可以单独测试。
pub(crate) fn with_pagers(
    mut choices: Vec<ScopedChoice>,
    offset: u32,
    total: u64,
    page: u32,
) -> Vec<ScopedChoice> {
    let more = u64::from(offset) + choices.len() as u64;
    if page > 0 {
        choices.push(ScopedChoice::new(PREV, false));
    }
    if more < total {
        choices.push(ScopedChoice::new(NEXT, false));
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(n: usize) -> Vec<ScopedChoice> {
        (0..n)
            .map(|i| ScopedChoice::new(format!("hit{i}"), false))
            .collect()
    }

    fn labels(choices: &[ScopedChoice]) -> Vec<&str> {
        choices.iter().map(|choice| choice.label.as_str()).collect()
    }

    /// 一页装得下就没有翻页项
    #[test]
    fn no_pager_when_everything_fits() {
        let choices = with_pagers(hits(20), 0, 20, 0);
        assert_eq!(choices.len(), 20);
        assert!(!labels(&choices).contains(&NEXT));
        assert!(!labels(&choices).contains(&PREV));
    }

    /// 首页只有下一页；末页只有上一页
    #[test]
    fn pager_follows_position_in_the_result_set() {
        let first_choices = with_pagers(hits(20), 0, 42, 0);
        let first = labels(&first_choices);
        assert_eq!(first[first.len() - 1], NEXT, "首页末尾是下一页");
        assert!(!first.contains(&PREV), "首页没有上一页");

        let last_choices = with_pagers(hits(2), 40, 42, 2);
        let last = labels(&last_choices);
        assert_eq!(last[last.len() - 1], PREV, "末页只有上一页");
        assert!(!last.contains(&NEXT), "末页没有下一页");
    }

    /// 中间页两项都在：上一页在上、下一页在下
    #[test]
    fn middle_page_has_both_in_reading_order() {
        let middle_choices = with_pagers(hits(20), 20, 42, 1);
        let middle = labels(&middle_choices);
        assert_eq!(middle[middle.len() - 2], PREV, "上一页在上");
        assert_eq!(middle[middle.len() - 1], NEXT, "下一页在下");
    }

    /// 空结果不生成翻页项：0 条时整页就是空的，只留 Esc 返回上一级
    #[test]
    fn empty_page_has_no_pager() {
        assert!(with_pagers(Vec::new(), 0, 0, 0).is_empty());
    }
}
