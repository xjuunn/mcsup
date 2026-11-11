//! 启动崩溃归因：把异常栈里的帧归到「哪个 mod 提供的类」
//!
//! 与 [`super::Risk`] 的分工：风险等级回答「装之前能不能装」，本模块回答
//! 「装完了为什么起不来」。后者是**事后**判定，依据是加载器自己抛出的栈，
//! 因此不需要猜；但它只能指出「栈顶落在谁身上」，不能证明那个 mod 一定有错
//! ——也可能是它依赖的库缺失、版本区间不符。
//!
//! # 为什么不用静态扫描代替
//!
//! 实测一个 26 个 mod 的 Fabric 服务端：20 个 jar 声明双端却引用
//! `net.minecraft.client.*`，其中 16 个在专用服务端运行正常——共享代码里的
//! 客户端分支不会被执行到。静态扫描无法区分「引用」与「执行到」，误报率
//! 六成以上；而崩溃栈里出现的帧是加载器**实际执行过**的代码。因此归因只在
//! 事后做，且只把它当作处置建议，不当作定罪。
//!
//! # 解析口径
//!
//! 输入是原始日志文本，输出两类事实：
//!
//! - **缺失类**：`NoClassDefFoundError` / `ClassNotFoundException` 的目标
//! - **可疑帧**：非平台包（JDK / Minecraft / 加载器 / 常用库）的栈帧，
//!   按出现顺序去重
//!
//! 平台帧必须排除：`at java.base/java.lang.Thread.run` 与
//! `at knot//net.minecraft.server.Main.main` 出现在每一条崩溃里，保留它们
//! 会让归因结果永远是「Minecraft 自己」。

use std::fmt;

/// 平台自身的包前缀（点分）
///
/// 这些包由 JDK、Minecraft、加载器或它们的传递依赖提供，不可能属于
/// `mods/` 下的某个 jar。归因时必须排除，否则每条栈都能归到
/// `net.minecraft.server.Main`。
const PLATFORM_PREFIXES: &[&str] = &[
    "java.",
    "javax.",
    "jdk.",
    "sun.",
    "com.sun.",
    "net.minecraft.",
    "com.mojang.",
    "net.fabricmc.",
    "net.minecraftforge.",
    "net.neoforged.",
    "cpw.mods.",
    "org.spongepowered.",
    "org.lwjgl.",
    "io.netty.",
    "it.unimi.",
    "org.apache.",
    "org.slf4j.",
    "com.google.",
    "org.objectweb.asm.",
    "org.jetbrains.",
];

/// 标记「类不存在」的异常名
const MISSING_MARKERS: &[&str] = &["NoClassDefFoundError: ", "ClassNotFoundException: "];

/// 一条可疑栈帧
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashSuspect {
    /// 帧所在类（内部名，`net/jsa2025/calcmod/commands/CalcCommand`）
    ///
    /// 用内部名而不是点分名：`mods/` 下的 jar 归档条目也是这个形状，
    /// 服务层按它反查文件时不需要再做转换。
    pub class: String,
    /// 原始帧文本，供展示时对照日志
    pub frame: String,
}

impl CrashSuspect {
    /// 点分形式的类名，用于日志展示
    #[must_use]
    pub fn dotted_class(&self) -> String {
        self.class.replace('/', ".")
    }
}

impl fmt::Display for CrashSuspect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dotted_class())
    }
}

/// 一次归因的结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CrashAttribution {
    /// 缺失的类（内部名，去重后保持出现顺序）
    pub missing: Vec<String>,
    /// 可疑帧，按出现顺序（栈顶在前）
    pub suspects: Vec<CrashSuspect>,
    /// 加载器在错误消息里**明确指认**的类，优先级高于 [`CrashSuspect`]。
    ///
    /// Fabric Loader 的入口点失败消息形如 `provided by 'lilac' at
    /// 'opekope2.lilac.impl.tick.TickNotifier'`：肇事类是 Kotlin adapter 反射
    /// 加载的，不会以 `at` 帧出现在栈上，只存在于这条消息里。这条消息是
    /// 加载器自己的结论，比「栈顶落在谁身上」更权威，归因时应优先用它。
    pub blamed: Vec<CrashSuspect>,
}

impl CrashAttribution {
    /// 没有任何可归因的线索
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.suspects.is_empty() && self.blamed.is_empty()
    }
}

/// 从日志文本里提取归因线索
///
/// 纯函数：不读文件、不访问网络，输入多少行就只看多少行。调用方负责决定
/// 截取哪一段日志（通常是最近一次启动的末尾若干行）。
#[must_use]
pub fn attribute_crash(log: &str) -> CrashAttribution {
    let mut attribution = CrashAttribution::default();
    for raw in log.lines() {
        let line = raw.trim();
        if let Some(missing) = missing_class(line) {
            if !attribution.missing.contains(&missing) {
                attribution.missing.push(missing);
            }
            continue;
        }
        // 加载器指认的类优先级高于栈帧：先收集，它可能是 Kotlin 反射加载、
        // 不出现在 `at` 帧里的「隐形肇事者」。
        for class in blamed_classes(line) {
            if !attribution.blamed.iter().any(|seen| seen.class == class) {
                attribution.blamed.push(CrashSuspect {
                    class,
                    frame: line.to_owned(),
                });
            }
        }
        let Some(class) = frame_class(line) else {
            continue;
        };
        if is_platform(&class) {
            continue;
        }
        if attribution.suspects.iter().any(|seen| seen.class == class) {
            continue;
        }
        attribution.suspects.push(CrashSuspect {
            class,
            frame: line.to_owned(),
        });
    }
    attribution
}

/// 从一行错误消息里读出加载器指认的肇事类（内部名），没有返回空
///
/// 专盯 Fabric Loader 的入口点失败消息：
/// `provided by 'lilac' at 'opekope2.lilac.impl.tick.TickNotifier'`。
/// 肇事类只出现在单引号里，按「引号内、点分、非平台」三条过滤，`lilac`、
/// `main` 这类不含点分的 token 会被形状校验挡掉。
fn blamed_classes(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(idx) = rest.find('\'') {
        let after = &rest[idx + 1..];
        let Some(end) = after.find('\'') else {
            break;
        };
        let token = &after[..end];
        let class = token.replace('.', "/");
        if looks_like_class(token) && !is_platform(&class) && !out.contains(&class) {
            out.push(class);
        }
        rest = &after[end + 1..];
    }
    out
}

/// 从一行里读出「缺失的类」，不是这类行返回 `None`
fn missing_class(line: &str) -> Option<String> {
    for marker in MISSING_MARKERS {
        let Some(index) = line.find(marker) else {
            continue;
        };
        let rest = line[index + marker.len()..].trim();
        // 形如 `NoClassDefFoundError: Could not initialize class X` 时
        // 冒号后面是一句话而不是类名，此时不认
        let token = rest.split_whitespace().next()?;
        if !looks_like_class(token) {
            return None;
        }
        return Some(token.replace('.', "/"));
    }
    None
}

/// 从一行栈帧里读出类名（内部名），不是栈帧返回 `None`
fn frame_class(line: &str) -> Option<String> {
    let rest = line.strip_prefix("at ")?;
    let rest = rest.split('(').next()?.trim();
    // `knot//net/minecraft/...`、`java.base/java.lang.Thread.run`：
    // 加载器与模块前缀都在最后一个 `//` 之前
    let rest = rest.rsplit("//").next()?.trim();
    let (class, _method) = rest.rsplit_once('.')?;
    if class.is_empty() || !looks_like_class(class) {
        return None;
    }
    Some(if class.contains('/') {
        class.to_owned()
    } else {
        class.replace('.', "/")
    })
}

/// 类名是否由平台提供
fn is_platform(class: &str) -> bool {
    let dotted = class.replace('/', ".");
    PLATFORM_PREFIXES
        .iter()
        .any(|prefix| dotted.starts_with(prefix))
}

/// 粗判一个 token 像不像类名
///
/// 只做形状校验：带包分隔符、不含空格、字符集合法。不做「是否真的存在」
/// 的判断——那需要类加载器，本模块是纯函数。
fn looks_like_class(token: &str) -> bool {
    if !token.contains(['/', '.']) {
        return false;
    }
    token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '$' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 现场日志：CalcMod 在服务端注册命令时碰到客户端类
    const CALCMOD_CRASH: &str = r"
[10:13:47] [main/WARN]: Failed to load datapacks, can't proceed with server load.
java.util.concurrent.ExecutionException: java.lang.NoClassDefFoundError: net/minecraft/client/player/LocalPlayer
	at java.base/java.util.concurrent.CompletableFuture.wrapInExecutionException(CompletableFuture.java:345)
	at knot//net.minecraft.server.Main.main(Main.java:193)
	at net.fabricmc.loader.impl.game.minecraft.MinecraftGameProvider.launch(MinecraftGameProvider.java:517)
Caused by: java.lang.NoClassDefFoundError: net/minecraft/client/player/LocalPlayer
	at knot//net.jsa2025.calcmod.commands.CalcCommand.registerServer(CalcCommand.java:89)
	at knot//net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback.lambda$static$1(CommandRegistrationCallback.java:43)
	at knot//net.minecraft.commands.Commands.<init>(Commands.java:339)
	... 14 more
Caused by: java.lang.ClassNotFoundException: net.minecraft.client.player.LocalPlayer
";

    #[test]
    fn attributes_calcmod_style_crash() {
        let attribution = attribute_crash(CALCMOD_CRASH);
        assert_eq!(
            attribution.missing,
            vec!["net/minecraft/client/player/LocalPlayer"]
        );
        // 两个 `Caused by` 都提到同一个类，去重后只剩一条
        assert_eq!(attribution.suspects.len(), 1);
        assert_eq!(
            attribution.suspects[0].class,
            "net/jsa2025/calcmod/commands/CalcCommand"
        );
        assert_eq!(
            attribution.suspects[0].dotted_class(),
            "net.jsa2025.calcmod.commands.CalcCommand"
        );
    }

    #[test]
    fn platform_frames_are_not_suspects() {
        let log = r"
java.lang.RuntimeException: boom
	at java.base/java.lang.Thread.run(Thread.java:1583)
	at knot//net.minecraft.server.Main.main(Main.java:193)
	at net.fabricmc.loader.impl.launch.knot.Knot.launch(Knot.java:72)
";
        let attribution = attribute_crash(log);
        assert!(attribution.suspects.is_empty());
        assert!(attribution.is_empty());
    }

    #[test]
    fn same_class_appears_once() {
        let log = r"
	at knot//com/example/mod/Thing.b(Thing.java:2)
	at knot//com/example/mod/Thing.a(Thing.java:1)
";
        let attribution = attribute_crash(log);
        assert_eq!(attribution.suspects.len(), 1);
        assert_eq!(attribution.suspects[0].class, "com/example/mod/Thing");
    }

    #[test]
    fn message_without_class_name_is_not_missing_class() {
        let attribution = attribute_crash(
            "Caused by: java.lang.NoClassDefFoundError: Could not initialize class a.b.C",
        );
        assert!(attribution.missing.is_empty());
    }

    #[test]
    fn empty_log_attributes_to_nothing() {
        assert!(attribute_crash("").is_empty());
    }

    #[test]
    fn native_frame_without_line_number_is_still_read() {
        let attribution = attribute_crash("\tat app//com/example/mod/Native.baz(Native Method)");
        assert_eq!(attribution.suspects[0].class, "com/example/mod/Native");
    }

    /// Kotlin 反射加载的肇事类不出现在 `at` 帧里，但 Fabric Loader 在
    /// 错误消息中会写出 `provided by 'lilac' at 'opekope2...'`：
    /// 归因必须从消息里读到类名，放到 `blamed` 而不是 `suspects`。
    const LILAC_CRASH: &str = r"
[10:42:30] [main/ERROR]: Encountered an exception whilst scanning fields in 'main' entrypoint for mod 'LiLaC'!
net.fabricmc.loader.api.EntrypointException: Exception while loading entries for entrypoint 'main' provided by 'lilac'
	at net.fabricmc.loader.impl.entrypoint.EntrypointContainerImpl.getEntrypoint(EntrypointContainerImpl.java:56)
	at com.supermartijn642.core.registry.RegistryEntryAcceptor$Handler.gatherAnnotatedFields(RegistryEntryAcceptor.java:73)
	at com.supermartijn642.core.CoreLib.beforeInitialize(CoreLib.java:69)
	at net.minecraft.server.Main.main(Main.java:112)
	at net.fabricmc.loader.impl.launch.knot.Knot.launch(Knot.java:72)
[10:42:34] [main/ERROR]: Failed to start the minecraft server
java.lang.RuntimeException: Could not execute entrypoint stage 'main' due to errors, provided by 'lilac' at 'opekope2.lilac.impl.tick.TickNotifier'!
	at net.fabricmc.loader.impl.FabricLoaderImpl.invokeEntrypoints(FabricLoaderImpl.java:416)
Caused by: net.fabricmc.loader.api.EntrypointException: Exception while loading entries for entrypoint 'main' provided by 'lilac'
	at net.fabricmc.loader.impl.entrypoint.EntrypointContainerImpl.getEntrypoint(EntrypointContainerImpl.java:56)
Caused by: java.lang.RuntimeException: Cannot load class net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents$EndWorldTick in environment type SERVER
	at net.fabricmc.loader.impl.transformer.FabricTransformer.transform(FabricTransformer.java:64)
	at net.fabricmc.loader.impl.launch.knot.KnotClassDelegate.tryLoadClass(KnotClassDelegate.java:369)
";

    #[test]
    fn lilac_blamed_class_from_error_message() {
        let attribution = attribute_crash(LILAC_CRASH);
        // 引号里的 non-platform 类名被收进 blamed
        assert_eq!(
            attribution
                .blamed
                .iter()
                .map(|s| s.class.as_str())
                .collect::<Vec<_>>(),
            vec!["opekope2/lilac/impl/tick/TickNotifier"],
            "Fabric Loader 在错误消息里指认的类应该进 blamed"
        );
        // `at` 帧里仍有 supermartijn642corelib（它是无辜框架，但这是栈帧事实）
        assert!(
            attribution
                .suspects
                .iter()
                .any(|s| s.class
                    == "com/supermartijn642/core/registry/RegistryEntryAcceptor$Handler"),
            "at 帧里仍会记录框架类——归因优先级由 service 层决定"
        );
        // `lilac`（不含点）和 `main` 被 looks_like_class 挡掉
        assert!(
            !attribution
                .blamed
                .iter()
                .any(|s| s.class.contains("lilac") && !s.class.contains('/'))
        );
        // ClientTickEvents 是 platform（net.fabricmc.），不收入
        assert!(
            !attribution
                .blamed
                .iter()
                .any(|s| s.class.contains("ClientTickEvents"))
        );
    }
}
