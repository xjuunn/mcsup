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
use crate::classfile::{Call, ClassFile, Method};
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
    let side_fields = side_fields(classes, &index);
    let mut entries = classes
        .iter()
        .filter(|class| class.mod_entry && !class.mod_client_only)
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
        .into_iter()
        .find_map(|entry| walk(entry, &index, &side_fields))
}

/// 本 jar 内「值来自客 / 服判断」的静态布尔字段
///
/// 现场：Cupboard 的 `<clinit>` 写
/// `IS_CLIENT_OR_INTEGRATED = FMLEnvironment.dist == Dist.CLIENT`，配置加载里
/// 读它来跳过客户端兼容层（`if (IS_CLIENT_OR_INTEGRATED) ClientConfigCompat...`）。
/// 读点只看到一个布尔字段，认不出是 dist 判断，于是把后面的 `Minecraft`
/// 引用当成无条件执行，误判仅客户端。
///
/// 所以把「写点由 dist 比较得来」的字段收集起来，读它等同于 dist 判断。
/// 只收布尔字段、只认 `Dist` 本身或方向谓词名，避免把 `FMLEnvironment.
/// production` 派生的 `IN_DEV` 一类开发标志也算进来。
fn side_fields<'a>(
    classes: &'a [ClassFile],
    index: &HashMap<&'a str, &'a ClassFile>,
) -> HashSet<(String, String)> {
    let mut fields = HashSet::new();
    for class in classes {
        for method in &class.methods {
            if !writes_side_predicate(index, method) {
                continue;
            }
            for write in &method.field_writes {
                if write.descriptor == "Z" {
                    fields.insert((write.owner.clone(), write.name.clone()));
                }
            }
        }
    }
    fields
}

/// 该方法写入的值是否由客 / 服判断得来
fn writes_side_predicate(index: &HashMap<&str, &ClassFile>, method: &Method) -> bool {
    method
        .class_refs
        .iter()
        .any(|name| bytecode::is_dist_type(name))
        || method
            .calls
            .iter()
            .any(|call| bytecode::is_side_predicate_name(&call.name))
        || method
            .calls
            .iter()
            .any(|call| calls_side_predicate(index, call))
}

fn walk<'a>(
    entry: &'a ClassFile,
    index: &HashMap<&'a str, &'a ClassFile>,
    side_fields: &HashSet<(String, String)>,
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
            && !path_guarded(index, side_fields, &parent, key)
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
/// 四条都查：
///
/// 1. `class_refs` 覆盖直接读的 dist 类型（`Dist` / `FMLEnvironment`）
/// 2. `calls` 覆盖包装在平台工具类里的判断（`Platform.isPhysicalClient()`、
///    `isDedicatedServer` 这类）
/// 3. 调用一个**本 jar 内返回 boolean、且自身含 dist 判断**的方法
/// 4. 读一个**本 jar 内由 dist 比较赋值**的静态布尔字段
///
/// 第 3 条是实测补的：Jade 的 `CommonProxy.<init>` 写的是
/// `if (isPhysicallyClient()) { ClientProxy.init(); }`，而 `isPhysicallyClient`
/// 是它自己定义的 `FMLEnvironment.dist.isClient()` 包装。按名字白名单只能
/// 命中拼写相同的 `isPhysicalClient`（Kiwi），拼写差一个字符就漏——实测
/// `isPhysicallyClient` 让 Jade 被误判成仅客户端。按「是不是一个 dist 判断
/// 谓词」识别，不再依赖拼写。
///
/// 第 4 条同理，只是把判断存进了字段：Cupboard 的
/// `IS_CLIENT_OR_INTEGRATED` 在 `<clinit>` 里由 `dist == Dist.CLIENT` 赋值，
/// 配置加载读它来跳过客户端兼容层。
///
/// 要求返回 `boolean`，是因为只有谓词才可能作为分支条件决定后续代码走不走；
/// 一个 void 方法内部有 dist 判断，说明不了调用方紧接着的客户端调用是否受它
/// 保护。
fn method_guarded<'a>(
    index: &HashMap<&'a str, &'a ClassFile>,
    side_fields: &HashSet<(String, String)>,
    method: &Method,
) -> bool {
    method
        .class_refs
        .iter()
        .any(|name| bytecode::is_guard_type(name))
        || method
            .calls
            .iter()
            .any(|call| bytecode::is_guard_call(&call.owner, &call.name))
        || method
            .calls
            .iter()
            .any(|call| calls_side_predicate(index, call))
        || method
            .calls
            .iter()
            .any(|call| reads_side_field(side_fields, call))
}

/// 该成员访问是不是「读一个由 dist 比较赋值的静态布尔字段」
fn reads_side_field(side_fields: &HashSet<(String, String)>, call: &Call) -> bool {
    call.descriptor == "Z" && side_fields.contains(&(call.owner.clone(), call.name.clone()))
}

/// 调用目标是否「本 jar 内返回 boolean 的客 / 服判断谓词」
fn calls_side_predicate<'a>(index: &HashMap<&'a str, &'a ClassFile>, call: &Call) -> bool {
    if !call.descriptor.ends_with(")Z") {
        return false;
    }
    let Some(owner) = index.get(call.owner.as_str()).copied() else {
        return false;
    };
    let Some(target) = owner
        .methods
        .iter()
        .find(|method| method.name == call.name && method.descriptor == call.descriptor)
    else {
        return false;
    };
    target
        .class_refs
        .iter()
        .any(|name| bytecode::is_guard_type(name))
        || target
            .calls
            .iter()
            .any(|inner| bytecode::is_guard_call(&inner.owner, &inner.name))
}

/// 从入口到引用点的整条路径上是否出现过 dist 判断
fn path_guarded<'a>(
    index: &HashMap<&'a str, &'a ClassFile>,
    side_fields: &HashSet<(String, String)>,
    parent: &HashMap<Key<'a>, Option<Key<'a>>>,
    key: Key<'a>,
) -> bool {
    let mut current = Some(key);
    while let Some(node) = current {
        if let Some(method) = method_at(index, node)
            && method_guarded(index, side_fields, method)
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
            field_writes: Vec::new(),
        }
    }

    /// 带静态字段写入的方法：`putstatic owner.name:Z`
    fn writer(name: &str, refs: &[&str], writes: &[(&str, &str)]) -> Method {
        let mut method = method(name, refs, &[]);
        method.field_writes = writes
            .iter()
            .map(|(owner, field)| Call {
                owner: (*owner).to_owned(),
                name: (*field).to_owned(),
                descriptor: "Z".to_owned(),
            })
            .collect();
        method
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

    /// 本 jar 内自造的 dist 谓词：Jade 的 `isPhysicallyClient()` 拼写与白名单
    /// 里的 `isPhysicalClient` 差一个字符，只能按「是不是 dist 谓词」识别
    ///
    /// 现场：`CommonProxy.<init>` 是
    /// `if (isPhysicallyClient()) { ClientProxy.init(bus); }`，
    /// `isPhysicallyClient` 的方法体是 `FMLEnvironment.dist.isClient()`。
    #[test]
    fn local_side_predicate_prevents_flagging() {
        let mut predicate = method(
            "isPhysicallyClient",
            &[
                "net/neoforged/fml/loading/FMLEnvironment",
                "net/neoforged/api/distmarker/Dist",
            ],
            &[("net/neoforged/api/distmarker/Dist", "isClient", "()Z")],
        );
        predicate.descriptor = "()Z".to_owned();
        let classes = vec![
            class(
                "com/example/CommonProxy",
                true,
                vec![
                    method(
                        "<init>",
                        &[],
                        &[
                            ("com/example/CommonProxy", "isPhysicallyClient", "()Z"),
                            ("com/example/ClientProxy", "init", "()V"),
                        ],
                    ),
                    predicate,
                ],
            ),
            class(
                "com/example/ClientProxy",
                false,
                vec![method(
                    "init",
                    &["com/mojang/blaze3d/platform/InputConstants"],
                    &[],
                )],
            ),
        ];
        assert!(find(&classes).is_none());
    }

    /// 谓词不返回 boolean：说明不了调用方后续的客户端调用受它保护，仍要报
    #[test]
    fn void_helper_with_dist_check_does_not_suppress() {
        let classes = vec![
            class(
                "com/example/Entry",
                true,
                vec![
                    method(
                        "<init>",
                        &[],
                        &[
                            ("com/example/Entry", "logSide", "()V"),
                            ("com/example/ClientProxy", "init", "()V"),
                        ],
                    ),
                    // void 方法内部有 dist 判断，但调用它不代表后续调用被分支保护
                    method(
                        "logSide",
                        &["net/minecraftforge/fml/loading/FMLEnvironment"],
                        &[],
                    ),
                ],
            ),
            class(
                "com/example/ClientProxy",
                false,
                vec![method("init", &["net/minecraft/client/Minecraft"], &[])],
            ),
        ];
        let found = find(&classes).expect("应仍然报出");
        assert_eq!(found.client_type, "net/minecraft/client/Minecraft");
    }

    /// 由 dist 比较赋值的静态布尔字段：Cupboard 的 `IS_CLIENT_OR_INTEGRATED`
    ///
    /// 现场：`Cupboard.<clinit>` 写 `IS_CLIENT_OR_INTEGRATED = dist == Dist.CLIENT`，
    /// `CupboardConfig.load` 读它再决定要不要走客户端兼容层。读点只有一个布尔
    /// 字段，认不出是 dist 判断，会把后面的 `Minecraft` 引用当成无条件执行。
    #[test]
    fn dist_derived_static_field_prevents_flagging() {
        let mut load = method(
            "load",
            &[],
            &[
                ("com/example/Entry", "IS_CLIENT_OR_INTEGRATED", "Z"),
                ("com/example/ClientCompat", "onLoad", "()V"),
            ],
        );
        load.descriptor = "(Z)V".to_owned();
        let classes = vec![
            class(
                "com/example/Entry",
                true,
                vec![
                    method("<init>", &[], &[("com/example/Entry", "load", "(Z)V")]),
                    load,
                    writer(
                        "<clinit>",
                        &["net/neoforged/api/distmarker/Dist"],
                        &[("com/example/Entry", "IS_CLIENT_OR_INTEGRATED")],
                    ),
                ],
            ),
            class(
                "com/example/ClientCompat",
                false,
                vec![method("onLoad", &["net/minecraft/client/Minecraft"], &[])],
            ),
        ];
        assert!(find(&classes).is_none());
    }

    /// 开发环境标志不是客 / 服判断：`IN_DEV = !FMLEnvironment.production`
    ///
    /// 认成 guard 会把「只在 dev 环境才碰客户端类」的 mod 放过，漏报真正的
    /// 服务端崩溃。
    #[test]
    fn dev_flag_field_is_not_a_side_guard() {
        let classes = vec![
            class(
                "com/example/Entry",
                true,
                vec![
                    method("<init>", &[], &[("com/example/Compat", "init", "()V")]),
                    writer(
                        "<clinit>",
                        &["net/neoforged/fml/loading/FMLEnvironment"],
                        &[("com/example/Entry", "IN_DEV")],
                    ),
                ],
            ),
            class(
                "com/example/Compat",
                false,
                vec![method(
                    "init",
                    &["net/minecraft/client/Minecraft"],
                    &[("com/example/Entry", "IN_DEV", "Z")],
                )],
            ),
        ];
        assert!(find(&classes).is_some());
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
