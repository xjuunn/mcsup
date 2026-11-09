//! 从 jar 归档里读出 mod 的原始信息
//!
//! 判定分层的落点：本模块只把归档里**存在的证据**读出来，归入
//! [`SideVerdict`] 的 `evidence` 列表，并填上元数据**显式声明**的
//! `side`。所有推断（尤其「入口点缺失」这类）也在此完成，因为只有这里能
//! 看到入口点与 mixin 分段。
//!
//! 推断结论按可靠度标注，绝不把弱信号升级成确定结论：
//!
//! | 证据 | 结论 | 可靠度 |
//! | --- | --- | --- |
//! | `environment: "client"` | 仅客户端 | Certain |
//! | NeoForge `[[mods]] side = "CLIENT"` | 只记证据（加载器不读该字段） | — |
//! | `[[dependencies.x]] side = "CLIENT"` | 该依赖在服务端不参与校验 | — |
//! | 只有 `client` 入口，无 `main`/`server` | 仅客户端 | High |
//! | 字节码引用客户端类（仅无加载器元数据的裸 jar） | 仅客户端迹象 | Medium |
//! | mixin 有 `client` 段 | 仅记证据，不下结论 | — |

use domain::mod_::{
    Confidence, DepSide, Evidence, EvidenceSource, ModCapability, ModDependency, ModLoader,
    SideSupport, SideVerdict, VersionRange,
};
use port::mod_::{ModInspectError, ModInspector};
use std::io::Read;
use std::path::Path;
use zip::ZipArchive;

use crate::bytecode;
use crate::json;

/// 基于 zip 归档的本地检测器
#[derive(Debug, Clone, Copy, Default)]
pub struct ZipModInspector;

impl ZipModInspector {
    /// 新建
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ModInspector for ZipModInspector {
    fn inspect(&self, path: &Path) -> Result<ModCapability, ModInspectError> {
        let file = std::fs::File::open(path).map_err(|source| ModInspectError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut zip = ZipArchive::new(file).map_err(|_| ModInspectError::NotAnArchive {
            path: path.to_path_buf(),
        })?;

        // 文件名清单一次取全：后面多次判断「有没有某个文件」都靠它，
        // 避免反复读 central directory
        let names = entry_names(&mut zip);

        let display = path.display().to_string();
        let mut cap = ModCapability::empty(display);

        cap.loaders = detect_loaders(&names);

        cap.nested_jars = names.iter().filter(|n| is_nested_jar(n)).count();

        parse_metadata(&mut zip, &mut cap);

        // 字节码：Java 版本取所有 class 的最大值；客户端类引用只在元数据
        // 完全没给 side 结论时才采信
        let (max_java, any_client_ref) = scan_classes(&mut zip, &names);
        // class major → Java 大版本（65 对用户没意义，用户要的是「Java 21」）
        cap.java_compiled = max_java.and_then(domain::mod_::java_version_from_class_major);
        if any_client_ref {
            apply_client_bytecode(&mut cap);
        }

        // 嵌套 jar：整合包里的「壳 jar」常把真正的实现放在 `META-INF/jars/`
        // （Fabric 的 Jar-in-Jar）或 `META-INF/jarjar/`（Forge 的 JarJar），
        // 依赖方引用的是内层 modid（实测 c2me 依赖 c2me-base，而 c2me-base
        // 只存在于本 jar 的 `META-INF/jars/` 内；PuzzlesLib 把
        // `puzzlesaccessapi` 放在 `META-INF/jarjar/` 内）。不读就会把已满足
        // 的依赖误报成缺失。
        let nested: Vec<String> = names.iter().filter(|n| is_nested_jar(n)).cloned().collect();
        for entry_name in nested {
            if let Some(inner_id) = nested_mod_id(&mut zip, &entry_name) {
                cap.provides.push(inner_id);
            }
        }

        // MANIFEST 里的编译期 MC 版本：只记证据，**不**当运行区间
        //
        // 实测反例：Fabric Language Kotlin 只声明 `fabricloader` 依赖、
        // 没有声明 `minecraft`（它是版本无关的语言适配器，上游版本表覆盖
        // 1.14–26.x），但构建于 MC 26.2。把编译期版本当成运行区间会把
        // 它判成「装不进 1.21.1」。没声明 `minecraft` 就是没有运行约束——
        // 编译期版本只说明「作者用什么版本开发」，不是兼容性声明。
        if let Some(raw) = read_entry(&mut zip, "META-INF/MANIFEST.MF") {
            if let Some(mc) = manifest_mc_version(&raw) {
                cap.side.evidence.push(Evidence::new(
                    EvidenceSource::Archive,
                    format!("MANIFEST 记录编译期 MC 版本 {mc}（非兼容性声明）"),
                ));
            }
            // `mods.toml` 里写 `${file.jarVersion}` 是 Forge 的占位符，加载器
            // 装载时用 MANIFEST 的 `Implementation-Version` 替换。不解析就只
            // 能把占位符原样展示给用户（实测 JEI、`sophisticatedcore`、
            // guardvillagers 等大量 Forge mod 都这么写）。
            if cap.version.as_deref() == Some(FILE_JAR_VERSION)
                && let Some(version) = manifest_jar_version(&raw)
            {
                cap.version = Some(version);
            }
        }

        Ok(cap)
    }

    fn class_names(&self, path: &Path) -> Result<Option<Vec<String>>, ModInspectError> {
        let file = std::fs::File::open(path).map_err(|source| ModInspectError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut zip = ZipArchive::new(file).map_err(|_| ModInspectError::NotAnArchive {
            path: path.to_path_buf(),
        })?;

        let names = entry_names(&mut zip);
        let mut out = class_names_of(&names);

        // 壳 jar 把真正的实现放在 `META-INF/jars/`：内层类名也要收，否则
        // 崩溃帧落在内层实现上时归因不到外层文件（实测 c2me 就是这个形状）。
        // 归因的落点是外层文件——移除它，内层实现随之消失。
        let nested: Vec<String> = names
            .iter()
            .filter(|n| n.starts_with("META-INF/jars/") && has_ext(n, "jar"))
            .cloned()
            .collect();
        for entry_name in nested {
            let Ok(entry) = zip.by_name(&entry_name) else {
                continue;
            };
            let mut raw = Vec::new();
            if entry.take(NESTED_JAR_LIMIT).read_to_end(&mut raw).is_err() {
                continue;
            }
            let Ok(mut inner) = ZipArchive::new(std::io::Cursor::new(raw)) else {
                continue;
            };
            out.extend(class_names_of(&entry_names(&mut inner)));
        }

        Ok(Some(out))
    }
}

/// 取归档内全部条目名
///
/// 名字不是合法 UTF-8 的条目（极少见）跳过：元数据与类名都只可能是
/// ASCII，跳过不会漏掉要判定东西。
fn entry_names<R: Read + std::io::Seek>(zip: &mut ZipArchive<R>) -> Vec<String> {
    let mut names = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        if let Ok(entry) = zip.by_index_raw(i)
            && let Ok(name) = entry.name().map(std::borrow::Cow::into_owned)
        {
            names.push(name);
        }
    }
    names
}

/// 从条目名里挑出类名（内部名，去掉 `.class` 后缀）
fn class_names_of(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|name| has_ext(name, "class"))
        .map(|name| name[..name.len() - ".class".len()].to_owned())
        .collect()
}

/// 路径是否以给定扩展名结尾（大小写不敏感）
///
/// 归档内文件名大小写不受控：`FOO.JAR`、`Foo.Jar` 都出现过，直接
/// `ends_with(".jar")` 会漏掉它们。
fn has_ext(name: &str, ext: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// 按元数据文件存在性判定加载器归属
///
/// 一个 jar 可能同时有多个加载器元数据（Forge + `NeoForge` 双支持），因此
/// 返回 `Vec` 而非单个值。
fn detect_loaders(names: &[String]) -> Vec<ModLoader> {
    let has = |name: &str| names.iter().any(|n| n == name);
    let mut loaders = Vec::new();
    if has("fabric.mod.json") {
        loaders.push(ModLoader::Fabric);
    }
    if has("quilt.mod.json") {
        loaders.push(ModLoader::Quilt);
    }
    if has("META-INF/neoforge.mods.toml") {
        loaders.push(ModLoader::NeoForge);
    }
    if has("META-INF/mods.toml") {
        loaders.push(ModLoader::Forge);
    }
    if has("mcmod.info") {
        loaders.push(ModLoader::LegacyForge);
    }
    if has("litemod.json") {
        loaders.push(ModLoader::LiteLoader);
    }
    if has("plugin.yml") || has("paper-plugin.yml") {
        loaders.push(ModLoader::Bukkit);
    }
    // 无任何模组元数据时，按内容判断是不是纯资源 / 数据包
    if loaders.is_empty() && has("pack.mcmeta") {
        if names.iter().any(|n| n.starts_with("assets/")) {
            loaders.push(ModLoader::ResourcePack);
        } else if names.iter().any(|n| n.starts_with("data/")) {
            loaders.push(ModLoader::DataPack);
        }
    }
    loaders
}

/// 逐加载器解析元数据
///
/// `neoforge.mods.toml` 优先于 `mods.toml`：同时存在时前者是 `NeoForge` 的
/// 权威描述。两者内容通常一致，取其一避免重复记依赖。
fn parse_metadata<R: Read + std::io::Seek>(zip: &mut ZipArchive<R>, cap: &mut ModCapability) {
    if let Some(raw) = read_entry(zip, "fabric.mod.json") {
        parse_fabric(&raw, cap);
    }
    if let Some(raw) = read_entry(zip, "quilt.mod.json") {
        parse_quilt(&raw, cap);
    }
    if let Some(raw) = read_entry(zip, "META-INF/neoforge.mods.toml") {
        parse_toml_mods(&raw, cap, true);
    } else if let Some(raw) = read_entry(zip, "META-INF/mods.toml") {
        parse_toml_mods(&raw, cap, false);
    }
    if let Some(raw) = read_entry(zip, "plugin.yml") {
        parse_plugin_yml(&raw, cap);
    }
}

/// 从嵌套 jar 里读 modid
///
/// 复用 [`parse_metadata`]，因此 Fabric / Quilt / Forge / `NeoForge` 四种
/// 元数据都能读。内层 modid 同样算「本 jar 提供」：加载器会在装载时自动
/// 解出嵌套 jar。
fn nested_mod_id(zip: &mut ZipArchive<std::fs::File>, entry_name: &str) -> Option<String> {
    let mut raw = Vec::new();
    zip.by_name(entry_name)
        .ok()?
        .take(NESTED_JAR_LIMIT)
        .read_to_end(&mut raw)
        .ok()?;
    let cursor = std::io::Cursor::new(raw);
    let mut inner = ZipArchive::new(cursor).ok()?;
    let mut cap = ModCapability::empty(entry_name);
    parse_metadata(&mut inner, &mut cap);
    cap.id
}

/// 嵌套 jar 最多读多少字节
const NESTED_JAR_LIMIT: u64 = 8 * 1024 * 1024;

/// 嵌套 jar 的两套约定
///
/// Fabric 的 Jar-in-Jar 放 `META-INF/jars/`，Forge 的 `JarJar` 放
/// `META-INF/jarjar/`。两者都由加载器在装载时自动解出，内层 modid 因此
/// 同样算「本 jar 提供」——只认前者会把 Forge 的已满足依赖误报成缺失。
const NESTED_JAR_PREFIXES: [&str; 2] = ["META-INF/jars/", "META-INF/jarjar/"];

/// 归档里的某个条目是不是嵌套 jar
fn is_nested_jar(name: &str) -> bool {
    NESTED_JAR_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        && has_ext(name, "jar")
}

/// 读一个条目的全部字节
fn read_entry<R: Read + std::io::Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let mut entry = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// 扫描所有 class：返回（最大 Java 大版本, 是否有客户端类引用）
fn scan_classes(zip: &mut ZipArchive<std::fs::File>, names: &[String]) -> (Option<u32>, bool) {
    let mut max_java = None;
    let mut any_client = false;
    let mut budget = CLASS_SCAN_BUDGET;
    for name in names {
        if !has_ext(name, "class") || budget == 0 {
            continue;
        }
        let Ok(mut entry) = zip.by_name(name) else {
            continue;
        };
        // 大 class 只读头部即可判断 Java 版本，但客户端引用可能在任意偏移，
        // 因此限长读取：绝大多数 class 远小于上限
        let mut buf = Vec::new();
        if entry
            .by_ref()
            .take(CLASS_READ_LIMIT)
            .read_to_end(&mut buf)
            .is_err()
        {
            continue;
        }
        budget -= 1;
        if let Some(major) = bytecode::java_major(&buf) {
            max_java = Some(max_java.map_or(major, |m: u32| m.max(major)));
        }
        if bytecode::references_client_classes(&buf) {
            any_client = true;
        }
    }
    (max_java, any_client)
}

/// 字节码发现客户端类引用后的 side 处置
///
/// 证据永远记；结论只对「连加载器都认不出」的裸 jar 下：
/// - Fabric/Quilt 用 intermediary 名，扫出 `net/minecraft/client/` 不稀奇，
///   定罪会大量误伤
/// - Forge/NeoForge 的服务端 mod 引用客户端类是常态（DistExecutor、懒加载
///   渲染分支），加载器运行期按 dist 正确跳过——实测反例：1.19.2 的
///   MOKEYneedsGeckoLib、betterfpsdist，mods.toml 不写 side、字节码引用了
///   客户端类，装到服务端正常加载
///
/// 裸 jar 没有更好的解释，引用客户端类才是最强信号
fn apply_client_bytecode(cap: &mut ModCapability) {
    cap.side.evidence.push(Evidence::new(
        EvidenceSource::Bytecode,
        "字节码引用客户端专属类（net/minecraft/client/ 或 blaze3d）",
    ));
    if cap.loaders.is_empty()
        && cap.side.server == SideSupport::Unknown
        && cap.side.client == SideSupport::Unknown
    {
        cap.side.client = SideSupport::Required;
        cap.side.server = SideSupport::Unsupported;
        cap.side.confidence = Confidence::Medium;
    }
}

/// 最多扫描多少个 class（避免对超大 jar 做全量 IO）
const CLASS_SCAN_BUDGET: usize = 4000;

/// 单个 class 最多读多少字节
const CLASS_READ_LIMIT: u64 = 512 * 1024;

/// 解析 `fabric.mod.json`
fn parse_fabric(raw: &[u8], cap: &mut ModCapability) {
    let Some(v) = json::parse_lenient(raw) else {
        cap.side.evidence.push(Evidence::new(
            EvidenceSource::Manifest,
            "fabric.mod.json 无法解析（格式损坏）",
        ));
        return;
    };
    cap.id = v["id"].as_str().map(ToOwned::to_owned);
    cap.name = v["name"].as_str().map(ToOwned::to_owned);
    cap.version = v["version"].as_str().map(ToOwned::to_owned);

    // environment：client / server / *。实测几乎全是 `*`，那等同于没声明
    match v["environment"].as_str() {
        Some("client") => {
            cap.side = SideVerdict::declared(SideSupport::Required, SideSupport::Unsupported);
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "environment 显式声明为 client",
            ));
        }
        Some("server") => {
            cap.side = SideVerdict::declared(SideSupport::Unsupported, SideSupport::Required);
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "environment 显式声明为 server",
            ));
        }
        Some("*") => {
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "environment 为 *（通用，不区分客/服，不携带信息）",
            ));
        }
        _ => {}
    }

    // 入口点：只有 client 没有 main/server → 强客户端信号
    let entrypoints = &v["entrypoints"];
    if let Some(map) = entrypoints.as_object() {
        let has_main = map.contains_key("main") || map.contains_key("server");
        let has_client = map.contains_key("client");
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        cap.side.evidence.push(Evidence::new(
            EvidenceSource::Manifest,
            format!("入口点: [{}]", keys.join(", ")),
        ));
        if has_client && !has_main && cap.side.server == SideSupport::Unknown {
            // 只标记「疑似」，**不**升级成 Unsupported。
            //
            // 实测反例：cloth-config 只有 client 入口，却是众多服务端 mod
            // 依赖的配置库（服务端可正常运行）。Iceberg 同样只有 client
            // 入口，而它确实仅客户端。两者在元数据层面无法区分，
            // 因此这条信号只能提示人工确认，不能作为排除依据。
            cap.side.client = SideSupport::Required;
            cap.side.confidence = Confidence::Medium;
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "只有 client 入口、无 main/server：疑似仅客户端，但存在双端库的反例，需实跑确认",
            ));
        }
    }

    // provides 别名：依赖方可能用别名引用本 mod
    if let Some(provides) = v["provides"].as_array() {
        for item in provides {
            if let Some(alias) = item.as_str() {
                cap.provides.push(alias.to_owned());
            }
        }
    }

    // 库身份：`custom.badges` 含 `library`。API 库的 `environment: "client"`
    // 是误标高发区（实测 satin——依赖方要求它，服务端也装载正常），库身份
    // 让领域判定把这类「仅客户端」降级为「待确认」而不是拦死。
    if let Some(badges) = v["custom"]["badges"].as_array() {
        let is_library = badges
            .iter()
            .filter_map(|b| b.as_str())
            .any(|b| b.eq_ignore_ascii_case("library"));
        if is_library {
            cap.is_library = true;
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "上游声明 badges: [library]——API 库，client 入口仅供客户端调用",
            ));
        }
    }

    // depends / recommends / breaks
    push_fabric_deps(&v["depends"], true, cap);
    push_fabric_deps(&v["recommends"], false, cap);
    if let Some(breaks) = v["breaks"].as_object() {
        cap.breaks.extend(breaks.keys().cloned());
    }
    take_fabric_mc(&v["depends"], cap);
    take_fabric_java(&v["depends"], cap);
}

/// 把 fabric 的 depends / recommends 对象转成依赖列表
fn push_fabric_deps(node: &serde_json::Value, mandatory: bool, cap: &mut ModCapability) {
    let Some(map) = node.as_object() else {
        return;
    };
    for (id, spec) in map {
        let range = match spec {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|i| i.as_str())
                .collect::<Vec<_>>()
                .join(" || "),
            _ => String::new(),
        };
        cap.depends
            .push(ModDependency::new(id.clone(), range, mandatory).with_loader(ModLoader::Fabric));
    }
}

/// 从 depends 里取 minecraft 版本要求
fn take_fabric_mc(depends: &serde_json::Value, cap: &mut ModCapability) {
    let Some(spec) = depends["minecraft"].as_str() else {
        return;
    };
    cap.set_mc_range(ModLoader::Fabric, parse_mc_range(spec));
}

/// 从 depends 里取 java 版本要求
fn take_fabric_java(depends: &serde_json::Value, cap: &mut ModCapability) {
    cap.java_required = spec_min_major(depends["java"].as_str());
}

/// 把版本要求字符串解析成 MC 区间
///
/// 支持 fabric 的写法：`1.21.1`、`>=1.21 <1.22`、`~1.21.1`、`>=1.21`、
/// `<1.22`、`1.21.x`。解析不出下界/上界时保留原文，区间留空。
fn parse_mc_range(spec: &str) -> VersionRange {
    let mut range = VersionRange {
        raw: spec.to_owned(),
        min: None,
        max: None,
    };
    // Maven 区间写法优先：Forge 的 `versionRange` 常写成 `[1.21,1.21.2)`。
    // 不认它就会把整串当裸版本号，产出 `[1.21,1.21.2)` 这种乱码下界。
    if let Some((min, max)) = parse_maven_range(spec.trim()) {
        range.min = min;
        range.max = max;
        return range;
    }
    for token in spec.split_whitespace() {
        if let Some(rest) = token.strip_prefix(">=") {
            range.min = Some(normalize_mc(rest));
        } else if let Some(rest) = token.strip_prefix('>') {
            // 严格大于：本项目不区分，按含下界处理并保留原文
            range.min = Some(normalize_mc(rest));
        } else if let Some(rest) = token.strip_prefix("~=") {
            range.min = Some(normalize_mc(rest));
        } else if let Some(rest) = token.strip_prefix('~') {
            // ~1.21.1 表示 1.21.x
            let base = normalize_mc(rest);
            range.min = Some(base.clone());
            range.max = Some(bump_minor(&base));
        } else if let Some(rest) = token.strip_prefix("<=") {
            range.max = Some(bump_patch(&normalize_mc(rest)));
        } else if let Some(rest) = token.strip_prefix('<') {
            range.max = Some(normalize_mc(rest));
        } else if let Some(rest) = token.strip_prefix('^') {
            range.min = Some(normalize_mc(rest));
        } else if is_wildcard(token) {
            // `1.21.x` / `1.21.*` / `1.21`：下界 1.21，上界 1.22。
            // 必须先于裸版本判定：`1.21.x` 不含 `*`，否则会被当成精确版本
            let trimmed = token.trim_end_matches(['x', '*']).trim_end_matches('.');
            let base = normalize_mc(trimmed);
            if !base.is_empty() {
                range.min = Some(base.clone());
                range.max = Some(bump_minor(&base));
            }
        } else if !token.is_empty() {
            // 裸版本号：精确匹配
            let v = normalize_mc(token);
            range.min = Some(v.clone());
            // 1.21.1 这样的精确版本，上界取下一 patch
            if v.matches('.').count() >= 2 {
                range.max = Some(bump_patch(&v));
            }
        }
    }
    range
}

/// 该 token 是否是通配写法（`1.21.x` / `1.21.*`）
///
/// 只看尾部：`x` 在版本号其它位置没有合法含义。
fn is_wildcard(token: &str) -> bool {
    token.contains('*') || token.to_ascii_lowercase().ends_with(".x")
}

/// 规格化 MC 版本号：去掉尾部 `.0` 之外的噪声，保留原样
fn normalize_mc(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c: char| c == ',' || c == '[' || c == ']' || c == '(' || c == ')')
        // fabric 用 `>=1.21-` 表示「1.21 及以后，含预发布」，尾随的 `-` 是
        // 语义标记而非版本号的一部分
        .trim_end_matches('-')
        .to_owned()
}

/// 1.21 → 1.22
fn bump_minor(v: &str) -> String {
    let mut parts: Vec<String> = v.split('.').map(ToOwned::to_owned).collect();
    if parts.len() >= 2
        && let Ok(minor) = parts[1].parse::<u64>()
    {
        parts[1] = (minor + 1).to_string();
        parts.truncate(2);
        return parts.join(".");
    }
    v.to_owned()
}

/// 1.21.1 → 1.21.2
fn bump_patch(v: &str) -> String {
    let mut parts: Vec<String> = v.split('.').map(ToOwned::to_owned).collect();
    if parts.len() >= 3 {
        if let Ok(patch) = parts[2].parse::<u64>() {
            parts[2] = (patch + 1).to_string();
            return parts.join(".");
        }
    } else if parts.len() == 2 {
        return format!("{v}.1");
    }
    v.to_owned()
}

/// 从版本要求里取最低 Java 大版本（`>=17` → 17，`21` → 21）
fn spec_min_major(spec: Option<&str>) -> Option<u32> {
    let spec = spec?;
    let mut best: Option<u32> = None;
    for token in spec.split_whitespace() {
        let digits: String = token.chars().filter(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<u32>()
            && (10..=99).contains(&n)
        {
            best = Some(best.map_or(n, |b: u32| b.min(n)));
        }
    }
    best
}

/// 解析 `quilt.mod.json`
fn parse_quilt(raw: &[u8], cap: &mut ModCapability) {
    let Some(v) = json::parse_lenient(raw) else {
        return;
    };
    let loader = &v["quilt_loader"];
    if cap.id.is_none() {
        cap.id = loader["id"].as_str().map(ToOwned::to_owned);
    }
    if cap.version.is_none() {
        cap.version = loader["version"].as_str().map(ToOwned::to_owned);
    }
    // Quilt 的依赖是对象数组，且支持条件豁免：
    //   {"id": "quilt_resource_loader", "versions": "*",
    //    "unless": "fabric-resource-loader-v0"}
    // 实测 `lukis-*` 系列在 Fabric 服务端就是这个形态：`unless` 命中
    // fabric-api 提供的模块，本依赖不该算缺失。
    for key in ["depends", "recommends"] {
        let Some(deps) = loader[key].as_array() else {
            continue;
        };
        let mandatory = key == "depends";
        for dep in deps {
            let Some(id) = dep["id"].as_str() else {
                continue;
            };
            let mut entry = ModDependency::new(
                id.to_owned(),
                dep["versions"].as_str().unwrap_or_default().to_owned(),
                mandatory && dep["optional"].as_bool().not_optional(),
            )
            .with_loader(ModLoader::Quilt);
            entry = entry.unless_any(quilt_unless(&dep["unless"]));
            entry = entry.unless_any(quilt_unless(&dep["unless_any"]));
            cap.depends.push(entry);
        }
    }
}

/// `optional: true` 表示「非必需」，取反读起来更直观
trait OptionalFlag {
    /// `true`/缺省 → 必需；`false` → 非必需
    fn not_optional(self) -> bool;
}

impl OptionalFlag for Option<bool> {
    fn not_optional(self) -> bool {
        !self.unwrap_or(false)
    }
}

/// 解析 Maven 风格版本区间（Forge `versionRange` 的写法）
///
/// `[1.21,1.21.2)` 下界含、上界不含；`(1.21,1.22]` 两端开闭由括号决定。
/// 本项目只表达「下界含 + 上界不含」，因此：
/// - `[`/`(` 都当含下界（`(` 是排他下界，差一个版本，Minecraft 版本粒度下
///   不值得为此引入开区间模型）
/// - `]` 是含上界的写法，换算成「下一 patch 之前」
///
/// 只认单区间；逗号出现在括号外（多区间并集）或缺少括号时返回 `None`，
/// 交回逐 token 解析，避免把并集误读成一个区间。
fn parse_maven_range(spec: &str) -> Option<(Option<String>, Option<String>)> {
    let mut chars = spec.chars();
    let open = chars.next()?;
    if !matches!(open, '[' | '(') {
        return None;
    }
    let close = spec.chars().last()?;
    if !matches!(close, ']' | ')') {
        return None;
    }
    let inner = &spec[1..spec.len() - 1];
    let (min_raw, max_raw) = inner.split_once(',')?;
    // 多区间：`[1.21,1.21.2),[1.21.3,1.22)` —— 逗号不止一个
    if max_raw.contains(',') {
        return None;
    }
    let min = Some(normalize_mc(min_raw.trim())).filter(|v| !v.is_empty());
    let max = if max_raw.trim().is_empty() {
        None
    } else {
        let v = normalize_mc(max_raw.trim());
        if v.is_empty() {
            None
        } else if close == ']' {
            // 含上界：换算成下一 patch 之前
            Some(bump_patch(&v))
        } else {
            Some(v)
        }
    };
    Some((min, max))
}

/// 读 Quilt 依赖里的条件豁免字段（`unless` / `unless_any`）
///
/// 两种写法都合法：单值字符串 `"fabric-resource-loader-v0"` 或字符串数组。
/// 读不出来就当没有豁免——宁可多报一次缺失，也不要凭空吞掉依赖。
fn quilt_unless(node: &serde_json::Value) -> Vec<String> {
    match node {
        serde_json::Value::String(id) => vec![id.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// 解析 Forge / `NeoForge` 的 `mods.toml`
///
/// 只做最小解析：要 `modId`、`version`、`side`，以及 `[[dependencies]]`
/// 的必需依赖。不引入完整 TOML 解析器——`mods.toml` 里大量 `${...}`
/// 占位符会让严格解析器失败，而我们要的字段都是简单键值。
fn parse_toml_mods(raw: &[u8], cap: &mut ModCapability, neoforge: bool) {
    let Ok(text) = std::str::from_utf8(raw) else {
        return;
    };
    // 只取第一个 [[mods]] 块
    //
    // 块头后面常带尾注释：ForgeMDK 模板生成的 `mods.toml` 一律写
    // `[[mods]] #mandatory`（实测 JEI、`sophisticatedcore`、Placebo、Xaero's
    // 等 17 个 mod 都是这个形态）。用全等判断会把整块跳过去，`modId` 读不
    // 出来，这个 jar 就「不提供任何 modid」，依赖它的 mod 全被误报缺依赖。
    let mut in_first_mod = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[mods]]") {
            if in_first_mod {
                break;
            }
            in_first_mod = true;
            continue;
        }
        if line.starts_with("[[") && in_first_mod {
            in_first_mod = false;
        }
        if !in_first_mod {
            continue;
        }
        if let Some(rest) = line.strip_prefix("modId") {
            cap.id = toml_string(rest);
        } else if let Some(rest) = line.strip_prefix("version") {
            if cap.version.is_none() {
                cap.version = toml_string(rest);
            }
        } else if let Some(rest) = line.strip_prefix("displayName") {
            cap.name = toml_string(rest);
        } else if let Some(rest) = line.strip_prefix("side") {
            apply_toml_side(toml_string(rest).as_deref(), cap);
        }
    }
    // 依赖块
    parse_toml_dependencies(text, cap, neoforge);
    if neoforge && cap.loaders.contains(&ModLoader::Forge) {
        // 同时有 neoforge.mods.toml 与 mods.toml：两者都记，不合并
        cap.side.evidence.push(Evidence::new(
            EvidenceSource::Manifest,
            "同时提供 neoforge.mods.toml 与 mods.toml（双加载器支持）",
        ));
    }
}

/// `key = "value"` 里取 value
fn toml_string(rest: &str) -> Option<String> {
    let rest = rest.trim_start().strip_prefix('=')?.trim();
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find('"').unwrap_or(rest.len());
    let v = rest[..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_owned())
    }
}

/// `NeoForge` / Forge 的 `side` 声明
///
/// # 为什么 `CLIENT` 不再是「服务端不可用」
///
/// `[[mods]] side` 在 Forge 1.19.2 的 `ModInfo` / `ModFileInfo` 里根本没有对应
/// 的字符串常量——加载器不读这个字段，它既不参与装载筛选也不参与依赖校验
/// （真正被读的是 `[[dependencies.x]] side`，见 [`parse_toml_dependencies`]）。
/// 实测 `CreativeCore_FORGE_v2.9.4_mc1.19.2.jar` 写着 `side="CLIENT"` 却能在
/// dedicated server 上正常装载并进入 Done。
///
/// 因此这里只记一条证据，不下「不可用于服务端」的结论：结论交给上游元数据
/// 与其余迹象，风险等级最多到 `Review`。
fn apply_toml_side(side: Option<&str>, cap: &mut ModCapability) {
    match side.map(str::to_ascii_uppercase).as_deref() {
        Some("CLIENT") => {
            // 只有客户端侧必需：服务端不做断言
            if cap.side.client == SideSupport::Unknown {
                cap.side.client = SideSupport::Required;
            }
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "[[mods]] side 声明为 CLIENT（Forge 装载器不读该字段，仅作参考）",
            ));
        }
        Some("SERVER") => {
            cap.side = SideVerdict::declared(SideSupport::Unsupported, SideSupport::Required);
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "side 显式声明为 SERVER",
            ));
        }
        Some("BOTH") => {
            cap.side = SideVerdict::declared(SideSupport::Required, SideSupport::Required);
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                "side 显式声明为 BOTH",
            ));
        }
        _ => {}
    }
}

/// 解析 `[[dependencies.xxx]]` 块
fn parse_toml_dependencies(text: &str, cap: &mut ModCapability, neoforge: bool) {
    let mut in_dep = false;
    let mut id = None;
    let mut range = String::new();
    let mut mandatory = true;
    let mut side = DepSide::Both;
    let flush = |cap: &mut ModCapability,
                 id: &Option<String>,
                 range: &str,
                 mandatory: bool,
                 side: DepSide| {
        if let Some(id) = id {
            cap.depends.push(
                ModDependency::new(id.clone(), range, mandatory)
                    .with_loader(if neoforge {
                        ModLoader::NeoForge
                    } else {
                        ModLoader::Forge
                    })
                    .with_side(side),
            );
            if id == "minecraft" {
                // 按加载器分别记：同一个 jar 可以既有 `fabric.mod.json`
                // （写 Fabric 版的区间）又有 `mods.toml`（写 Forge 版的区间），
                // 实测 Cell-Forge-1.19.2.jar 前者是 `~1.20`、后者是 `[1.19.2]`。
                // 只留先解析到的那份，会把能跑的 Forge mod 判成区间不符。
                cap.set_mc_range(
                    if neoforge {
                        ModLoader::NeoForge
                    } else {
                        ModLoader::Forge
                    },
                    parse_mc_range(range),
                );
            }
            if id == "java" && cap.java_required.is_none() {
                cap.java_required = spec_min_major(Some(range));
            }
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[dependencies") {
            if in_dep {
                flush(cap, &id, &range, mandatory, side);
            }
            in_dep = true;
            id = None;
            range = String::new();
            mandatory = true;
            side = DepSide::Both;
            continue;
        }
        if !in_dep {
            continue;
        }
        if line.starts_with('[') {
            flush(cap, &id, &range, mandatory, side);
            in_dep = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("modId") {
            id = toml_string(rest);
        } else if let Some(rest) = line.strip_prefix("versionRange") {
            range = toml_string(rest).unwrap_or_default();
        } else if let Some(rest) = line.strip_prefix("mandatory") {
            mandatory = !rest.contains("false");
        } else if let Some(rest) = line.strip_prefix("side") {
            // 加载器校验依赖前先按当前 dist 过滤：dedicated server 上
            // `side="CLIENT"` 的依赖整个不参与，读不出来就会误报缺失。
            side = toml_string(rest)
                .as_deref()
                .map_or(DepSide::Both, DepSide::parse);
        }
    }
    if in_dep {
        flush(cap, &id, &range, mandatory, side);
    }
}

/// 解析 `plugin.yml`
fn parse_plugin_yml(raw: &[u8], cap: &mut ModCapability) {
    let Ok(text) = std::str::from_utf8(raw) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            cap.id = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("version:") {
            cap.version = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("api-version:") {
            let api = rest.trim();
            cap.side.evidence.push(Evidence::new(
                EvidenceSource::Manifest,
                format!("Bukkit 插件 api-version {api}"),
            ));
            // `api-version` 的语义是「最低支持的 MC 版本」，与 mod 的
            // `depends.minecraft` 不同：没有上界。不填这一格，插件在
            // 「能否装到这台服务端」里永远只能给出「判不出」。
            if cap.mc_versions.is_any()
                && !api.is_empty()
                && api.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                cap.set_mc_range(
                    ModLoader::Bukkit,
                    VersionRange {
                        raw: format!(">={api}"),
                        min: Some(api.to_owned()),
                        max: None,
                    },
                );
            }
        }
    }
    // 插件只跑在服务端
    if cap.side.server == SideSupport::Unknown {
        cap.side.client = SideSupport::Unsupported;
        cap.side.server = SideSupport::Required;
        cap.side.confidence = Confidence::Certain;
        cap.side.evidence.push(Evidence::new(
            EvidenceSource::Manifest,
            "plugin.yml 存在：Bukkit 插件只运行于服务端",
        ));
    }
}

/// 从 MANIFEST.MF 取 `Fabric-Minecraft-Version`
fn manifest_mc_version(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Fabric-Minecraft-Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// `mods.toml` 里 `version` 的占位符，加载器用 MANIFEST 替换
const FILE_JAR_VERSION: &str = "${file.jarVersion}";

/// 从 MANIFEST.MF 取 `Implementation-Version`
///
/// 这是 Forge 替换 `${file.jarVersion}` 时用的字段。缺失时退回
/// `Specification-Version`：部分 mod 只写后者。
fn manifest_jar_version(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut fallback = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Implementation-Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("Specification-Version:") {
            let v = rest.trim();
            if !v.is_empty() {
                fallback = Some(v.to_owned());
            }
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forge 的 Maven 区间必须按区间解析，不能整串当版本号
    ///
    /// 不修这条时表格里会出现 `>=1.21],[1.21.1 <1.21],[1.22.1` 这类乱码。
    #[test]
    fn parses_maven_range() {
        let r = parse_mc_range("[1.21,1.21.2)");
        assert_eq!(r.min.as_deref(), Some("1.21"));
        assert_eq!(r.max.as_deref(), Some("1.21.2"));
        assert!(r.contains("1.21.1"));
        assert!(!r.contains("1.21.2"));

        // 开上界边界：`]` 是含上界，换算成下一 patch 之前
        let inclusive = parse_mc_range("[1.21,1.21.1]");
        assert!(inclusive.contains("1.21.1"));

        // 无上界
        let open_end = parse_mc_range("[1.21,)");
        assert_eq!(open_end.min.as_deref(), Some("1.21"));
        assert!(open_end.max.is_none());
        assert!(open_end.contains("1.22"));
    }

    /// 多区间并集不硬套成单区间
    #[test]
    fn multi_interval_is_not_flattened() {
        assert!(parse_maven_range("[1.21,1.21.2),[1.22,1.23)").is_none());
    }

    /// Fabric 清单的依赖带 fabric 归属
    #[test]
    fn fabric_deps_carry_loader() {
        let raw = br#"{"schemaVersion":1,"id":"x","version":"1",
            "environment":"*","depends":{"minecraft":"1.21.1","cobblemon":"*"}}"#;
        let mut cap = ModCapability::empty("x.jar");
        parse_fabric(raw, &mut cap);
        let cobblemon = cap
            .depends
            .iter()
            .find(|d| d.id == "cobblemon")
            .expect("应记录 cobblemon 依赖");
        assert_eq!(cobblemon.loader, Some(ModLoader::Fabric));
        assert!(cobblemon.unless.is_empty());
    }

    /// `badges: [library]` 记录库身份（satin 现场的基础）
    #[test]
    fn library_badge_is_detected() {
        let raw = br#"{"schemaVersion":1,"id":"satin","version":"2.0.0",
            "environment":"client","custom":{"badges":["library"]}}"#;
        let mut cap = ModCapability::empty("satin.jar");
        parse_fabric(raw, &mut cap);
        assert!(cap.is_library);
    }

    /// 普通 mod 没有 library 徽章，不误标
    #[test]
    fn normal_mod_is_not_library() {
        let raw = br#"{"schemaVersion":1,"id":"iris","version":"1","environment":"client"}"#;
        let mut cap = ModCapability::empty("iris.jar");
        parse_fabric(raw, &mut cap);
        assert!(!cap.is_library);
    }

    /// Quilt 的条件豁免要读进来，别当成普通硬依赖
    #[test]
    fn quilt_unless_is_captured() {
        let raw = br#"{"schemaVersion":1,"quilt_loader":{"id":"x","version":"1",
            "depends":[{"id":"quilt_resource_loader","versions":"*",
            "unless":"fabric-resource-loader-v0"}]}}"#;
        let mut cap = ModCapability::empty("x.jar");
        parse_quilt(raw, &mut cap);
        let dep = cap
            .depends
            .iter()
            .find(|d| d.id == "quilt_resource_loader")
            .expect("应记录 quilt 依赖");
        assert_eq!(dep.loader, Some(ModLoader::Quilt));
        assert_eq!(dep.unless, vec!["fabric-resource-loader-v0"]);
    }

    /// Forge / `NeoForge` 的 TOML 依赖各自带自己的加载器归属
    #[test]
    fn toml_deps_carry_loader() {
        let text = "[[mods]]\nmodId=\"x\"\nversion=\"1\"\n[[dependencies.x]]\nmodId=\"forge\"\nmandatory=true\nversionRange=\"[47,)\"\n[[dependencies.x]]\nmodId=\"curios\"\nmandatory=true\nversionRange=\"*\"";
        let mut cap = ModCapability::empty("x.jar");
        parse_toml_mods(text.as_bytes(), &mut cap, true);
        let curios = cap
            .depends
            .iter()
            .find(|d| d.id == "curios")
            .expect("应记录 curios 依赖");
        assert_eq!(curios.loader, Some(ModLoader::NeoForge));
    }

    #[test]
    fn parses_exact_mc_version() {
        let r = parse_mc_range("1.21.1");
        assert_eq!(r.min.as_deref(), Some("1.21.1"));
        assert!(r.contains("1.21.1"));
        assert!(!r.contains("1.21"));
    }

    #[test]
    fn parses_range_with_space() {
        let r = parse_mc_range(">=1.21 <1.22");
        assert_eq!(r.min.as_deref(), Some("1.21"));
        assert_eq!(r.max.as_deref(), Some("1.22"));
        assert!(r.contains("1.21.1"));
        assert!(!r.contains("1.22"));
    }

    #[test]
    fn parses_tilde_range() {
        // ~1.21.1 的语义是 >=1.21.1 <1.22（锁 minor，不锁 patch）
        let r = parse_mc_range("~1.21.1");
        assert_eq!(r.min.as_deref(), Some("1.21.1"));
        assert_eq!(r.max.as_deref(), Some("1.22"));
        assert!(r.contains("1.21.5"));
        assert!(!r.contains("1.22"));
    }

    #[test]
    fn parses_open_lower_bound() {
        let r = parse_mc_range(">=1.21");
        assert_eq!(r.min.as_deref(), Some("1.21"));
        assert!(r.max.is_none());
        assert!(r.contains("1.22"));
    }

    #[test]
    fn parses_wildcard() {
        let r = parse_mc_range("1.21.x");
        assert_eq!(r.min.as_deref(), Some("1.21"));
        assert_eq!(r.max.as_deref(), Some("1.22"));
    }

    #[test]
    fn extracts_java_major() {
        assert_eq!(spec_min_major(Some(">=17")), Some(17));
        assert_eq!(spec_min_major(Some("21")), Some(21));
        assert_eq!(spec_min_major(Some(">=21 <25")), Some(21));
        assert_eq!(spec_min_major(Some("")), None);
    }

    #[test]
    fn toml_string_extracts_quoted_value() {
        assert_eq!(
            toml_string(" = \"cobblemon\""),
            Some("cobblemon".to_owned())
        );
        assert_eq!(toml_string("=\"x\""), Some("x".to_owned()));
        assert_eq!(toml_string(" = "), None);
    }

    #[test]
    fn bump_helpers() {
        assert_eq!(bump_minor("1.21"), "1.22");
        assert_eq!(bump_patch("1.21.1"), "1.21.2");
        assert_eq!(bump_patch("1.21"), "1.21.1");
    }

    #[test]
    fn manifest_mc_version_extracted() {
        let raw = b"Manifest-Version: 1.0\r\nFabric-Minecraft-Version: 1.21.1\r\n";
        assert_eq!(manifest_mc_version(raw), Some("1.21.1".to_owned()));
    }

    #[test]
    fn manifest_without_field_returns_none() {
        assert_eq!(manifest_mc_version(b"Manifest-Version: 1.0\r\n"), None);
    }

    /// `${file.jarVersion}` 由 `Implementation-Version` 替换
    #[test]
    fn manifest_jar_version_prefers_implementation() {
        let raw = b"Manifest-Version: 1.0\r\nSpecification-Version: 15.20.0\r\nImplementation-Version: 15.20.0.127\r\n";
        assert_eq!(manifest_jar_version(raw), Some("15.20.0.127".to_owned()));
    }

    /// 只有 `Specification-Version` 时退回它
    #[test]
    fn manifest_jar_version_falls_back_to_specification() {
        let raw = b"Manifest-Version: 1.0\r\nSpecification-Version: 1.2.3\r\n";
        assert_eq!(manifest_jar_version(raw), Some("1.2.3".to_owned()));
        assert_eq!(manifest_jar_version(b"Manifest-Version: 1.0\r\n"), None);
    }

    /// Forge mod 的 mods.toml 不写 side 是常态，字节码引用客户端类不能
    /// 单独定罪（MOKEYneedsGeckoLib / betterfpsdist 现场）：服务端可用 mod
    /// 引用客户端类（DistExecutor、懒加载渲染分支）加载器会正确跳过
    #[test]
    fn forge_mod_with_client_class_refs_is_not_condemned() {
        let text = "[[mods]]\nmodId=\"mokey\"\nversion=\"1.7.0\"\n\
            [[dependencies.mokey]]\nmodId=\"forge\"\nmandatory=true\nversionRange=\"[43,)\"\n\
            [[dependencies.mokey]]\nmodId=\"minecraft\"\nmandatory=true\nversionRange=\"[1.19.2,1.20)\"\n\
            [[dependencies.mokey]]\nmodId=\"geckolib3\"\nmandatory=true\nversionRange=\"[3.1.40,)\"\nside=\"BOTH\"";
        let mut cap = ModCapability::empty("mokey.jar");
        cap.loaders.push(ModLoader::Forge);
        parse_toml_mods(text.as_bytes(), &mut cap, false);
        // 依赖块里的 side=BOTH 是依赖的装载侧，不是 mod 自身的 side：
        // 不应该把 mod 推成双端 Required（那是 apply_toml_side 的事）
        assert_eq!(cap.side.server, SideSupport::Unknown);
        // 字节码引用客户端类：只记证据，不下结论（loaders 非空）
        apply_client_bytecode(&mut cap);
        assert_eq!(cap.side.server, SideSupport::Unknown);
        assert!(
            cap.side
                .evidence
                .iter()
                .any(|e| e.source == EvidenceSource::Bytecode)
        );
    }

    /// 认不出加载器的裸 jar：字节码引用客户端类才允许定罪
    #[test]
    fn unknown_loader_jar_with_client_refs_is_condemned() {
        let mut cap = ModCapability::empty("mystery.jar");
        apply_client_bytecode(&mut cap);
        assert_eq!(cap.side.server, SideSupport::Unsupported);
        assert_eq!(cap.side.client, SideSupport::Required);
    }

    /// `[[mods]] side="CLIENT"` 不再是「服务端不可用」
    ///
    /// Forge 的装载器不读这个字段（`ModInfo` 与 `ModFileInfo` 的字符串常量里
    /// 都没有它），实测 `CreativeCore` 写着 `side="CLIENT"` 却能在 dedicated
    /// server 上装载。因此：客户端侧记必需，服务端侧保持「判不出」，
    /// 风险按 `Review` 处理而不是拦死。
    #[test]
    fn toml_client_side_is_evidence_not_a_verdict() {
        let text = "[[mods]]\nmodId=\"x\"\nside=\"CLIENT\"";
        let mut cap = ModCapability::empty("x.jar");
        cap.loaders.push(ModLoader::Forge);
        parse_toml_mods(text.as_bytes(), &mut cap, false);
        assert_eq!(cap.side.client, SideSupport::Required);
        assert_eq!(cap.side.server, SideSupport::Unknown);
        assert_eq!(cap.risk(), domain::mod_::Risk::Review);
        // 依据里要说明「加载器不读」，否则用户无从判断我们是不是又在瞎判
        assert!(cap.side.evidence.iter().any(|e| e.detail.contains("不读")));
    }

    /// `side="SERVER"` 仍是确定结论：只装服务端
    #[test]
    fn toml_server_side_stays_certain() {
        let text = "[[mods]]\nmodId=\"x\"\nside=\"SERVER\"";
        let mut cap = ModCapability::empty("x.jar");
        parse_toml_mods(text.as_bytes(), &mut cap, false);
        assert_eq!(cap.side.server, SideSupport::Required);
        assert_eq!(cap.side.client, SideSupport::Unsupported);
    }

    /// `[[dependencies.x]] side="CLIENT"` 的依赖在服务端不参与校验
    ///
    /// 实测 embeddiumplus：四个依赖块全写 `side="CLIENT"`，装到 dedicated
    /// server 上加载器整段跳过，服务端正常启动。不读 side 就会报「缺少必需
    /// 依赖 embeddium」，进而把一个能跑的 mod 判成必须移除。
    #[test]
    fn toml_client_dependency_is_inactive_on_server() {
        let text = "[[mods]]\nmodId=\"embeddiumplus\"\n\
                    [[dependencies.embeddiumplus]]\nmodId=\"minecraft\"\nversionRange=\"[1.19.2]\"\nmandatory=true\nside=\"BOTH\"\n\
                    [[dependencies.embeddiumplus]]\nmodId=\"embeddium\"\nversionRange=\"[0.3,)\"\nmandatory=true\nside=\"CLIENT\"\n\
                    [[dependencies.embeddiumplus]]\nmodId=\"forge\"\nversionRange=\"[43,)\"\nmandatory=true\nside=\"BOTH\"\n";
        let mut cap = ModCapability::empty("embeddiumplus.jar");
        cap.loaders.push(ModLoader::Forge);
        parse_toml_mods(text.as_bytes(), &mut cap, false);
        // 依赖要读全（含 side），不能只剩 id
        let dep = cap
            .depends
            .iter()
            .find(|d| d.id == "embeddium")
            .expect("embeddium 依赖应当被读到");
        assert_eq!(dep.side, DepSide::Client);
        // 服务端口径下不成立，也就不该出现在缺失列表里
        assert!(
            cap.missing_mandatory_deps(&[], Some(ModLoader::Forge))
                .is_empty()
        );
    }

    /// 同一个 jar 的两份清单各写各的 MC 区间：按目标加载器取值
    #[test]
    fn mc_range_is_kept_per_loader() {
        let text = "[[mods]]\nmodId=\"cell\"\n\
                    [[dependencies.cell]]\nmodId=\"minecraft\"\nversionRange=\"[1.19.2]\"\nmandatory=true\nside=\"BOTH\"\n";
        let mut cap = ModCapability::empty("cell.jar");
        cap.loaders = vec![ModLoader::Fabric, ModLoader::Forge];
        // Fabric 清单写的是 ~1.20（先解析到的那份）
        cap.set_mc_range(ModLoader::Fabric, parse_mc_range("~1.20"));
        parse_toml_mods(text.as_bytes(), &mut cap, false);
        assert!(cap.mc_range_for(Some(ModLoader::Forge)).contains("1.19.2"));
        assert!(!cap.mc_range_for(Some(ModLoader::Forge)).contains("1.20.1"));
        // Fabric 那份不受 Forge 覆盖
        assert!(cap.mc_range_for(Some(ModLoader::Fabric)).contains("1.20.1"));
    }
}
