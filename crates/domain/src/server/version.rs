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
}

impl ServerVersion {
    /// 按展示口径排序：新在前
    ///
    /// 判定收敛在领域层，入口不再各自排序。口径：
    ///
    /// 1. 正式版在快照、旧版之前
    /// 2. 能解析出数字的版本在快照名（`24w14a`）之类之前
    /// 3. 数字段按数值逐段比较，`1.12.2` > `1.12` > `1.10` > `1.1`
    /// 4. 同一版本号的正式形态在后缀之前（`1.21.1` 在 `1.21.1-pre1` 之前）
    ///
    /// 只排序、不去重：来源若给出重复 id，由来源自己负责修正。
    pub fn sort_display_order(versions: &mut [Self]) {
        // 先解析再排序：解析是 O(段数)，不能放进比较函数里反复做
        let keys: Vec<VersionOrderKey<'_>> = versions
            .iter()
            .map(|version| VersionOrderKey {
                kind_rank: match version.kind {
                    VersionKind::Release => 0,
                    VersionKind::Snapshot => 1,
                    VersionKind::Historical => 2,
                },
                segments: parse_segments(version.id.as_str()),
                raw: version.id.as_str(),
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
    raw: &'a str,
}

/// 版本串的一个分段
#[derive(Debug, PartialEq, Eq)]
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

        // 直接给出「谁在前」：正式版优先，然后版本号大的在前
        match self.kind_rank.cmp(&other.kind_rank) {
            Ordering::Equal => {}
            other => return other,
        }
        match (&self.segments, &other.segments) {
            // 能解析的版本排在快照名之类之前
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => return other.raw.cmp(self.raw),
            (Some(left), Some(right)) => {
                if let Some(ordering) = compare_segments(left, right) {
                    // `compare_segments` 返回「谁更旧」，这里取反成新的在前
                    return ordering.reverse();
                }
            }
        }
        // 前缀完全相同、只是一方多出段：
        // - 多出的是数字段（`1.12.2` 对 `1.12`）→ 多出者更新，在前
        // - 多出的是带前缀段（`1.21.1-pre1` 对 `1.21.1`）→ 预发布，在后
        let suffix_kind = |key: &Self, shared: usize| {
            key.segments
                .as_ref()
                .and_then(|segments| segments.get(shared))
                .map(|segment| matches!(segment, VersionSegment::Text { .. }))
        };
        let left_len = self.segments.as_ref().map_or(0, Vec::len);
        let right_len = other.segments.as_ref().map_or(0, Vec::len);
        let shared = left_len.min(right_len);
        match (suffix_kind(self, shared), suffix_kind(other, shared)) {
            // 自己多出数字段（`1.12.2` 对 `1.12`）更新，在前；对方多出预发布后缀同理
            (Some(false), None) | (None, Some(true)) => Ordering::Less,
            // 自己多出预发布后缀（`1.21.1-pre1` 对 `1.21.1`）在后；对方多出数字段同理
            (Some(true), None) | (None, Some(false)) => Ordering::Greater,
            _ => other.raw.cmp(self.raw),
        }
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
    None
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
        // `1.21.1` 与 `1.21.1-pre1` 是同一版本号，正式形态在前
        assert_eq!(
            sorted_ids(versions),
            vec!["1.21.1", "1.21.1-pre1", "24w45a", "1.0"]
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
        assert_eq!(
            sorted_ids(versions),
            vec!["26.3-21.0.0", "26.3", "26.3-21.0.0-RC2755"]
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
