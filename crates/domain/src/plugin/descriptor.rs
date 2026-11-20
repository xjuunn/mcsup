//! descriptor：插件界面与输入输出形状的静态声明
//!
//! descriptor 由 SDK 在构建时生成，宿主只读它。它决定 Web 渲染、CLI 输入
//! 校验与流式分块形状——插件作者不写前端，界面完全由这里的声明投影出来。
//!
//! 校验分两步：先内联 `#/schemas/*`（拒绝外部引用与循环），再按白名单子集
//! 检查每个 schema。两步都必须在安装阶段完成：运行时才发现 schema 用了
//! `oneOf`，要么白屏，要么四语言客户端各写一套解释器。

use crate::plugin::{DESCRIPTOR_SCHEMA_VERSION, PLUGIN_API_VERSION, PluginId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// schema 深度上限
pub const MAX_SCHEMA_DEPTH: usize = 6;

/// 单对象属性上限
pub const MAX_SCHEMA_PROPERTIES: usize = 64;

/// enum 取值上限
pub const MAX_ENUM_VALUES: usize = 100;

/// 单个 schema 节点上限
pub const MAX_SCHEMA_NODES: usize = 512;

/// 单个 schema 序列化字节上限
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;

/// 整个 descriptor 字节上限
pub const MAX_DESCRIPTOR_BYTES: usize = 256 * 1024;

/// 单字符串 `maxLength` 上限
pub const MAX_STRING_LENGTH: u64 = 8192;

/// op 超时下限（毫秒）
pub const MIN_OP_TIMEOUT_MS: u64 = 1_000;

/// op 超时上限（毫秒）
pub const MAX_OP_TIMEOUT_MS: u64 = 300_000;

/// 默认 op 超时（毫秒）
pub const DEFAULT_OP_TIMEOUT_MS: u64 = 30_000;

/// op 标题长度上限（字符）
pub const MAX_OP_TITLE_CHARS: usize = 64;

/// op 描述长度上限（字符）
pub const MAX_OP_DESCRIPTION_CHARS: usize = 512;

/// op 种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// 只读查询
    Query,
    /// 有副作用的动作
    Action,
    /// 流式输出
    Stream,
}

impl OpKind {
    /// 线上文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Action => "action",
            Self::Stream => "stream",
        }
    }

    /// 从线上文本解析
    ///
    /// # Errors
    ///
    /// 未知种类返回 [`DescriptorError::InvalidKind`]
    pub fn parse(raw: &str) -> Result<Self, DescriptorError> {
        match raw {
            "query" => Ok(Self::Query),
            "action" => Ok(Self::Action),
            "stream" => Ok(Self::Stream),
            other => Err(DescriptorError::InvalidKind {
                raw: other.to_owned(),
            }),
        }
    }
}

impl Serialize for OpKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OpKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// op 风险：决定 CLI / Web 的确认强度
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpRisk {
    /// 只读
    Read,
    /// 改变状态
    Mutating,
    /// 可能删数据
    Destructive,
}

impl OpRisk {
    /// 线上文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Mutating => "mutating",
            Self::Destructive => "destructive",
        }
    }

    /// 解析
    ///
    /// # Errors
    ///
    /// 未知风险返回 [`DescriptorError::InvalidRisk`]
    pub fn parse(raw: &str) -> Result<Self, DescriptorError> {
        match raw {
            "read" => Ok(Self::Read),
            "mutating" => Ok(Self::Mutating),
            "destructive" => Ok(Self::Destructive),
            other => Err(DescriptorError::InvalidRisk {
                raw: other.to_owned(),
            }),
        }
    }
}

impl Serialize for OpRisk {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OpRisk {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// 六种展示类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Display {
    /// 表单 + 提交
    Form,
    /// 表格 + 分页
    Table {
        /// 列定义（可选，缺省取首行键）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<Vec<String>>,
    },
    /// 键值面板
    Detail {
        /// 展示字段（可选）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<Vec<String>>,
    },
    /// 会话
    Chat {
        /// 发送消息的 action op
        send: String,
        /// 实时消息的 stream op
        stream: String,
        /// 文本字段，缺省 `text`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_field: Option<String>,
        /// 发送者字段，缺省 `from`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_field: Option<String>,
        /// 时间字段，缺省 `at`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_field: Option<String>,
    },
    /// 追加日志
    Log {
        /// 文本字段，缺省 `text`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_field: Option<String>,
        /// 级别字段，缺省 `level`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level_field: Option<String>,
        /// 时间字段，缺省 `at`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_field: Option<String>,
    },
    /// 指标
    Metrics {
        /// 数值字段，缺省 `value`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_field: Option<String>,
        /// 时间字段，缺省 `at`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_field: Option<String>,
        /// 单位
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
}

impl Display {
    /// 稳定文本
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Table { .. } => "table",
            Self::Detail { .. } => "detail",
            Self::Chat { .. } => "chat",
            Self::Log { .. } => "log",
            Self::Metrics { .. } => "metrics",
        }
    }

    /// 按 11.4 推断缺省展示类型
    ///
    /// # Errors
    ///
    /// 无法推断（如 action 的输出是数组）返回 [`DescriptorError::MissingDisplay`]
    pub fn infer(kind: OpKind, output: &serde_json::Value) -> Result<Self, DescriptorError> {
        match kind {
            OpKind::Action => Ok(Self::Form),
            OpKind::Stream => Ok(Self::Log {
                text_field: None,
                level_field: None,
                at_field: None,
            }),
            OpKind::Query => {
                let props = output.get("properties").and_then(|p| p.as_object());
                if props.is_some_and(|p| p.get("items").is_some_and(JsonShape::is_array_like)) {
                    Ok(Self::Table { columns: None })
                } else if output.is_object_like() {
                    Ok(Self::Detail { fields: None })
                } else {
                    Err(DescriptorError::MissingDisplay)
                }
            }
        }
    }
}

trait JsonShape {
    fn is_array_like(&self) -> bool;
    fn is_object_like(&self) -> bool;
}

impl JsonShape for serde_json::Value {
    fn is_array_like(&self) -> bool {
        self.get("type").and_then(|t| t.as_str()) == Some("array")
    }

    fn is_object_like(&self) -> bool {
        self.get("type").and_then(|t| t.as_str()) == Some("object")
    }
}

/// 分页声明
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pagination {
    /// 游标字段
    pub cursor_field: String,
    /// 每页上限字段
    pub limit_field: String,
    /// 默认条数
    pub default_limit: u32,
    /// 最大条数
    pub max_limit: u32,
}

/// 一个 op 的声明
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    /// 插件内唯一 id
    pub id: String,
    /// 种类
    pub kind: OpKind,
    /// 界面标题
    pub title: String,
    /// 描述
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 风险
    pub risk: OpRisk,
    /// 展示类型；缺省按 [`Display::infer`] 推断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    /// 输入 schema
    pub input: serde_json::Value,
    /// 输出 schema
    pub output: serde_json::Value,
    /// 超时（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// 分页
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagination: Option<Pagination>,
}

/// descriptor 原始结构
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    /// schema 版本
    pub schema: u32,
    /// 所属插件
    pub plugin_id: String,
    /// 插件 API 主版本
    pub api: u32,
    /// 配置 schema
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// op 列表
    #[serde(default)]
    pub ops: Vec<Op>,
    /// 命名 schema
    #[serde(default)]
    pub schemas: BTreeMap<String, serde_json::Value>,
}

/// 校验通过的 op：schema 已内联
#[derive(Debug, Clone)]
pub struct ResolvedOp {
    /// 原始声明
    pub op: Op,
    /// 推断后的展示类型
    pub display: Display,
    /// 内联后的输入 schema
    pub input: serde_json::Value,
    /// 内联后的输出 schema
    pub output: serde_json::Value,
}

/// 校验通过的 descriptor
#[derive(Debug, Clone)]
pub struct ValidatedDescriptor {
    /// 原始结构
    pub raw: Descriptor,
    /// 内联后的配置 schema
    pub config: Option<serde_json::Value>,
    /// 按 id 排序的 op
    pub ops: Vec<ResolvedOp>,
}

impl ValidatedDescriptor {
    /// 按 id 查 op
    #[must_use]
    pub fn op(&self, id: &str) -> Option<&ResolvedOp> {
        self.ops.iter().find(|op| op.op.id == id)
    }

    /// op 的最大声明超时
    #[must_use]
    pub fn max_timeout_ms(&self) -> u64 {
        self.ops
            .iter()
            .map(|op| op.op.timeout_ms.unwrap_or(DEFAULT_OP_TIMEOUT_MS))
            .max()
            .unwrap_or(DEFAULT_OP_TIMEOUT_MS)
    }
}

impl Descriptor {
    /// 解析并校验
    ///
    /// # Errors
    ///
    /// JSON 非法、schema 超出子集、引用悬空或展示类型形状不符时返回
    /// [`DescriptorError`]
    pub fn from_json(
        bytes: &[u8],
        expected: &PluginId,
    ) -> Result<ValidatedDescriptor, DescriptorError> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(DescriptorError::DescriptorTooLarge {
                limit: MAX_DESCRIPTOR_BYTES,
            });
        }
        let descriptor: Self =
            serde_json::from_slice(bytes).map_err(|err| DescriptorError::Json(err.to_string()))?;
        descriptor.validate(expected)
    }

    /// 校验已构造的结构
    ///
    /// # Errors
    ///
    /// 同 [`Self::from_json`]
    pub fn validate(self, expected: &PluginId) -> Result<ValidatedDescriptor, DescriptorError> {
        if self.schema != DESCRIPTOR_SCHEMA_VERSION {
            return Err(DescriptorError::UnsupportedSchema {
                found: self.schema,
                supported: DESCRIPTOR_SCHEMA_VERSION,
            });
        }
        if self.api != PLUGIN_API_VERSION {
            return Err(DescriptorError::UnsupportedApi {
                found: self.api,
                supported: PLUGIN_API_VERSION,
            });
        }
        if self.plugin_id != expected.as_str() {
            return Err(DescriptorError::PluginMismatch {
                declared: self.plugin_id,
                expected: expected.to_string(),
            });
        }
        let mut seen = BTreeSet::new();
        let mut resolved = Vec::with_capacity(self.ops.len());
        for op in &self.ops {
            if !op_id_is_valid(&op.id) {
                return Err(DescriptorError::InvalidOpId { id: op.id.clone() });
            }
            if !seen.insert(op.id.clone()) {
                return Err(DescriptorError::DuplicateOp { id: op.id.clone() });
            }
            if op.title.is_empty() || op.title.chars().count() > MAX_OP_TITLE_CHARS {
                return Err(DescriptorError::InvalidTitle { id: op.id.clone() });
            }
            if let Some(description) = &op.description
                && description.chars().count() > MAX_OP_DESCRIPTION_CHARS
            {
                return Err(DescriptorError::InvalidDescription { id: op.id.clone() });
            }
            let timeout = op.timeout_ms.unwrap_or(DEFAULT_OP_TIMEOUT_MS);
            if !(MIN_OP_TIMEOUT_MS..=MAX_OP_TIMEOUT_MS).contains(&timeout) {
                return Err(DescriptorError::InvalidTimeout {
                    id: op.id.clone(),
                    value: timeout,
                });
            }
            if let Some(pagination) = &op.pagination {
                if op.kind == OpKind::Stream {
                    return Err(DescriptorError::InvalidPagination { id: op.id.clone() });
                }
                if pagination.default_limit == 0
                    || pagination.default_limit > pagination.max_limit
                    || pagination.max_limit > 1_000
                {
                    return Err(DescriptorError::InvalidPagination { id: op.id.clone() });
                }
            }
            let input = self.inline_refs(&op.input)?;
            let output = self.inline_refs(&op.output)?;
            validate_schema_root(&input, &op.id, "input")?;
            validate_schema_root(&output, &op.id, "output")?;
            if op.kind == OpKind::Stream && op.pagination.is_some() {
                return Err(DescriptorError::InvalidPagination { id: op.id.clone() });
            }
            let display = match &op.display {
                Some(display) => display.clone(),
                None => Display::infer(op.kind, &output)?,
            };
            resolved.push(ResolvedOp {
                op: op.clone(),
                display,
                input,
                output,
            });
        }
        self.validate_display_refs(&resolved)?;
        for entry in &resolved {
            validate_display_shape(entry)?;
        }
        let config = match &self.config {
            Some(schema) => {
                let inlined = self.inline_refs(schema)?;
                validate_schema_root(&inlined, "<config>", "config")?;
                Some(inlined)
            }
            None => None,
        };
        Ok(ValidatedDescriptor {
            raw: self,
            config,
            ops: resolved,
        })
    }

    fn inline_refs(
        &self,
        schema: &serde_json::Value,
    ) -> Result<serde_json::Value, DescriptorError> {
        inline(schema, &self.schemas, &mut Vec::new(), 0)
    }

    fn validate_display_refs(&self, ops: &[ResolvedOp]) -> Result<(), DescriptorError> {
        for entry in ops {
            if let Display::Chat { send, stream, .. } = &entry.display {
                match self.ops.iter().find(|op| &op.id == send) {
                    Some(target) if target.kind == OpKind::Action => {}
                    _ => {
                        return Err(DescriptorError::InvalidDisplayRef {
                            op: entry.op.id.clone(),
                            field: "send".to_owned(),
                            target: send.clone(),
                        });
                    }
                }
                match self.ops.iter().find(|op| &op.id == stream) {
                    Some(target) if target.kind == OpKind::Stream => {}
                    _ => {
                        return Err(DescriptorError::InvalidDisplayRef {
                            op: entry.op.id.clone(),
                            field: "stream".to_owned(),
                            target: stream.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

fn op_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// 展示类型要求的输出形状
fn validate_display_shape(entry: &ResolvedOp) -> Result<(), DescriptorError> {
    let output = &entry.output;
    let require_prop = |name: &str| {
        output
            .get("properties")
            .and_then(|p| p.as_object())
            .is_some_and(|p| p.contains_key(name))
    };
    let ok = match &entry.display {
        Display::Form | Display::Detail { .. } => output.is_object_like(),
        Display::Table { .. } | Display::Chat { .. } => require_prop("items"),
        Display::Log { .. } => entry.op.kind == OpKind::Stream || require_prop("lines"),
        Display::Metrics { .. } => entry.op.kind == OpKind::Stream || require_prop("points"),
    };
    if ok {
        Ok(())
    } else {
        Err(DescriptorError::DisplayShapeMismatch {
            op: entry.op.id.clone(),
            display: entry.display.type_name().to_owned(),
        })
    }
}

/// 产物不得包含前端资源
///
/// # Errors
///
/// 出现 `.html` / `.css` / `.js` 等扩展名时返回 [`DescriptorError::ForbiddenAsset`]
pub fn check_asset_names<'a, I>(paths: I) -> Result<(), DescriptorError>
where
    I: IntoIterator<Item = &'a Path>,
{
    const FORBIDDEN: &[&str] = &[
        "html", "htm", "css", "js", "mjs", "cjs", "jsx", "ts", "tsx", "vue", "svelte",
    ];
    for path in paths {
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && FORBIDDEN.contains(&ext.to_ascii_lowercase().as_str())
        {
            return Err(DescriptorError::ForbiddenAsset {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn inline(
    value: &serde_json::Value,
    schemas: &BTreeMap<String, serde_json::Value>,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<serde_json::Value, DescriptorError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(DescriptorError::SchemaTooDeep { depth });
    }
    match value {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                if map.len() > 1 {
                    return Err(DescriptorError::InvalidRefSibling);
                }
                let reference = reference.as_str().ok_or(DescriptorError::InvalidRef)?;
                let name = reference.strip_prefix("#/schemas/").ok_or_else(|| {
                    DescriptorError::ExternalRef {
                        reference: reference.to_owned(),
                    }
                })?;
                let schema = schemas
                    .get(name)
                    .ok_or_else(|| DescriptorError::DanglingRef {
                        reference: reference.to_owned(),
                    })?;
                if stack.iter().any(|item| item == name) {
                    return Err(DescriptorError::RecursiveRef {
                        reference: reference.to_owned(),
                    });
                }
                stack.push(name.to_owned());
                let result = inline(schema, schemas, stack, depth + 1);
                stack.pop();
                return result;
            }
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, item) in map {
                out.insert(key.clone(), inline(item, schemas, stack, depth)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(inline(item, schemas, stack, depth)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

fn validate_schema_root(
    schema: &serde_json::Value,
    op: &str,
    direction: &str,
) -> Result<(), DescriptorError> {
    let mut nodes = 0;
    validate_node(schema, op, direction, 1, &mut nodes)?;
    if !schema.is_object_like() {
        return Err(DescriptorError::InvalidSchema {
            op: op.to_owned(),
            direction: direction.to_owned(),
            reason: "top-level schema must be an object".to_owned(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // 关键字表与限额逐项校验，拆开只是把同一份清单分成两处
fn validate_node(
    schema: &serde_json::Value,
    op: &str,
    direction: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), DescriptorError> {
    *nodes += 1;
    if *nodes > MAX_SCHEMA_NODES {
        return Err(DescriptorError::SchemaTooLarge { nodes: *nodes });
    }
    if depth > MAX_SCHEMA_DEPTH {
        return Err(DescriptorError::SchemaTooDeep { depth });
    }
    let Some(map) = schema.as_object() else {
        return Err(DescriptorError::InvalidSchema {
            op: op.to_owned(),
            direction: direction.to_owned(),
            reason: "schema node must be an object".to_owned(),
        });
    };
    for forbidden in [
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "patternProperties",
        "prefixItems",
        "definitions",
        "$defs",
        "transform",
        "refine",
    ] {
        if map.contains_key(forbidden) {
            return Err(DescriptorError::UnsupportedKeyword {
                op: op.to_owned(),
                keyword: forbidden.to_owned(),
            });
        }
    }
    if let Some(reference) = map.get("$ref") {
        let reference = reference.as_str().unwrap_or_default();
        return Err(if reference.starts_with("#/schemas/") {
            DescriptorError::UnresolvedRef {
                reference: reference.to_owned(),
            }
        } else {
            DescriptorError::ExternalRef {
                reference: reference.to_owned(),
            }
        });
    }
    if map.contains_key("$defs") || map.contains_key("definitions") {
        return Err(DescriptorError::UnsupportedKeyword {
            op: op.to_owned(),
            keyword: "$defs".to_owned(),
        });
    }
    let kind = map.get("type").and_then(|t| t.as_str());
    if kind == Some("null") {
        return Err(DescriptorError::UnsupportedKeyword {
            op: op.to_owned(),
            keyword: "type:null".to_owned(),
        });
    }
    if let Some(kind) = kind
        && !matches!(
            kind,
            "object" | "array" | "string" | "number" | "integer" | "boolean"
        )
    {
        return Err(DescriptorError::UnsupportedKeyword {
            op: op.to_owned(),
            keyword: format!("type:{kind}"),
        });
    }
    if direction == "output" && map.contains_key("x-mcsup-secret") {
        return Err(DescriptorError::SecretOnOutput { op: op.to_owned() });
    }
    if let Some(format) = map.get("x-mcsup-format") {
        if kind != Some("string") {
            return Err(DescriptorError::InvalidFormatTarget { op: op.to_owned() });
        }
        let format = format.as_str().unwrap_or_default();
        if format == "html"
            || !matches!(
                format,
                "plain"
                    | "minecraft-text"
                    | "markdown-sanitized"
                    | "datetime"
                    | "duration"
                    | "bytes"
            )
        {
            return Err(DescriptorError::UnsupportedFormat {
                format: format.to_owned(),
            });
        }
    }
    if let Some(max_length) = map.get("maxLength").and_then(serde_json::Value::as_u64)
        && max_length > MAX_STRING_LENGTH
    {
        return Err(DescriptorError::InvalidLimit {
            op: op.to_owned(),
            detail: format!("maxLength {max_length} exceeds {MAX_STRING_LENGTH}"),
        });
    }
    if let Some(values) = map.get("enum").and_then(|v| v.as_array())
        && values.len() > MAX_ENUM_VALUES
    {
        return Err(DescriptorError::EnumTooLarge {
            values: values.len(),
        });
    }
    if kind == Some("object") {
        if let Some(properties) = map.get("properties").and_then(|v| v.as_object()) {
            if properties.len() > MAX_SCHEMA_PROPERTIES {
                return Err(DescriptorError::SchemaTooWide {
                    properties: properties.len(),
                });
            }
            for (name, child) in properties {
                if name.is_empty() {
                    return Err(DescriptorError::InvalidSchema {
                        op: op.to_owned(),
                        direction: direction.to_owned(),
                        reason: "empty property name".to_owned(),
                    });
                }
                validate_node(child, op, direction, depth + 1, nodes)?;
            }
        }
        if let Some(required) = map.get("required").and_then(|v| v.as_array()) {
            for item in required {
                let name = item
                    .as_str()
                    .ok_or_else(|| DescriptorError::InvalidSchema {
                        op: op.to_owned(),
                        direction: direction.to_owned(),
                        reason: "required entries must be strings".to_owned(),
                    })?;
                let declared = map
                    .get("properties")
                    .and_then(|v| v.as_object())
                    .is_some_and(|p| p.contains_key(name));
                if !declared {
                    return Err(DescriptorError::DanglingRequired {
                        op: op.to_owned(),
                        field: name.to_owned(),
                    });
                }
            }
        }
    }
    if kind == Some("array")
        && let Some(items) = map.get("items")
    {
        validate_node(items, op, direction, depth + 1, nodes)?;
    }
    Ok(())
}

/// descriptor 校验失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DescriptorError {
    /// 不是合法 JSON
    #[error("descriptor is not valid JSON: {0}")]
    Json(String),
    /// 文件过大
    #[error("descriptor exceeds {limit} bytes")]
    DescriptorTooLarge {
        /// 上限
        limit: usize,
    },
    /// schema 版本不匹配
    #[error("unsupported descriptor schema {found}, host supports {supported}")]
    UnsupportedSchema {
        /// 声明值
        found: u32,
        /// 宿主支持
        supported: u32,
    },
    /// API 不匹配
    #[error("unsupported plugin api {found}, host supports {supported}")]
    UnsupportedApi {
        /// 声明值
        found: u32,
        /// 宿主支持
        supported: u32,
    },
    /// descriptor 与 manifest 的插件 id 不一致
    #[error("descriptor belongs to {declared}, expected {expected}")]
    PluginMismatch {
        /// descriptor 声明
        declared: String,
        /// 期望值
        expected: String,
    },
    /// op id 非法
    #[error("invalid op id: {id}")]
    InvalidOpId {
        /// op id
        id: String,
    },
    /// op id 重复
    #[error("duplicate op id: {id}")]
    DuplicateOp {
        /// op id
        id: String,
    },
    /// op 种类非法
    #[error("invalid op kind: {raw}")]
    InvalidKind {
        /// 原始值
        raw: String,
    },
    /// op 风险非法
    #[error("invalid op risk: {raw}")]
    InvalidRisk {
        /// 原始值
        raw: String,
    },
    /// 标题非法
    #[error("invalid title for op {id}")]
    InvalidTitle {
        /// op id
        id: String,
    },
    /// 描述非法
    #[error("description too long for op {id}")]
    InvalidDescription {
        /// op id
        id: String,
    },
    /// 超时越界
    #[error("invalid timeout {value} for op {id}")]
    InvalidTimeout {
        /// op id
        id: String,
        /// 毫秒
        value: u64,
    },
    /// 分页声明非法
    #[error("invalid pagination for op {id}")]
    InvalidPagination {
        /// op id
        id: String,
    },
    /// 无法推断展示类型
    #[error("cannot infer display type; declare it explicitly")]
    MissingDisplay,
    /// 展示类型缺少必填引用
    #[error("op {op} display field {field} must reference an existing op, got {target}")]
    InvalidDisplayRef {
        /// op id
        op: String,
        /// 字段
        field: String,
        /// 目标
        target: String,
    },
    /// 输出形状与展示类型不符
    #[error("output shape does not match display {display} for op {op}")]
    DisplayShapeMismatch {
        /// op id
        op: String,
        /// 展示类型
        display: String,
    },
    /// schema 非法
    #[error("invalid {direction} schema for op {op}: {reason}")]
    InvalidSchema {
        /// op id
        op: String,
        /// `input` / `output` / `config`
        direction: String,
        /// 原因
        reason: String,
    },
    /// 使用了子集之外的构造
    #[error("unsupported schema keyword {keyword} in op {op}")]
    UnsupportedKeyword {
        /// op id
        op: String,
        /// 关键字
        keyword: String,
    },
    /// 引用悬空
    #[error("dangling schema reference: {reference}")]
    DanglingRef {
        /// 原始引用
        reference: String,
    },
    /// 外部引用
    #[error("external schema reference is not allowed: {reference}")]
    ExternalRef {
        /// 原始引用
        reference: String,
    },
    /// 内联后仍有引用
    #[error("schema reference was not inlined: {reference}")]
    UnresolvedRef {
        /// 原始引用
        reference: String,
    },
    /// 循环引用
    #[error("recursive schema reference: {reference}")]
    RecursiveRef {
        /// 原始引用
        reference: String,
    },
    /// `$ref` 与兄弟键混用
    #[error("$ref must not have sibling keywords")]
    InvalidRefSibling,
    /// `$ref` 不是字符串
    #[error("$ref must be a string")]
    InvalidRef,
    /// schema 过深
    #[error("schema exceeds depth limit: {depth}")]
    SchemaTooDeep {
        /// 实际深度
        depth: usize,
    },
    /// 对象属性过多
    #[error("object declares too many properties: {properties}")]
    SchemaTooWide {
        /// 属性数
        properties: usize,
    },
    /// enum 取值过多
    #[error("enum declares too many values: {values}")]
    EnumTooLarge {
        /// 取值数
        values: usize,
    },
    /// 节点过多
    #[error("schema has too many nodes: {nodes}")]
    SchemaTooLarge {
        /// 节点数
        nodes: usize,
    },
    /// 限值非法
    #[error("invalid limit in op {op}: {detail}")]
    InvalidLimit {
        /// op id
        op: String,
        /// 细节
        detail: String,
    },
    /// `required` 指向未声明字段
    #[error("op {op} requires undeclared field {field}")]
    DanglingRequired {
        /// op id
        op: String,
        /// 字段
        field: String,
    },
    /// 密钥字段出现在输出
    #[error("x-mcsup-secret is only allowed on input, found on output of op {op}")]
    SecretOnOutput {
        /// op id
        op: String,
    },
    /// 格式注解挂在非字符串上
    #[error("x-mcsup-format requires a string schema, violated in op {op}")]
    InvalidFormatTarget {
        /// op id
        op: String,
    },
    /// 不支持的格式
    #[error("unsupported x-mcsup-format: {format}")]
    UnsupportedFormat {
        /// 格式名
        format: String,
    },
    /// 产物包含前端资源
    #[error("forbidden frontend asset in artifact: {path}")]
    ForbiddenAsset {
        /// 路径
        path: std::path::PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id() -> PluginId {
        PluginId::parse("demo").expect("id")
    }

    #[allow(clippy::needless_pass_by_value)] // 测试构造：直接吞掉 json! 的临时值
    fn object_schema(props: serde_json::Value) -> serde_json::Value {
        json!({ "type": "object", "properties": props, "additionalProperties": false })
    }

    fn base() -> serde_json::Value {
        json!({
            "schema": 1,
            "plugin_id": "demo",
            "api": 1,
            "ops": [{
                "id": "history",
                "kind": "query",
                "title": "历史",
                "risk": "read",
                "input": object_schema(json!({})),
                "output": object_schema(json!({}))
            }]
        })
    }

    #[allow(clippy::needless_pass_by_value)] // 测试里反复 parse(value)，按值转交更贴近调用点
    fn parse(value: serde_json::Value) -> Result<ValidatedDescriptor, DescriptorError> {
        Descriptor::from_json(&serde_json::to_vec(&value).expect("encode"), &id())
    }

    #[test]
    fn minimal_descriptor_validates_and_infers_detail() {
        let descriptor = parse(base()).expect("valid");
        assert_eq!(descriptor.ops.len(), 1);
        assert_eq!(descriptor.ops[0].display.type_name(), "detail");
    }

    #[test]
    fn duplicate_and_invalid_op_ids_are_rejected() {
        let mut value = base();
        value["ops"] = json!([
            { "id": "History", "kind": "query", "title": "t", "risk": "read",
              "input": object_schema(json!({})), "output": object_schema(json!({})) }
        ]);
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidOpId { .. })
        ));

        let mut value = base();
        let op = value["ops"][0].clone();
        value["ops"] = json!([op.clone(), op]);
        assert!(matches!(
            parse(value),
            Err(DescriptorError::DuplicateOp { .. })
        ));
    }

    #[test]
    fn timeout_bounds_are_enforced() {
        for timeout in [999, 300_001] {
            let mut value = base();
            value["ops"][0]["timeout_ms"] = json!(timeout);
            assert!(matches!(
                parse(value),
                Err(DescriptorError::InvalidTimeout { .. })
            ));
        }
        let mut value = base();
        value["ops"][0]["timeout_ms"] = json!(1_000);
        assert!(parse(value).is_ok());
    }

    #[test]
    fn refs_are_inlined_and_missing_refs_rejected() {
        let mut value = base();
        value["schemas"] = json!({ "empty": object_schema(json!({})) });
        value["ops"][0]["input"] = json!({ "$ref": "#/schemas/empty" });
        assert!(parse(value).is_ok());

        let mut value = base();
        value["ops"][0]["input"] = json!({ "$ref": "#/schemas/missing" });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::DanglingRef { .. })
        ));

        let mut value = base();
        value["ops"][0]["input"] = json!({ "$ref": "https://example.com/schema.json" });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::ExternalRef { .. })
        ));
    }

    #[test]
    fn recursive_refs_are_rejected() {
        let mut value = base();
        value["schemas"] = json!({
            "a": { "type": "object", "properties": { "next": { "$ref": "#/schemas/a" } } }
        });
        value["ops"][0]["input"] = json!({ "$ref": "#/schemas/a" });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::RecursiveRef { .. })
        ));
    }

    #[test]
    fn schema_subset_is_enforced() {
        for keyword in ["oneOf", "anyOf", "allOf", "if", "patternProperties"] {
            let mut value = base();
            let mut schema = object_schema(json!({}));
            schema[keyword] = json!([]);
            value["ops"][0]["input"] = schema;
            assert!(parse(value).is_err(), "{keyword}");
        }
        let mut value = base();
        value["ops"][0]["input"] = json!({ "type": "null" });
        assert!(parse(value).is_err());
    }

    #[test]
    fn depth_width_and_enum_limits_are_enforced() {
        fn nested(depth: usize) -> serde_json::Value {
            let mut schema = object_schema(json!({}));
            for _ in 1..depth {
                schema = object_schema(json!({ "child": schema }));
            }
            schema
        }
        let mut value = base();
        value["ops"][0]["input"] = nested(6);
        assert!(parse(value).is_ok(), "depth 6 应通过");

        let mut value = base();
        value["ops"][0]["input"] = nested(7);
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SchemaTooDeep { .. })
        ));

        let mut props = serde_json::Map::new();
        for i in 0..65 {
            props.insert(format!("p{i}"), json!({ "type": "string" }));
        }
        let mut value = base();
        value["ops"][0]["input"] = object_schema(serde_json::Value::Object(props));
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SchemaTooWide { .. })
        ));

        let mut value = base();
        value["ops"][0]["input"] = object_schema(json!({
            "kind": { "type": "string", "enum": (0..101).collect::<Vec<_>>() }
        }));
        assert!(matches!(
            parse(value),
            Err(DescriptorError::EnumTooLarge { .. })
        ));
    }

    #[test]
    fn required_must_reference_declared_property() {
        let mut value = base();
        value["ops"][0]["input"] = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["b"]
        });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::DanglingRequired { .. })
        ));
    }

    #[test]
    fn secret_is_input_only() {
        let mut value = base();
        value["ops"][0]["input"] = object_schema(json!({
            "token": { "type": "string", "x-mcsup-secret": true }
        }));
        assert!(parse(value).is_ok());

        let mut value = base();
        value["ops"][0]["output"] = object_schema(json!({
            "token": { "type": "string", "x-mcsup-secret": true }
        }));
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SecretOnOutput { .. })
        ));
    }

    #[test]
    fn format_annotations_are_checked() {
        let mut value = base();
        value["ops"][0]["input"] = object_schema(json!({
            "at": { "type": "string", "format": "date-time", "x-mcsup-format": "datetime" }
        }));
        assert!(parse(value).is_ok());

        let mut value = base();
        value["ops"][0]["input"] = object_schema(json!({
            "n": { "type": "integer", "x-mcsup-format": "plain" }
        }));
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidFormatTarget { .. })
        ));

        let mut value = base();
        value["ops"][0]["input"] = object_schema(json!({
            "html": { "type": "string", "x-mcsup-format": "html" }
        }));
        assert!(matches!(
            parse(value),
            Err(DescriptorError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn all_six_display_types_resolve() {
        let mut value = base();
        value["ops"] = json!([
            { "id": "submit", "kind": "action", "title": "提交", "risk": "mutating",
              "display": { "type": "form" },
              "input": object_schema(json!({ "text": { "type": "string" } })),
              "output": object_schema(json!({})) },
            { "id": "rows", "kind": "query", "title": "列表", "risk": "read",
              "display": { "type": "table", "columns": ["name"] },
              "input": object_schema(json!({})),
              "output": object_schema(json!({
                  "items": { "type": "array", "items": { "type": "object" } }
              })) },
            { "id": "one", "kind": "query", "title": "详情", "risk": "read",
              "display": { "type": "detail", "fields": ["name"] },
              "input": object_schema(json!({})),
              "output": object_schema(json!({ "name": { "type": "string" } })) },
            { "id": "history", "kind": "query", "title": "会话", "risk": "read",
              "display": { "type": "chat", "send": "submit", "stream": "messages" },
              "input": object_schema(json!({})),
              "output": object_schema(json!({
                  "items": { "type": "array", "items": { "type": "object" } }
              })) },
            { "id": "lines", "kind": "stream", "title": "日志", "risk": "read",
              "display": { "type": "log" },
              "input": object_schema(json!({})),
              "output": object_schema(json!({
                  "at": { "type": "string" }, "text": { "type": "string" }
              })) },
            { "id": "points", "kind": "stream", "title": "指标", "risk": "read",
              "display": { "type": "metrics", "unit": "ms" },
              "input": object_schema(json!({})),
              "output": object_schema(json!({
                  "at": { "type": "string" }, "value": { "type": "number" }
              })) },
            { "id": "messages", "kind": "stream", "title": "消息", "risk": "read",
              "input": object_schema(json!({})),
              "output": object_schema(json!({
                  "at": { "type": "string" }, "from": { "type": "string" },
                  "text": { "type": "string" }
              })) }
        ]);
        let descriptor = parse(value).expect("six display types validate");
        let names: Vec<&str> = descriptor
            .ops
            .iter()
            .map(|op| op.display.type_name())
            .collect();
        assert_eq!(
            names,
            ["form", "table", "detail", "chat", "log", "metrics", "log"]
        );
        assert_eq!(
            descriptor.ops[3].display,
            Display::Chat {
                send: "submit".to_owned(),
                stream: "messages".to_owned(),
                text_field: None,
                from_field: None,
                at_field: None,
            }
        );
    }

    #[test]
    fn display_shape_is_checked() {
        let mut value = base();
        value["ops"][0]["display"] = json!({ "type": "table" });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::DisplayShapeMismatch { .. })
        ));

        let mut value = base();
        value["ops"][0]["display"] = json!({ "type": "table" });
        value["ops"][0]["output"] = object_schema(json!({
            "items": { "type": "array", "items": { "type": "object" } }
        }));
        assert!(parse(value).is_ok());
    }

    #[test]
    fn chat_display_must_reference_declared_ops() {
        let mut value = base();
        value["ops"] = json!([
            { "id": "history", "kind": "query", "title": "t", "risk": "read",
              "display": { "type": "chat", "send": "send", "stream": "messages" },
              "input": object_schema(json!({})),
              "output": object_schema(json!({ "items": { "type": "array", "items": { "type": "object" } } })) },
            { "id": "send", "kind": "action", "title": "t", "risk": "mutating",
              "input": object_schema(json!({})), "output": object_schema(json!({})) },
            { "id": "messages", "kind": "stream", "title": "t", "risk": "read",
              "input": object_schema(json!({})), "output": object_schema(json!({})) }
        ]);
        assert!(parse(value).is_ok());
    }

    #[test]
    fn stream_defaults_to_log() {
        let mut value = base();
        value["ops"][0]["kind"] = json!("stream");
        value["ops"][0]["output"] = object_schema(json!({}));
        let descriptor = parse(value).expect("stream");
        assert_eq!(descriptor.ops[0].display.type_name(), "log");
    }

    #[test]
    fn descriptor_id_must_match_manifest() {
        let mut value = base();
        value["plugin_id"] = json!("other");
        assert!(matches!(
            parse(value),
            Err(DescriptorError::PluginMismatch { .. })
        ));
    }

    #[test]
    fn frontend_assets_are_rejected() {
        for name in ["index.html", "app.js", "style.css", "ui.jsx"] {
            let path = std::path::PathBuf::from(name);
            assert!(
                matches!(
                    check_asset_names([path.as_path()]),
                    Err(DescriptorError::ForbiddenAsset { .. })
                ),
                "{name}"
            );
        }
        assert!(check_asset_names([Path::new("bin/demo")]).is_ok());
        assert!(check_asset_names([Path::new("README.md")]).is_ok());
    }
}
