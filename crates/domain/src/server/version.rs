use std::fmt;

pub use super::variant::{ServerVariant, VariantId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionId(String);

impl VersionId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionKind {
    Release,
    Snapshot,
    Historical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    pub id: VersionId,
    pub kind: VersionKind,
    pub variant: ServerVariant,
    /// Minecraft 版本要求的 Java 大版本（如 1.21.1 需要 Java 21）
    /// 来源：版本清单中的 `javaVersion.majorVersion`
    pub java_major: Option<crate::java::JavaMajor>,
    /// 发布时间，RFC 3339 UTC 原文（如 `2026-11-03T13:08:56+00:00`）
    ///
    /// 来源：`Mojang` 清单的 `releaseTime`。排序优先按它倒序，因为版本号
    /// 拆不出数字的名字（`26w14a` / `26.4-snapshot-7` / `3D Shareware v1.34`）
    /// 只能靠时间定位先后；只按版本号排会把它们整体垫到列表末尾，而它们
    /// 往往是最新的。
    ///
    /// 存原文而不是解析成时间戳：`chrono` 是 `domain` 之外的依赖，而该字段
    /// 只用于同一来源内部比较——`Mojang` 给出的格式定长且统一为 `+00:00`，
    /// 字典序即时间序，无需引入日期库。**不得**把不同来源、不同格式的时间
    /// 混在一起比较。`None` 表示来源不提供发布时间（`Fabric` / `Paper` 等）。
    pub released: Option<String>,
}

impl ServerVersion {
    /// 按展示口径排序：新在前
    ///
    /// 判定收敛在领域层，入口不再各自排序。口径：
    ///
    /// 1. 有发布时间的按时间倒序（`Mojang` 清单），版本号拆不出数字的
    ///    `26w14a` / `26.4-snapshot-7` / `3D Shareware v1.34` 靠它定位
    /// 2. 版本号大的在前（按数字段逐段比较，`1.12.2` > `1.12` > `1.10`）
    /// 3. **版本号相同时**才比 kind：正式版在快照、旧版之前
    /// 4. 同一版本号的正式形态在后缀之前（`1.21.1` 在 `1.21.1-pre1` 之前）
    ///
    /// 只排序、不去重：来源若给出重复 id，由来源自己负责修正。
    pub fn sort_display_order(versions: &mut [Self]) {
        // 先解析再排序：解析是 O(段数)，不能放进比较函数里反复做
        let keys: Vec<VersionOrderKey<'_>> = versions
            .iter()
            .map(|version| {
                let segments = parse_segments(version.id.as_str());
                let (main, prerelease) = split_prerelease(segments.as_deref());
                VersionOrderKey {
                    kind_rank: match version.kind {
                        VersionKind::Release => 0,
                        VersionKind::Snapshot => 1,
                        VersionKind::Historical => 2,
                    },
                    segments,
                    main,
                    prerelease,
                    released: version.released.as_deref(),
                    raw: version.id.as_str(),
                }
            })
            .collect();

        // 排序下标而非元素：`VersionId` 不实现 `Ord`，元素本身也无从比较。
        // `sort_by` 是稳定排序，相同 key 保持来源给出的相对顺序
        let mut order: Vec<usize> = (0..versions.len()).collect();
        order.sort_by(|left, right| keys[*left].cmp(&keys[*right]));

        let mut sorted: Vec<Self> = Vec::with_capacity(versions.len());
        for index in order {
            sorted.push(std::mem::replace(&mut versions[index], Self::placeholder()));
        }
        for (slot, version) in versions.iter_mut().zip(sorted) {
            *slot = version;
        }
    }

    /// 搬移期间的占位值：每次都被 `sorted` 里真正的元素覆盖，不会外泄
    fn placeholder() -> Self {
        Self {
            id: VersionId::new(String::new()),
            kind: VersionKind::Release,
            variant: ServerVariant::Vanilla,
            java_major: None,
            released: None,
        }
    }
}

/// 版本列表的展示顺序：从新到旧
///
/// 各来源（`Mojang` manifest、Fabric meta、`Forge` / `NeoForge` 的 `BTreeMap`）
/// 给出的顺序各不相同：`BTreeMap` 按串字典序，`1.10` 会排在 `1.1` 之后。入口不能
/// 自己各排一遍，因此顺序收敛在这里，由 [`ServerVersion::sort_display_order`] 应用。
///
/// 排序口径（比较函数直接返回「谁在前」，调用方不需要再反转）：
///
/// 1. **正式版优先**：`Release` 在 `Snapshot` 在 `Historical` 之前，用户默认
///    看到的是能用的版本，扩展候选排在后面。
/// 2. **能解析的版本在前**：快照名（`24w14a`）等拆不出数字的版本串排在所有
///    能解析的版本之后，并按字典序相互比较。
/// 3. **数字段逐段比较**：`1.12.2` > `1.12` > `1.10` > `1.1`，`1.21.1` >
///    `1.21`，而不是串字典序。
/// 4. **同一版本线上正式版在前**：`26.3` 在 `26.3-21.0.0` 之前，`26.3-21.0.0`
///    在 `26.3-21.0.0-RC2755` 之前；`1.21.1` 在 `1.21.1-pre1` 之前。
#[derive(Debug, PartialEq, Eq)]
struct VersionOrderKey<'a> {
    kind_rank: u8,
    /// `None` 表示版本串拆不出数字段，只能按 `raw` 字面比较
    segments: Option<Vec<VersionSegment<'a>>>,
    /// 主路径：`segments` 里第一个带前缀段（`pre` / `rc` / `snapshot` …）之前
    /// 的部分。`1.21-pre1` → `[1, 21]`，`1.21.4-rc1` → `[1, 21, 4]`。
    ///
    /// 排序先看主路径，再看待有预发布后缀：这样 `1.21-pre1`（主路径 `1.21`）
    /// 落在 `1.21.4-rc1`（主路径 `1.21.4`）之前，而 `1.21` 正式版又排在
    /// `1.21-pre1` 之前。若直接用整段比较，「多出数字段更新」与「预发布在
    /// 后」两条规则会互相矛盾，比较函数退化为非全序，`sort_by` 会 panic。
    main: Option<Vec<VersionSegment<'a>>>,
    /// 预发布 / 变体后缀（首个带前缀段及其后全部段）
    prerelease: Vec<VersionSegment<'a>>,
    /// 发布时间原文（RFC 3339 UTC）；`None` = 来源不提供
    released: Option<&'a str>,
    raw: &'a str,
}

/// 版本串的一个分段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionSegment<'a> {
    /// 纯数字段（`1` / `21` / `2755`），按数值比较
    Number(u64),
    /// 前缀 + 数字（`RC2755` / `pre1`），前缀相同则按数字比较，因此
    /// `pre2` 在 `pre10` 之前
    Text { prefix: &'a str, number: u64 },
}

impl PartialOrd for VersionOrderKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VersionOrderKey<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        // 排序口径（新在前），逐级判定，任一级分出胜负即返回：
        // 0. **发布时间晚的在前**——版本号拆不出数字的名字（`26w14a` /
        //    `26.4-snapshot-7` / `3D Shareware v1.34`）只能靠时间定位，
        //    否则会整体垫到列表末尾，而它们往往是最新的
        // 1. 能解析的版本排在拆不出的名字之前
        // 2. 主路径（去掉预发布后缀的版本号）大的在前：`1.21.4` > `1.21`
        // 3. 主路径相同：正式版（无预发布后缀）在有后缀的之前
        // 4. 都有后缀：按后缀段比较（`pre2` < `pre10`，`rc` 在 `pre` 前）
        // 5. 仍相同：按 kind（正式版在快照 / 旧版之前），最后按字面倒序
        //
        // 这一顺序是全序：每一级都是对固定元组的逐位比较，不存在两条路径
        // 给出相反结论的情况。kind 只在版本号完全相同时才起作用——若让
        // kind 优先，`Mojang` 清单里 758 个快照会整体垫到 103 个正式版之后，
        // 「全部」视图前 100 多项全是正式版，用户翻不到快照（`Ctrl+A` 看起来
        // 没生效）。
        //
        // 发布时间只在**同一来源内部**可比：`released` 是来源给的原文，
        // 格式由来源保证一致（见 [`ServerVersion::released`]）。一方有、
        // 一方没有时把有时间的排前面——有时间说明来源给出了权威先后，
        // 比版本号字面更可信。
        match (self.released, other.released) {
            (Some(left), Some(right)) => {
                if left != right {
                    // 时间倒序：晚的在前
                    return right.cmp(left);
                }
            }
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => {}
        }

        match (&self.main, &other.main) {
            (Some(left), Some(right)) => {
                match compare_segments(left, right) {
                    // `compare_segments` 返回「谁更旧」，取反成新的在前
                    Some(ordering) if ordering.is_ne() => return ordering.reverse(),
                    _ => {}
                }
            }
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => {}
        }

        // 主路径相同：正式版先于预发布
        let self_pre = !self.prerelease.is_empty();
        let other_pre = !other.prerelease.is_empty();
        if self_pre != other_pre {
            return if self_pre {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        // 都有预发布后缀：比较后缀段。`compare_segments` 在这里返回的就是
        // 「谁在前」（`prefix_rank` 已把 rc / pre / snapshot 按「越接近正式版
        // 越新」排好，数字段也是数值大在前），**不能** reverse。
        if self_pre
            && let Some(ordering) = compare_segments(&self.prerelease, &other.prerelease)
            && ordering.is_ne()
        {
            return ordering;
        }

        // 版本号完全并列：kind 决胜，再按字面倒序
        match self.kind_rank.cmp(&other.kind_rank) {
            Ordering::Equal => other.raw.cmp(self.raw),
            other => other,
        }
    }
}

/// 把解析后的段切成「主路径」与「预发布后缀」
///
/// 主路径取第一个带前缀段之前的部分；找不到带前缀段时后缀为空、主路径即全部。
/// `parse_segments` 返回 `None` 时两者都是 `None` / 空。
fn split_prerelease<'a>(
    segments: Option<&[VersionSegment<'a>]>,
) -> (Option<Vec<VersionSegment<'a>>>, Vec<VersionSegment<'a>>) {
    let Some(segments) = segments else {
        return (None, Vec::new());
    };
    let first_text = segments
        .iter()
        .position(|segment| matches!(segment, VersionSegment::Text { .. }));
    match first_text {
        Some(index) => {
            let main = segments[..index].to_vec();
            // 首段就是带前缀段（`b1.8` / `a1.2` 这类 beta / alpha 历史版，或
            // 纯 `pre1`）：没有数字主路径可比，视为「排在任何数字版本之后」。
            // 返回 `None` 让排序把它归到不可解析一档，内部再按 kind + 字面排。
            if main.is_empty() {
                (None, segments.to_vec())
            } else {
                (Some(main), segments[index..].to_vec())
            }
        }
        None => (Some(segments.to_vec()), Vec::new()),
    }
}

/// 逐段比较；第一个不相等的段决定结果，全部相等时返回 `None`
///
/// 比较只到两串共有段为止；一方更短（`26.3` 对 `26.3-21.0.0`）时由调用方按
/// 「短者更旧」定序，因此这里不看后缀。
///
/// 数字段与带前缀段相比时，数字段更旧：这是「正式版在预发布之前」的来源。
fn compare_segments(
    left: &[VersionSegment<'_>],
    right: &[VersionSegment<'_>],
) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;

    for (l, r) in left.iter().zip(right) {
        let ordering = match (l, r) {
            (VersionSegment::Number(a), VersionSegment::Number(b)) => a.cmp(b),
            (VersionSegment::Number(_), VersionSegment::Text { .. }) => Ordering::Less,
            (VersionSegment::Text { .. }, VersionSegment::Number(_)) => Ordering::Greater,
            (
                VersionSegment::Text {
                    prefix: a,
                    number: a_num,
                },
                VersionSegment::Text {
                    prefix: b,
                    number: b_num,
                },
            ) => prefix_rank(a)
                .cmp(&prefix_rank(b))
                .then_with(|| a_num.cmp(b_num)),
        };
        if ordering.is_ne() {
            return Some(ordering);
        }
    }
    // 共享段全部相等：按长度决胜（短者更旧）。
    //
    // 主路径里不会有带前缀段（都被切进 prerelease 了），所以「更长」等于
    // 「多出数字段」→ 更长的版本更新；预发布后缀同理（`pre` ⊂ `pre.1`，
    // 后者是更细的变体，更新）。长度相同返回 `None` 交给调用方的 kind 决胜。
    match left.len().cmp(&right.len()) {
        Ordering::Equal => None,
        // 谁更短谁更旧
        Ordering::Less => Some(Ordering::Less),
        Ordering::Greater => Some(Ordering::Greater),
    }
}

/// 后缀前缀的稳定性排序：常见的预发布前缀按「越接近正式版越新」排列
///
/// 未知前缀排在已知前缀之后，并相互按字面比较，避免出现不可预测的相对顺序。
fn prefix_rank(prefix: &str) -> (u8, &str) {
    let rank = match prefix.to_ascii_lowercase().as_str() {
        "rc" => 0,
        "pre" | "pre-release" | "prerelease" => 1,
        "snapshot" => 2,
        "beta" => 3,
        "alpha" => 4,
        _ => 5,
    };
    (rank, prefix)
}

/// 解析版本串为可比较的各段；拆不出数字的版本串返回 `None`
///
/// 规则：
///
/// - 纯数字段（`1` / `21` / `2755`）→ [`VersionSegment::Number`]
/// - 带数字前缀的段（`RC2755` → `RC`、`2755`；`pre1` → `pre`、`1`）
/// - 快照名（`24w14a`、`1.21.1-pre-release`）→ `None`，调用方按字面比较
///
/// `-` 与 `.` 都视为段分隔符：`26.3-21.0.0-RC2755` 与 `1.21.1-pre1` 同属一种
/// 形状，不需要为 Sponge 的「MC 段 + 实现段」单独开分支。
fn parse_segments(raw: &str) -> Option<Vec<VersionSegment<'_>>> {
    let mut segments = Vec::new();
    for piece in raw.split(['.', '-']).filter(|piece| !piece.is_empty()) {
        if let Ok(number) = piece.parse::<u64>() {
            segments.push(VersionSegment::Number(number));
            continue;
        }
        // 非数字前缀后面必须紧跟数字，否则整串无法排序
        let split = piece.find(|ch: char| ch.is_ascii_digit())?;
        let number = piece[split..].parse::<u64>().ok()?;
        segments.push(VersionSegment::Text {
            prefix: &piece[..split],
            number,
        });
    }
    (!segments.is_empty()).then_some(segments)
}

/// 按 Minecraft 版本推断所需的 Java 大版本
///
/// 原版来源从版本清单的 `javaVersion.majorVersion` 直接读到，用不到这个；但
/// **`Forge` / `NeoForge` 的 Maven 元数据里没有 Java 要求**，它只发布 installer。
/// 不给出版本就只能挑「本机最新 Java」，用 JDK 26 跑 1.16 的 Forge 会直接崩，
/// 用户看到的是一串 class 版本错误而不是「缺 Java 8」。
///
/// 分档依据是 Mojang 各版本的官方要求（写入 `version.json` 的最低 Java）：
///
/// | MC 版本 | Java |
/// | --- | --- |
/// | 1.20.5 及以上 | 21 |
/// | 1.18 – 1.20.4 | 17 |
/// | 1.17 | 16 |
/// | 1.12 – 1.16.5 | 8 |
/// | 1.6.4 – 1.11.2 | 8（同样在 Java 8 上运行） |
///
/// 解析不出主次版本号时返回 `None`，由调用方退回自己的兜底口径。
///
/// 只认 `主.次` 两段：快照版（如 `24w14a`）与未发布版一律返回 `None`。
#[must_use]
pub fn required_java_major(mc: &str) -> Option<crate::java::JavaMajor> {
    let mut parts = mc.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    if major != 1 {
        // 1.x 之外没有已知的 MC 版本命名，无法判断
        return None;
    }
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);

    let java = match minor {
        // 1.20.5 起要求 Java 21
        20 if patch >= 5 => 21,
        // 1.21 及之后
        21.. => 21,
        // 1.18 – 1.20.4
        18..=20 => 17,
        // 1.17
        17 => 16,
        // 1.16 及以下（Forge 从 1.6.4 开始有服务端安装器）
        5..=16 => 8,
        _ => return None,
    };
    crate::java::JavaMajor::parse(i64::from(java)).ok()
}

impl ServerVariant {
    /// 解析为**内置**变体
    ///
    /// 只认内置集合：用于 `--variant` 这类需要尽早报错的入口。第三方变体不经
    /// 此路径校验，而是由组合根已注册的来源集合决定（见
    /// [`Self::from_id`]）。
    ///
    /// # Errors
    ///
    /// 名称不在内置集合内时返回 [`super::error::UnknownVariant`]
    pub fn parse(raw: &str) -> Result<Self, super::error::UnknownVariant> {
        Self::known(raw).ok_or_else(|| super::error::UnknownVariant {
            raw: raw.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::required_java_major;
    use super::{ServerVariant, ServerVersion, VersionId, VersionKind};

    fn version(kind: VersionKind, id: &str) -> ServerVersion {
        ServerVersion {
            id: VersionId::new(id),
            kind,
            variant: ServerVariant::Vanilla,
            java_major: None,
            released: None,
        }
    }

    /// 带发布时间的版本（`Mojang` 清单形态）
    fn dated(kind: VersionKind, id: &str, released: &str) -> ServerVersion {
        ServerVersion {
            released: Some(released.to_owned()),
            ..version(kind, id)
        }
    }

    /// 从 Forge 的实际输出顺序出发：数值必须按段比较，而不是串字典序
    fn sorted_ids(mut versions: Vec<ServerVersion>) -> Vec<String> {
        ServerVersion::sort_display_order(&mut versions);
        versions.into_iter().map(|v| v.id.to_string()).collect()
    }

    #[test]
    fn display_order_compares_numeric_segments_by_value() {
        // Forge 的 BTreeMap 给出串字典序，1.10 会落在 1.1 之后
        let mut ids = [
            "1.1", "1.10", "1.10.2", "1.11", "1.11.2", "1.12", "1.12.1", "1.12.2", "1.13.2",
            "1.14.2",
        ];
        ids.sort_unstable(); // 还原上游顺序
        let versions = ids
            .into_iter()
            .map(|id| version(VersionKind::Release, id))
            .collect();
        assert_eq!(
            sorted_ids(versions),
            vec![
                "1.14.2", "1.13.2", "1.12.2", "1.12.1", "1.12", "1.11.2", "1.11", "1.10.2", "1.10",
                "1.1",
            ]
        );
    }

    #[test]
    fn display_order_puts_release_before_snapshot_before_historical() {
        let versions = vec![
            version(VersionKind::Historical, "1.0"),
            version(VersionKind::Snapshot, "24w45a"),
            version(VersionKind::Release, "1.21.1"),
            version(VersionKind::Release, "1.21.1-pre1"),
        ];
        // 主路径大的在前：`1.21.1` 系列最新；同主路径时正式形态在预发布前。
        // 可解析出数字序列的 `1.0` 排在拆不出的快照名 `24w45a` 之前。
        assert_eq!(
            sorted_ids(versions),
            vec!["1.21.1", "1.21.1-pre1", "1.0", "24w45a"]
        );
    }

    #[test]
    fn display_order_keeps_unparsable_ids_after_numeric_ones() {
        // 快照名解析不出数字，退化为字面比较，且整体排在数字版本之后
        let versions = vec![
            version(VersionKind::Snapshot, "24w14a"),
            version(VersionKind::Snapshot, "24w45a"),
            version(VersionKind::Snapshot, "1.21.1-pre1"),
        ];
        assert_eq!(
            sorted_ids(versions),
            vec!["1.21.1-pre1", "24w45a", "24w14a"]
        );
    }

    #[test]
    fn display_order_ranks_prerelease_suffix_after_release_of_same_line() {
        // Sponge：`26.3-21.0.0` 与 `26.3-21.0.0-RC2755` 是同一实现版本，
        // RC 是预发布，排在正式版之后
        let versions = vec![
            version(VersionKind::Snapshot, "26.3-21.0.0-RC2755"),
            version(VersionKind::Release, "26.3-21.0.0"),
            version(VersionKind::Release, "26.3"),
        ];
        // 主路径 `26.3-21.0.0` 最长最新；RC 是其预发布，次之；只到
        // `26.3` 的串最短最旧
        assert_eq!(
            sorted_ids(versions),
            vec!["26.3-21.0.0", "26.3-21.0.0-RC2755", "26.3"]
        );
    }

    /// 回归：有发布时间的按时间倒序，拆不出数字的名字不能垫底
    ///
    /// `26.4-snapshot-7` / `26w14a` / `3D Shareware v1.34` 都解析不出数字
    /// 版本号，只按版本号排会整体落到列表末尾，而它们恰恰是最新的。
    #[test]
    fn release_time_orders_unparsable_names_by_recency() {
        let versions = vec![
            dated(
                VersionKind::Historical,
                "3D Shareware v1.34",
                "2019-04-01T00:00:00+00:00",
            ),
            dated(VersionKind::Release, "1.21.11", "2025-12-09T12:00:00+00:00"),
            dated(
                VersionKind::Snapshot,
                "26.4-snapshot-7",
                "2026-11-03T13:08:56+00:00",
            ),
            dated(VersionKind::Snapshot, "26w14a", "2026-04-07T12:00:00+00:00"),
        ];
        assert_eq!(
            sorted_ids(versions),
            vec!["26.4-snapshot-7", "26w14a", "1.21.11", "3D Shareware v1.34"],
            "按发布时间倒序，与版本号能否解析无关"
        );
    }

    /// 有发布时间的排在没时间的之前；两边都没时间才比版本号
    #[test]
    fn release_time_wins_over_missing_time() {
        let versions = vec![
            version(VersionKind::Release, "1.21.11"),
            dated(VersionKind::Snapshot, "26w14a", "2026-04-07T12:00:00+00:00"),
        ];
        assert_eq!(sorted_ids(versions), vec!["26w14a", "1.21.11"]);

        // 都没时间时退回版本号比较（`1.21.11` > `1.20.1`）
        let undated = vec![
            version(VersionKind::Release, "1.20.1"),
            version(VersionKind::Release, "1.21.11"),
        ];
        assert_eq!(sorted_ids(undated), vec!["1.21.11", "1.20.1"]);
    }

    /// 同一时间戳仍要满足全序，不能因 `Equal` 破坏传递性
    #[test]
    fn equal_release_time_falls_back_to_version_order() {
        let same = "2025-06-17T12:00:00+00:00";
        let versions = vec![
            dated(VersionKind::Release, "1.21.1", same),
            dated(VersionKind::Release, "1.21.2", same),
            dated(VersionKind::Snapshot, "24w45a", same),
        ];
        let sorted = sorted_ids(versions);
        assert_eq!(sorted[0], "1.21.2", "同时间按版本号大的在前: {sorted:?}");
        assert_eq!(sorted[1], "1.21.1", "同时间按版本号大的在前: {sorted:?}");
    }

    /// 回归：快照 / 预发布必须与正式版按版本号穿插，不能整体垫底
    ///
    /// 真实清单里快照 758 个、正式版 103 个。旧口径「kind 优先」会让「全部」
    /// 视图前 100 多项全是正式版，`Ctrl+A` 后翻不到任何预发布版。修复后
    /// `1.21.11` 之后紧跟它的 pre / rc 快照，再往下才是 `1.21.10`。
    #[test]
    fn snapshots_interleave_with_releases_by_version_number() {
        let versions = vec![
            version(VersionKind::Release, "1.21.10"),
            version(VersionKind::Release, "1.21.11"),
            version(VersionKind::Snapshot, "1.21.11-pre1"),
            version(VersionKind::Snapshot, "1.21.11-rc1"),
        ];
        assert_eq!(
            sorted_ids(versions),
            vec!["1.21.11", "1.21.11-rc1", "1.21.11-pre1", "1.21.10"],
            "预发布紧跟在同版本号正式版之后，再排下一个正式版"
        );
    }

    /// 回归：混合 kind / 预发布 / 历史版时比较函数必须满足全序
    ///
    /// 非全序会让 `sort_by` panic（user-provided comparison function does not
    /// correctly implement a total order）。这里只验证不 panic 且新版本在最前。
    #[test]
    fn comparison_is_a_total_order_across_mixed_kinds() {
        let versions = vec![
            version(VersionKind::Release, "1.21.1"),
            version(VersionKind::Snapshot, "1.21.1"),
            version(VersionKind::Historical, "1.21.1"),
            version(VersionKind::Snapshot, "1.21.1-pre1"),
            version(VersionKind::Release, "1.21"),
            version(VersionKind::Snapshot, "24w45a"),
            version(VersionKind::Historical, "b1.8"),
            version(VersionKind::Historical, "1.0"),
        ];
        let sorted = sorted_ids(versions);
        assert_eq!(sorted[0], "1.21.1", "最新正式版在最前，实际: {sorted:?}");
        // 同号正式 / 快照的 id 相同，无法只按字符串区分；这里验证预发布
        // 落在所有 `1.21.1` 之后、`1.21` 之前
        let pre = sorted.iter().position(|s| s == "1.21.1-pre1").unwrap();
        let short = sorted.iter().position(|s| s == "1.21").unwrap();
        assert!(
            pre < short,
            "同号预发布要排在 `1.21` 之前，实际: {sorted:?}"
        );
    }

    #[test]
    fn display_order_is_stable_for_equal_keys() {
        // 同一 key 保持原始相对顺序，避免同一版本的重复项被随机打乱
        let versions = vec![
            version(VersionKind::Release, "1.21.1"),
            version(VersionKind::Release, "1.21.1"),
        ];
        assert_eq!(sorted_ids(versions), vec!["1.21.1", "1.21.1"]);
    }

    fn java(mc: &str) -> Option<i16> {
        required_java_major(mc).map(crate::java::JavaMajor::as_i16)
    }

    #[test]
    fn modern_versions_need_java_21() {
        assert_eq!(java("1.21.1"), Some(21));
        assert_eq!(java("1.21"), Some(21));
        assert_eq!(java("1.20.5"), Some(21));
        assert_eq!(java("1.20.6"), Some(21));
    }

    #[test]
    fn one_twenty_before_five_needs_java_17() {
        assert_eq!(java("1.20.4"), Some(17));
        assert_eq!(java("1.20"), Some(17));
        assert_eq!(java("1.18"), Some(17));
        assert_eq!(java("1.18.2"), Some(17));
    }

    #[test]
    fn one_seventeen_needs_java_16() {
        assert_eq!(java("1.17"), Some(16));
        assert_eq!(java("1.17.1"), Some(16));
    }

    #[test]
    fn legacy_versions_need_java_8() {
        assert_eq!(java("1.16.5"), Some(8));
        assert_eq!(java("1.12.2"), Some(8));
        assert_eq!(java("1.6.4"), Some(8));
    }

    #[test]
    fn unknown_shapes_have_no_answer() {
        assert_eq!(java("24w14a"), None);
        assert_eq!(java("latest"), None);
        assert_eq!(java(""), None);
        assert_eq!(java("1"), None);
        assert_eq!(java("2.0"), None);
        // 早于 Forge 服务端安装器的版本不猜
        assert_eq!(java("1.2.5"), None);
    }
}
