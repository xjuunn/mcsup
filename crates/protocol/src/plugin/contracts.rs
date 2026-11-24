//! 契约导出（`plugins.md` §14.4）
//!
//! 契约的唯一来源是 `protocol::plugin` 与 `domain::plugin` 的类型。SDK 的
//! 契约文件（`sdks/typescript/src/contracts/v1.ts` 等）由本模块渲染，禁止手写：
//!
//! ```text
//! protocol::plugin::contracts
//!   └─ 生成 → sdks/typescript/src/contracts/v1.ts
//! ```
//!
//! 字段名、可选性与标量类型都取自真实 `serde` 序列化结果，不维护第二份
//! 手写表：新增或改名一个帧字段，生成物会随之变化，漂移测试随之失败。
//!
//! 扩展步骤：新增一种语言只要实现一个 `render_<lang>()`，由
//! `examples/gen-contracts.rs` 统一写出。

use crate::plugin::{
    ConfigValues, EventFrame, Frame, Hello, PLUGIN_PROTOCOL_VERSION, Reject, RejectCode, Welcome,
    WelcomeLimits, WelcomeScope, WireError, category_of,
};
use domain::plugin::PLUGIN_API_VERSION;
use domain::plugin::capability::Capability;
use domain::plugin::descriptor::{
    DEFAULT_OP_TIMEOUT_MS, MAX_DESCRIPTOR_BYTES, MAX_ENUM_VALUES, MAX_OP_DESCRIPTION_CHARS,
    MAX_OP_TIMEOUT_MS, MAX_OP_TITLE_CHARS, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH, MAX_SCHEMA_NODES,
    MAX_SCHEMA_PROPERTIES, MAX_STRING_LENGTH, MIN_OP_TIMEOUT_MS,
};
use domain::plugin::error::PluginErrorCode;
use domain::plugin::event::Subscription;
use domain::plugin::exit_code;
use serde_json::Value;
use std::fmt::Write as _;

/// 生成物首行：任何语言的契约文件都必须带这一行
pub const GENERATED_HEADER: &str = "// GENERATED FROM protocol::plugin. DO NOT EDIT.";

/// 目标语言
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `sdks/typescript/src/contracts/v1.ts`
    TypeScript,
}

impl Target {
    /// 从命令行取值
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "typescript" | "ts" => Some(Self::TypeScript),
            _ => None,
        }
    }

    /// 目标语言名
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
        }
    }
}

/// 帧字段类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// `string`
    String,
    /// `number`
    Number,
    /// `boolean`
    Boolean,
    /// `unknown`（任意 JSON）
    Unknown,
    /// `readonly string[]`
    StringArray,
    /// 引用另一个具名接口
    Ref(&'static str),
    /// 引用一个 TS 联合类型（如 `RejectCode`）
    Named(&'static str),
}

/// 帧字段
#[derive(Debug, Clone)]
pub struct Field {
    /// 线上字段名（`snake_case`）
    pub name: String,
    /// 类型
    pub ty: FieldType,
    /// 是否可省略（`skip_serializing_if` 的字段在缺省时不出现在线上）
    pub optional: bool,
}

/// 帧形状
#[derive(Debug, Clone)]
pub struct FrameShape {
    /// 线上 `type`
    pub wire_type: String,
    /// 合法方向
    pub direction: Direction,
    /// 载荷字段（不含信封 `v` / `type`）
    pub fields: Vec<Field>,
}

/// 帧方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 只允许宿主 → 插件
    HostToPlugin,
    /// 只允许插件 → 宿主
    PluginToHost,
    /// 双向
    Both,
}

/// 具名接口（`welcome.limits` 等）
#[derive(Debug, Clone)]
pub struct RecordShape {
    /// 接口名
    pub name: &'static str,
    /// 字段
    pub fields: Vec<Field>,
}

/// 帧样例的代表性载荷；`full` 为真时把可省略字段全部填上
///
/// 生成器与漂移测试共用同一份样例，字段改动只要动这里，两处一起更新。
#[must_use]
pub fn sample_frames(full: bool) -> Vec<Frame> {
    let optional = |value: &str| if full { Some(value.to_owned()) } else { None };
    vec![
        Frame::Hello(Hello {
            token: "token".to_owned(),
            api: PLUGIN_API_VERSION,
            plugin_id: "example".to_owned(),
            plugin_version: "1.0.0".to_owned(),
            platform: "darwin-arm64".to_owned(),
            pid: 42,
            requested: vec!["server:read".to_owned()],
            subscriptions: vec!["chat:message".to_owned()],
        }),
        Frame::Welcome(Welcome {
            session_id: "session".to_owned(),
            host_version: "0.1.0".to_owned(),
            locale: "en".to_owned(),
            granted: vec!["server:read".to_owned()],
            limits: WelcomeLimits::default(),
            config: ConfigValues::default(),
            scope: WelcomeScope {
                kind: "machine".to_owned(),
                // 两个采样取值不同，字段类型取并集后为可空
                server: if full {
                    Some("survival".to_owned())
                } else {
                    None
                },
            },
        }),
        Frame::Reject(Reject {
            code: RejectCode::Rejected,
            message: "rejected".to_owned(),
            details: if full {
                Some(serde_json::json!({ "why": "test" }))
            } else {
                None
            },
        }),
        Frame::Event(EventFrame {
            seq: 0,
            kind: "chat:message".to_owned(),
            at: "2026-11-20T00:00:00Z".to_owned(),
            server: optional("survival"),
            data: serde_json::json!({}),
        }),
        Frame::Call(crate::plugin::Call::op(1, "echo", serde_json::json!({}))),
        Frame::Result(crate::plugin::ResultFrame::ok(
            1,
            serde_json::json!({ "ok": true }),
        )),
        Frame::Stream(crate::plugin::StreamFrame {
            id: 1,
            seq: 0,
            chunk: serde_json::json!({}),
            done: false,
        }),
        Frame::Cancel(crate::plugin::Cancel {
            id: 1,
            reason: optional("client"),
        }),
        Frame::Config(crate::plugin::ConfigFrame {
            id: 1,
            revision: 0,
            values: serde_json::json!({}),
        }),
        Frame::Ping(1),
        Frame::Pong(1),
        Frame::Shutdown { grace_ms: 2_000 },
    ]
}

/// 帧样例覆盖的变体数；新增变体必须同时加样例
pub const SAMPLE_VARIANTS: usize = 12;

/// 具名接口样例
///
/// `sparse` 是缺省取值下的同一对象，用来判定可省略字段。
#[must_use]
pub fn sample_records() -> Vec<RecordShape> {
    vec![
        record(
            "WelcomeLimits",
            &WelcomeLimits::default(),
            &WelcomeLimits::default(),
        ),
        record(
            "ConfigValues",
            &ConfigValues::default(),
            &ConfigValues::default(),
        ),
        record(
            "WelcomeScope",
            &WelcomeScope {
                kind: "machine".to_owned(),
                server: Some("survival".to_owned()),
            },
            &WelcomeScope {
                kind: "machine".to_owned(),
                server: None,
            },
        ),
        record(
            "WireError",
            &WireError::from_code(PluginErrorCode::Denied, "denied"),
            &WireError::from_code(PluginErrorCode::Denied, "denied"),
        ),
    ]
}

/// 把任意可序列化值转成接口形状
fn record(
    name: &'static str,
    full: &impl serde::Serialize,
    sparse: &impl serde::Serialize,
) -> RecordShape {
    let full_json = serde_json::to_value(full).unwrap_or(Value::Null);
    let sparse_json = serde_json::to_value(sparse).unwrap_or(Value::Null);
    let full_map = full_json.as_object().cloned().unwrap_or_default();
    let sparse_map = sparse_json.as_object().cloned().unwrap_or_default();
    let mut names: Vec<&String> = full_map.keys().chain(sparse_map.keys()).collect();
    names.sort();
    names.dedup();
    let fields = names
        .into_iter()
        .map(|name| {
            let ty = full_map
                .get(name)
                .map(field_type)
                .or_else(|| sparse_map.get(name).map(field_type))
                .unwrap_or(FieldType::Unknown);
            Field {
                name: name.clone(),
                ty: record_type_override(name, ty),
                optional: !sparse_map.contains_key(name),
            }
        })
        .collect();
    RecordShape { name, fields }
}

/// 具名字段的具体类型：`code` / `category` 这类字段用生成的联合类型
fn record_type_override(name: &str, ty: FieldType) -> FieldType {
    match (name, ty) {
        ("code", FieldType::String) => FieldType::Named("ErrorCode"),
        ("category", FieldType::String) => FieldType::Named("ErrorCategory"),
        _ => ty,
    }
}

/// JSON 值 → 字段类型
fn field_type(value: &Value) -> FieldType {
    match value {
        Value::String(_) => FieldType::String,
        Value::Number(_) => FieldType::Number,
        Value::Bool(_) => FieldType::Boolean,
        Value::Array(items) => {
            if items.iter().all(Value::is_string) {
                FieldType::StringArray
            } else {
                FieldType::Unknown
            }
        }
        _ => FieldType::Unknown,
    }
}

/// 同一帧类型的全部代表性载荷
///
/// `call` 与 `result` 各有多种合法形状（四条路由 / 成功与失败），只取两两
/// 采样会漏字段，因此这两种帧给出完整列表；其余帧只有一种形状。
#[must_use]
pub fn variant_samples(wire_type: &str) -> Vec<Frame> {
    match wire_type {
        "call" => vec![
            crate::plugin::Call::op(1, "echo", serde_json::json!({})),
            crate::plugin::Call::host(1, "console:write", "send_line", serde_json::json!({})),
            crate::plugin::Call::control(1, "status", serde_json::json!({})),
            crate::plugin::Call::contract(
                1,
                "chat.publish@1",
                Some("bridge".to_owned()),
                Some("trace".to_owned()),
                serde_json::json!({}),
            ),
        ]
        .into_iter()
        .map(Frame::Call)
        .collect(),
        "result" => vec![
            Frame::Result(crate::plugin::ResultFrame::ok(
                1,
                serde_json::json!({ "ok": true }),
            )),
            Frame::Result(crate::plugin::ResultFrame::error(
                1,
                WireError::from_code(PluginErrorCode::Internal, "failed"),
            )),
        ],
        _ => Vec::new(),
    }
}

/// 帧形状：由两次序列化推导字段与可选性
///
/// 字段集合取两次采样的并集：只出现一次的字段记为可省略。类型取并集，
/// 这样「有值 / 缺省」与「对象 / 字符串」两种情况都能表达。`call` /
/// `result` 的额外形状见 [`variant_samples`]。
///
/// # Panics
///
/// 帧无法序列化时 panic；这属于实现错误，样例全部是合法帧
#[must_use]
pub fn frame_shapes() -> Vec<FrameShape> {
    let full = sample_frames(true);
    let sparse = sample_frames(false);
    assert_eq!(
        full.len(),
        SAMPLE_VARIANTS,
        "sample_frames must cover every Frame variant (SAMPLE_VARIANTS)"
    );
    assert_eq!(full.len(), sparse.len(), "两次采样必须一一对应");
    full.into_iter()
        .zip(sparse)
        .map(|(frame, minimal)| {
            let wire_type = frame.kind().to_owned();
            let mut samples = vec![frame.clone(), minimal];
            samples.extend(variant_samples(&wire_type));
            let kinds = collect_fields(&samples);
            FrameShape {
                direction: direction_of(&frame),
                wire_type,
                fields: kinds
                    .into_iter()
                    .map(|mut field| {
                        if let Some(ty) = ref_override(&field.name, &frame) {
                            field.ty = ty;
                        }
                        field
                    })
                    .collect(),
            }
        })
        .collect()
}

/// 从全部采样推导字段：并集、排序、可选性（一次不落才必填）、类型并集
///
/// 采样序列化失败时返回空表而不是 panic：序列化只可能是实现错误，
/// [`tests::shapes_match_serde_output`] 会立刻把它显示成字段集合漂移。
fn collect_fields(samples: &[Frame]) -> Vec<Field> {
    let maps: Vec<serde_json::Map<String, Value>> = samples
        .iter()
        .filter_map(|frame| {
            crate::plugin::frame_to_value(frame)
                .ok()
                .and_then(|value| value.as_object().cloned())
        })
        .collect();
    let mut names: Vec<String> = maps
        .iter()
        .flat_map(|map| map.keys().cloned())
        .filter(|key| key.as_str() != "v" && key.as_str() != "type")
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            // 全部采样都带 = 必填；只要缺一次就是可省略
            let optional = maps.iter().any(|map| !map.contains_key(&name));
            let mut ty: Option<FieldType> = None;
            for map in &maps {
                if let Some(value) = map.get(&name) {
                    let next = field_type(value);
                    ty = Some(match ty {
                        None => next,
                        Some(prev) => merge_type(prev, next),
                    });
                }
            }
            Field {
                name,
                ty: ty.unwrap_or(FieldType::Unknown),
                optional,
            }
        })
        .collect()
}

/// 两次采样的类型并集：不同即为 `unknown`
fn merge_type(a: FieldType, b: FieldType) -> FieldType {
    if a == b { a } else { FieldType::Unknown }
}

/// 需要映射成具名接口的字段（`welcome.limits` 等）
fn ref_override(field: &str, frame: &Frame) -> Option<FieldType> {
    match (frame, field) {
        (Frame::Welcome(_), "limits") => Some(FieldType::Ref("WelcomeLimits")),
        (Frame::Welcome(_), "config") => Some(FieldType::Ref("ConfigValues")),
        (Frame::Welcome(_), "scope") => Some(FieldType::Ref("WelcomeScope")),
        (Frame::Result(_), "error") => Some(FieldType::Ref("WireError")),
        (Frame::Reject(_), "code") => Some(FieldType::Named("RejectCode")),
        _ => None,
    }
}

/// 帧方向：`plugin_to_host` 只回答「是否只允许插件发送」
fn direction_of(frame: &Frame) -> Direction {
    if frame.plugin_to_host() {
        return Direction::PluginToHost;
    }
    match frame {
        Frame::Call(_) | Frame::Result(_) | Frame::Cancel(_) => Direction::Both,
        _ => Direction::HostToPlugin,
    }
}

// ---------------------------------------------------------------------------
// 渲染：TypeScript
// ---------------------------------------------------------------------------

/// 分节标题之间的分隔注释
const RULE: &str =
    "// ---------------------------------------------------------------------------\n";

/// 渲染 TypeScript 契约（`sdks/typescript/src/contracts/v1.ts`）
///
/// 每一节一个函数：新增帧类型只改帧那一节的采样，新增常量只改常量那一节，
/// 这样单节过长时也能立刻看出来。
#[must_use]
pub fn render_typescript() -> String {
    let mut out = String::new();
    out.push_str(GENERATED_HEADER);
    out.push('\n');
    out.push_str("// 由 `cargo run -q -p mcsup-protocol --example gen-contracts` 生成。\n");
    out.push_str("// 信封字段：每帧都带 `v`（协议版本）与 `type`（帧类型）。\n\n");
    render_constants(&mut out);
    render_exit_codes(&mut out);
    render_errors(&mut out);
    render_names(&mut out);
    render_records(&mut out);
    render_frames(&mut out);
    out
}

/// 节标题
fn section(out: &mut String, title: &str) {
    out.push_str(RULE);
    let _ = writeln!(out, "// {title}");
    out.push_str(RULE);
    out.push('\n');
}

/// 版本与限额
///
/// descriptor 的限值也在这里生成：SDK 要在构建期用同一组数字提前拒绝超限的
/// 产物，手抄一份就会出现「构建期通过、安装期被拒」的错位报错。
fn render_constants(out: &mut String) {
    section(out, "版本与限额");
    let mut constants = vec![
        (
            "PROTOCOL_VERSION",
            "插件协议版本（帧信封 `v`）",
            i64::from(PLUGIN_PROTOCOL_VERSION),
        ),
        (
            "PLUGIN_API_VERSION",
            "插件 API 主版本；SDK 主版本与它相等",
            i64::from(PLUGIN_API_VERSION),
        ),
        (
            "MAX_FRAME_BYTES",
            "单帧字节上限",
            i64::try_from(crate::plugin::MAX_FRAME_BYTES).unwrap_or(i64::MAX),
        ),
        (
            "MAX_STREAM_BYTES",
            "单个流式 op 的总字节上限",
            i64::try_from(port::plugin::MAX_STREAM_BYTES).unwrap_or(i64::MAX),
        ),
        (
            "MAX_JSON_DEPTH",
            "JSON 嵌套深度上限",
            i64::try_from(crate::plugin::MAX_JSON_DEPTH).unwrap_or(i64::MAX),
        ),
        (
            "DEFAULT_INFLIGHT",
            "inflight 缺省上限",
            i64::from(crate::plugin::DEFAULT_INFLIGHT),
        ),
        (
            "DEFAULT_EVENT_QUEUE",
            "事件队列缺省长度",
            i64::try_from(crate::plugin::DEFAULT_EVENT_QUEUE).unwrap_or(i64::MAX),
        ),
        (
            "DEFAULT_LOG_LINES_PER_SEC",
            "日志限速缺省值（行/秒）",
            i64::from(crate::plugin::DEFAULT_LOG_LINES_PER_SEC),
        ),
    ];
    constants.extend(
        descriptor_constants()
            .map(|(name, doc, value)| (name, doc, i64::try_from(value).unwrap_or(i64::MAX))),
    );
    for (name, doc, value) in constants {
        let _ = writeln!(out, "/** {doc} */\nexport const {name} = {value};\n");
    }
}

/// descriptor / schema 限值（`plugins.md` §11.2 / §11.5）
///
/// 这是构建期校验的唯一来源：SDK 侧不再手抄同一组数字。
fn descriptor_constants() -> [(&'static str, &'static str, u64); 12] {
    [
        (
            "MAX_SCHEMA_DEPTH",
            "schema 嵌套深度上限",
            MAX_SCHEMA_DEPTH as u64,
        ),
        (
            "MAX_SCHEMA_PROPERTIES",
            "单对象属性数上限",
            MAX_SCHEMA_PROPERTIES as u64,
        ),
        ("MAX_ENUM_VALUES", "enum 取值数上限", MAX_ENUM_VALUES as u64),
        (
            "MAX_SCHEMA_NODES",
            "单个 schema 节点数上限",
            MAX_SCHEMA_NODES as u64,
        ),
        (
            "MAX_SCHEMA_BYTES",
            "单个 schema 序列化字节上限",
            MAX_SCHEMA_BYTES as u64,
        ),
        (
            "MAX_DESCRIPTOR_BYTES",
            "整个 descriptor 字节上限",
            MAX_DESCRIPTOR_BYTES as u64,
        ),
        (
            "MAX_STRING_LENGTH",
            "单字符串 `maxLength` 上限",
            MAX_STRING_LENGTH,
        ),
        (
            "MIN_OP_TIMEOUT_MS",
            "op 超时下限（毫秒）",
            MIN_OP_TIMEOUT_MS,
        ),
        (
            "MAX_OP_TIMEOUT_MS",
            "op 超时上限（毫秒）",
            MAX_OP_TIMEOUT_MS,
        ),
        (
            "DEFAULT_OP_TIMEOUT_MS",
            "op 超时缺省值（毫秒）",
            DEFAULT_OP_TIMEOUT_MS,
        ),
        (
            "MAX_OP_TITLE_CHARS",
            "op 标题字符上限",
            MAX_OP_TITLE_CHARS as u64,
        ),
        (
            "MAX_OP_DESCRIPTION_CHARS",
            "op 描述字符上限",
            MAX_OP_DESCRIPTION_CHARS as u64,
        ),
    ]
}

/// 退出码
fn render_exit_codes(out: &mut String) {
    section(out, "退出码（plugins.md §6.7）");
    out.push_str("export const ExitCode = {\n");
    for (name, code) in exit_code::ALL {
        let _ = writeln!(out, "  {}: {code},", pascal(name));
    }
    out.push_str("} as const;\n\n");
    out.push_str("export type ExitCodeValue = (typeof ExitCode)[keyof typeof ExitCode];\n\n");
}

/// 错误码、分类与可重试性
fn render_errors(out: &mut String) {
    section(out, "错误码与分类");

    out.push_str("export type ErrorCode =\n");
    for code in PluginErrorCode::ALL {
        let _ = writeln!(out, "  | \"{}\"", code.as_str());
    }
    out.push_str("  ;\n\n");

    out.push_str("export type ErrorCategory =\n");
    for category in categories() {
        let _ = writeln!(out, "  | \"{category}\"");
    }
    out.push_str("  ;\n\n");

    out.push_str("/** 错误码 → 分类（宿主与 SDK 共用一张表） */\n");
    out.push_str("export const ERROR_CATEGORY: Readonly<Record<ErrorCode, ErrorCategory>> = {\n");
    for code in PluginErrorCode::ALL {
        let _ = writeln!(out, "  {}: \"{}\",", code.as_str(), category_of(code));
    }
    out.push_str("};\n\n");

    out.push_str(
        "/** 可重试的错误码 */\nexport const RETRYABLE_ERRORS: readonly ErrorCode[] = [\n",
    );
    for code in PluginErrorCode::ALL {
        if code.retryable() {
            let _ = writeln!(out, "  \"{}\",", code.as_str());
        }
    }
    out.push_str("];\n\n");
}

/// 能力名、订阅名与拒绝原因
fn render_names(out: &mut String) {
    section(out, "能力与订阅名");

    out.push_str("export type CapabilityName =\n");
    for capability in Capability::ALL {
        let _ = writeln!(out, "  | \"{}\"", capability.as_str());
    }
    out.push_str("  ;\n\n");

    out.push_str("/** 当前版本可授予的能力（未开放项不在内） */\n");
    out.push_str("export type OpenCapabilityName =\n");
    for capability in Capability::ALL {
        if capability.stage().is_open() {
            let _ = writeln!(out, "  | \"{}\"", capability.as_str());
        }
    }
    out.push_str("  ;\n\n");

    out.push_str("export type SubscriptionName =\n");
    for subscription in Subscription::ALL {
        let _ = writeln!(out, "  | \"{}\"", subscription.as_str());
    }
    out.push_str("  ;\n\n");

    out.push_str("/** 握手拒绝原因（plugins.md §6.2） */\n");
    out.push_str("export type RejectCode =\n");
    for code in RejectCode::ALL {
        let _ = writeln!(out, "  | \"{}\"", code.as_str());
    }
    out.push_str("  ;\n\n");
}

/// 具名接口
fn render_records(out: &mut String) {
    section(out, "具名接口");
    for record in sample_records() {
        let _ = writeln!(out, "export interface {} {{", record.name);
        push_fields(out, &record.fields);
        out.push_str("}\n\n");
    }
}

/// 帧接口与两个方向联合类型
fn render_frames(out: &mut String) {
    section(out, "帧");
    let shapes = frame_shapes();
    for shape in &shapes {
        let _ = writeln!(out, "export interface {}Frame {{", pascal(&shape.wire_type));
        let _ = writeln!(out, "  readonly type: \"{}\";", shape.wire_type);
        push_fields(out, &shape.fields);
        out.push_str("}\n\n");
    }

    out.push_str("/// 本地合成：未知 `type` 被忽略后由解析器产出，不对应线上帧\n");
    out.push_str("export interface UnknownFrame {\n  readonly type: \"unknown\";\n  readonly kind: string;\n}\n\n");

    out.push_str("export type InboundFrame =\n");
    for shape in &shapes {
        if shape.direction != Direction::PluginToHost {
            let _ = writeln!(out, "  | {}Frame", pascal(&shape.wire_type));
        }
    }
    out.push_str("  | UnknownFrame;\n\n");

    out.push_str("export type OutboundFrame =\n");
    for shape in &shapes {
        if shape.direction != Direction::HostToPlugin {
            let _ = writeln!(out, "  | {}Frame", pascal(&shape.wire_type));
        }
    }
    out.push_str("  ;\n");
}

/// 字段列表（`readonly name?: type;`）
fn push_fields(out: &mut String, fields: &[Field]) {
    for field in fields {
        let _ = writeln!(
            out,
            "  readonly {}{}: {};",
            field.name,
            if field.optional { "?" } else { "" },
            ts_type(field.ty)
        );
    }
}

/// 渲染指定语言的契约
///
/// # Panics
///
/// 目标语言尚未实现时 panic；调用方应先用 [`Target::parse`] 校验
#[must_use]
pub fn render(target: Target) -> String {
    match target {
        Target::TypeScript => render_typescript(),
    }
}

/// 出现过的错误分类（按固定顺序，保证生成物稳定）
fn categories() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for code in PluginErrorCode::ALL {
        let category = category_of(code);
        if !out.contains(&category) {
            out.push(category);
        }
    }
    out
}

/// 字段类型 → TS 类型
fn ts_type(ty: FieldType) -> String {
    match ty {
        FieldType::String => "string".to_owned(),
        FieldType::Number => "number".to_owned(),
        FieldType::Boolean => "boolean".to_owned(),
        FieldType::Unknown => "unknown".to_owned(),
        FieldType::StringArray => "readonly string[]".to_owned(),
        FieldType::Ref(name) | FieldType::Named(name) => name.to_owned(),
    }
}

/// `chat:message` → `ChatMessage`（接口名用）
fn pascal(wire_type: &str) -> String {
    let mut out = String::new();
    for word in wire_type.split([':', '_', '-']) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_variant_has_a_sample() {
        assert_eq!(
            sample_frames(true).len(),
            SAMPLE_VARIANTS,
            "新增 Frame 变体必须补 sample_frames 与 SAMPLE_VARIANTS"
        );
        let mut kinds: Vec<String> = sample_frames(true)
            .iter()
            .map(|frame| frame.kind().to_owned())
            .collect();
        kinds.sort();
        kinds.dedup();
        assert_eq!(kinds.len(), SAMPLE_VARIANTS, "样例出现重复的帧类型");
    }

    #[test]
    fn shapes_match_serde_output() {
        let full = sample_frames(true);
        let sparse = sample_frames(false);
        let shapes = frame_shapes();
        for ((frame, minimal), shape) in full.iter().zip(&sparse).zip(&shapes) {
            let mut samples = vec![frame.clone(), minimal.clone()];
            samples.extend(variant_samples(&shape.wire_type));
            let maps: Vec<serde_json::Map<String, Value>> = samples
                .iter()
                .map(|frame| {
                    crate::plugin::frame_to_value(frame)
                        .expect("serialize")
                        .as_object()
                        .expect("object")
                        .clone()
                })
                .collect();

            let mut expected: Vec<String> = maps
                .iter()
                .flat_map(|map| map.keys().cloned())
                .filter(|key| key.as_str() != "v" && key.as_str() != "type")
                .collect();
            expected.sort();
            expected.dedup();
            let declared: Vec<String> = shape.fields.iter().map(|f| f.name.clone()).collect();
            assert_eq!(declared, expected, "{} 字段集合漂移", shape.wire_type);

            for field in &shape.fields {
                let always_present = maps.iter().all(|map| map.contains_key(&field.name));
                assert_eq!(
                    field.optional, !always_present,
                    "{}.{} 的可选性与 serde 输出不一致",
                    shape.wire_type, field.name
                );
            }
        }
    }

    #[test]
    fn typescript_output_is_stable() {
        let text = render_typescript();
        assert!(text.starts_with(GENERATED_HEADER));
        assert!(text.contains("export const PROTOCOL_VERSION = 1;"));
        assert!(text.contains("export type ErrorCode ="));
        assert!(text.contains("export const ERROR_CATEGORY"));
        assert!(text.contains("export interface HelloFrame {"));
        assert!(text.contains("export type InboundFrame ="));
        assert!(text.contains("export type OutboundFrame ="));
        // 生成物必须能自洽：每个 inbound 引用的接口都存在
        for name in ["WelcomeFrame", "ResultFrame", "UnknownFrame"] {
            assert!(
                text.contains(&format!("export interface {name}")),
                "缺少 {name}"
            );
        }
    }
}
