//! 入口类初始化链的客户端类引用
//!
//! 判定「服务端一启动就崩」的强信号。锚点是 `@Mod` 入口类：它在专用服务端上
//! 一定会被实例化，所以它的构造与静态初始化必然执行。沿同 jar 内的调用追踪，
//! 任何一跳无条件引用客户端专属类（`net/minecraft/client/` 等），Forge 的
//! `RuntimeDistCleaner` 都会在类加载时拒绝，表现为 `ExceptionInInitializerError`，
//! 服务端连就绪标记都到不了。
//!
//! # 为什么只从 `@Mod` 入口出发
//!
//! 全 jar 扫 `<clinit>` 会把客户端专用类全部算进来：CreativeCore 的
//! `client/render/*`、`common/gui/style/GuiStyle` 都有 `<clinit>` 引用
//! `Minecraft`，但专用服务端从不加载这些类，mod 本身工作正常。实测 61 个
//! Forge mod 里，全量扫会误报 5 个，从入口出发只有真正会崩的 `ReplayMod` 命中。
//!
//! # 为什么整条路径上出现 dist 判断就放弃
//!
//! Forge 的服务端 mod 引用客户端类是常态，通常包在 dist 分支里。判断不仅认
//! `FMLEnvironment.dist` / `DistExecutor` / `Dist` 这些 Forge 自己的类型，
//! 也认包装在平台工具类与静态字段里的等价判断（`Platform.isPhysicalClient()`、
//! `isDedicatedServer` 字段等）——实测 Kiwi、NBTac 正是靠这两类包装避免在
//! 专用服务端执行客户端代码。只要路径上出现任何一处这样的判断，就无法静态
//! 证明客户端类会被执行，按「判不出」处理，不升级结论。
//!
//! 这是启发式：能证明「入口的初始化路径里确实有这么一条」，不能证明中间
//! 没有任何运行时分支绕开。因此证据写清完整调用链，供人工核对。

use crate::bytecode;
use crate::classfile::{ClassFile, Method};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

/// 追踪上限：防止恶意 / 异常 jar 制造巨大调用图
const MAX_VISITED: usize = 512;

/// 证据里最多展示几跳
const MAX_CHAIN: usize = 4;

/// 一条「入口初始化必然触及客户端类」的现场
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitClientAccess {
    /// `@Mod` 入口类（内部名）
    pub(crate) entry: String,
    /// 从入口方法到引用点的方法链（`Class.method`）
    pub(crate) chain: Vec<String>,
    /// 被无条件引用的客户端类型（内部名）
    pub(crate) client_type: String,
}

impl InitClientAccess {
    /// 证据用的调用链文本
    #[must_use]
    pub(crate) fn chain_text(&self) -> String {
        let head = self
            .chain
            .iter()
            .take(MAX_CHAIN)
            .cloned()
            .collect::<Vec<_>>()
            .join(" → ");
        let rest = self.chain.len().saturating_sub(MAX_CHAIN);
        if rest == 0 {
            head
        } else {
            format!("{head} → …（还有 {rest} 跳）")
        }
    }
}

/// 方法键：类名 + 方法名 + 描述符
type Key<'a> = (&'a str, &'a str, &'a str);

/// 找第一条无条件触及客户端类的入口初始化链
///
/// 按入口类名排序后逐个尝试，保证同一批文件每次得到的结果一致。
#[must_use]
pub(crate) fn find(classes: &[ClassFile]) -> Option<InitClientAccess> {
    let index: HashMap<&str, &ClassFile> = classes
        .iter()
        .map(|class| (class.name.as_str(), class))
        .collect();
    let mut entries = classes
        .iter()
        .filter(|class| class.mod_entry && !class.mod_client_only)
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries.into_iter().find_map(|entry| walk(entry, &index))
}

fn walk<'a>(
    entry: &'a ClassFile,
    index: &HashMap<&'a str, &'a ClassFile>,
) -> Option<InitClientAccess> {
    let mut queue: VecDeque<Key<'a>> = VecDeque::new();
    let mut parent: HashMap<Key<'a>, Option<Key<'a>>> = HashMap::new();
    for method in entry.init_roots() {
        let key = (
            entry.name.as_str(),
            method.name.as_str(),
            method.descriptor.as_str(),
        );
        if let Entry::Vacant(slot) = parent.entry(key) {
            slot.insert(None);
            queue.push_back(key);
        }
    }

    let mut visited = 0usize;
    let mut seen = HashSet::new();
    while let Some(key) = queue.pop_front() {
        if !seen.insert(key) {
            continue;
        }
        visited += 1;
        if visited > MAX_VISITED {
            return None;
        }
        let method = method_at(index, key)?;
        if let Some(client_type) = method
            .class_refs
            .iter()
            .find(|name| bytecode::is_client_class_name(name))
            && !path_guarded(index, &parent, key)
        {
            return Some(InitClientAccess {
                entry: entry.name.clone(),
                chain: reconstruct(&parent, key),
                client_type: client_type.clone(),
            });
        }
        for call in &method.calls {
            let Some(owner) = index.get(call.owner.as_str()).copied() else {
                continue;
            };
            let Some(target) = owner.methods.iter().find(|candidate| {
                candidate.name == call.name && candidate.descriptor == call.descriptor
            }) else {
                continue;
            };
            let child = (
                owner.name.as_str(),
                target.name.as_str(),
                target.descriptor.as_str(),
            );
            if let Entry::Vacant(slot) = parent.entry(child) {
                slot.insert(Some(key));
                queue.push_back(child);
            }
        }
    }
    None
}

fn method_at<'a>(index: &HashMap<&'a str, &'a ClassFile>, key: Key<'a>) -> Option<&'a Method> {
    index
        .get(key.0)
        .copied()?
        .methods
        .iter()
        .find(|method| method.name == key.1 && method.descriptor == key.2)
}

/// 方法里是否出现客 / 服判断
///
/// 两条都查：`class_refs` 覆盖直接读的 dist 类型，`calls` 覆盖包装在平台
/// 工具类 / 静态字段里的判断（`Platform.isPhysicalClient()`、
/// `isDedicatedServer` 这类）。
fn method_guarded(method: &Method) -> bool {
    method
        .class_refs
        .iter()
        .any(|name| bytecode::is_guard_type(name))
        || method
            .calls
            .iter()
            .any(|call| bytecode::is_guard_call(&call.owner, &call.name))
}

/// 从入口到引用点的整条路径上是否出现过 dist 判断
fn path_guarded<'a>(
    index: &HashMap<&'a str, &'a ClassFile>,
    parent: &HashMap<Key<'a>, Option<Key<'a>>>,
    key: Key<'a>,
) -> bool {
    let mut current = Some(key);
    while let Some(node) = current {
        if let Some(method) = method_at(index, node)
            && method_guarded(method)
        {
            return true;
        }
        current = parent.get(&node).copied().flatten();
    }
    false
}

/// 从入口方法沿父指针还原调用链
fn reconstruct<'a>(parent: &HashMap<Key<'a>, Option<Key<'a>>>, key: Key<'a>) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = Some(key);
    while let Some(node) = current {
        path.push(format!("{}.{}", node.0, node.1));
        current = parent.get(&node).copied().flatten();
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::find;
    use crate::classfile::{Call, ClassFile, Method};

    fn method(name: &str, refs: &[&str], calls: &[(&str, &str, &str)]) -> Method {
        Method {
            name: name.to_owned(),
            descriptor: "()V".to_owned(),
            class_refs: refs.iter().map(|name| (*name).to_owned()).collect(),
            calls: calls
                .iter()
                .map(|(owner, name, descriptor)| Call {
                    owner: (*owner).to_owned(),
                    name: (*name).to_owned(),
                    descriptor: (*descriptor).to_owned(),
                })
                .collect(),
        }
    }

    fn class(name: &str, mod_entry: bool, methods: Vec<Method>) -> ClassFile {
        ClassFile {
            name: name.to_owned(),
            mod_entry,
            mod_client_only: false,
            methods,
        }
    }

    /// 构造一个 `@Mod(dist = CLIENT)` 入口：专用服务端不装载，不能当追踪根
    fn client_only_class(name: &str, methods: Vec<Method>) -> ClassFile {
        ClassFile {
            name: name.to_owned(),
            mod_entry: true,
            mod_client_only: true,
            methods,
        }
    }

    /// 现场形态：入口 `<init>` → helper 静态方法 → 客户端类
    #[test]
    fn flags_unconditional_client_access_from_entry() {
        let classes = vec![
            class(
                "com/example/Entry",
                true,
                vec![method(
                    "<init>",
                    &[],
                    &[("com/example/Helper", "bootstrap", "()V")],
                )],
            ),
            class(
                "com/example/Helper",
                false,
                vec![method(
                    "bootstrap",
                    &["net/minecraft/client/Minecraft"],
                    &[],
                )],
            ),
        ];
        let found = find(&classes).expect("入口初始化链触及客户端类");
        assert_eq!(found.entry, "com/example/Entry");
        assert_eq!(found.client_type, "net/minecraft/client/Minecraft");
        assert_eq!(
            found.chain,
            vec!["com/example/Entry.<init>", "com/example/Helper.bootstrap"]
        );
    }

    /// dist 判断在路径上：放弃升级，交人工确认
    #[test]
    fn dist_guard_prevents_flagging() {
        let classes = vec![
            class(
                "com/example/Entry",
                true,
                vec![method(
                    "<clinit>",
                    &[],
                    &[("com/example/Helper", "onClient", "()V")],
                )],
            ),
            class(
                "com/example/Helper",
                false,
                vec![method(
                    "onClient",
                    &[
                        "net/minecraftforge/fml/loading/FMLEnvironment",
                        "net/minecraft/client/Minecraft",
                    ],
                    &[],
                )],
            ),
        ];
        assert!(find(&classes).is_none());
    }

    /// 包装在平台工具类里的 dist 判断同样放弃升级
    ///
    /// 现场：Kiwi 的 `Kiwi.<init>` 用 `Platform.isPhysicalClient()` 分支包住
    /// `RenderType` 写入，属主不在 Forge dist 白名单里。
    #[test]
    fn wrapped_dist_guard_prevents_flagging() {
        let classes = vec![class(
            "com/example/Entry",
            true,
            vec![method(
                "<init>",
                &["net/minecraft/client/renderer/RenderType"],
                &[("snownee/kiwi/loader/Platform", "isPhysicalClient", "()Z")],
            )],
        )];
        assert!(find(&classes).is_none());
    }

    /// 静态布尔字段当 guard：NBTac 读 `isDedicatedServer`，为真直接返回
    #[test]
    fn static_side_flag_field_prevents_flagging() {
        let classes = vec![class(
            "com/example/Entry",
            true,
            vec![method(
                "<init>",
                &["net/minecraft/client/Minecraft"],
                &[("com/example/Entry", "isDedicatedServer", "()Z")],
            )],
        )];
        assert!(find(&classes).is_none());
    }

    /// `@Mod(dist = CLIENT)` 的入口不参与追踪
    ///
    /// 实测 `create-1.21.1`：`CreateClient` 带 `dist = CLIENT`，其
    /// `<clinit>` 引用 `Minecraft`；若把它当入口，整个 Create 会被误判仅客户端。
    #[test]
    fn client_dist_restricted_entry_is_skipped() {
        let classes = vec![
            class("com/example/Create", true, vec![method("create", &[], &[])]),
            client_only_class(
                "com/example/CreateClient",
                vec![method("<clinit>", &["net/minecraft/client/Minecraft"], &[])],
            ),
        ];
        assert!(find(&classes).is_none());
    }

    /// 客户端引用在另一个类里、入口根本不调用它：不报
    #[test]
    fn client_reference_outside_entry_chain_is_ignored() {
        let classes = vec![
            class("com/example/Entry", true, vec![method("<init>", &[], &[])]),
            class(
                "com/example/client/Renderer",
                false,
                vec![method("<clinit>", &["net/minecraft/client/Minecraft"], &[])],
            ),
        ];
        assert!(find(&classes).is_none());
    }

    /// 没有 `@Mod` 注解的类不被当作入口
    #[test]
    fn class_without_mod_annotation_is_not_an_entry() {
        let classes = vec![class(
            "com/example/Entry",
            false,
            vec![method("<clinit>", &["net/minecraft/client/Minecraft"], &[])],
        )];
        assert!(find(&classes).is_none());
    }

    /// 只有普通方法引用客户端类、入口的初始化不碰它：不报
    #[test]
    fn client_reference_only_in_late_method_is_ignored() {
        let classes = vec![class(
            "com/example/Entry",
            true,
            vec![
                method("<init>", &[], &[]),
                method("render", &["net/minecraft/client/Minecraft"], &[]),
            ],
        )];
        assert!(find(&classes).is_none());
    }

    /// 证据链过长时截断，但保留跳数
    #[test]
    fn chain_text_truncates_and_counts_the_rest() {
        let classes = vec![
            class(
                "com/example/Entry",
                true,
                vec![method("<init>", &[], &[("com/example/A", "step", "()V")])],
            ),
            class(
                "com/example/A",
                false,
                vec![method("step", &[], &[("com/example/B", "step", "()V")])],
            ),
            class(
                "com/example/B",
                false,
                vec![method("step", &[], &[("com/example/C", "step", "()V")])],
            ),
            class(
                "com/example/C",
                false,
                vec![method("step", &[], &[("com/example/D", "step", "()V")])],
            ),
            class(
                "com/example/D",
                false,
                vec![method("step", &["net/minecraft/client/Minecraft"], &[])],
            ),
        ];
        let found = find(&classes).expect("五跳的链仍应命中");
        assert_eq!(found.chain.len(), 5);
        let text = found.chain_text();
        assert!(text.contains("还有 1 跳"), "{text}");
        assert!(
            !text.contains("com/example/D.step"),
            "只展示前 4 跳：{text}"
        );
    }
}
