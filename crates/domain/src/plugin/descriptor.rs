//! descriptor：插件界面与输入输出形状的静态声明
//!
//! descriptor 由 SDK 在构建时生成，宿主只读它。它决定 Web 渲染、CLI 输入
//! 校验与流式分块形状——插件作者不写前端，界面完全由这里的声明投影出来。
//!
//! 校验分两步：先内联 `#/schemas/*`（拒绝外部引用与循环），再按白名单子集
//! 检查每个 schema。两步都必须在安装阶段完成：运行时才发现 schema 用了
//! `oneOf`，要么白屏，要么四语言客户端各写一套解释器。

use crate::plugin::schema;
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

/// setup 字段数上限
pub const MAX_SETUP_FIELDS: usize = 32;

/// setup 路径长度上限（字符）
pub const MAX_SETUP_PATH_CHARS: usize = 128;

/// setup 标签长度上限（字符）
pub const MAX_SETUP_LABEL_CHARS: usize = 64;

/// setup 描述长度上限（字符）
pub const MAX_SETUP_DESCRIPTION_CHARS: usize = 512;

/// setup 列表字段的可选值上限（项）
pub const MAX_SETUP_CHOICES: usize = 32;

/// setup 单个可选值的长度上限（字符）
pub const MAX_SETUP_CHOICE_CHARS: usize = 64;

/// `bind.servers` 可声明的路径数上限
pub const MAX_BIND_PATHS: usize = 8;

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

/// 安装向导：授权后由宿主逐项询问配置，装完即可启动
///
/// 字段值由宿主写入配置（深度合并），插件无需实现任何安装逻辑。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// 配置写完后是否自动启用并启动；默认 `true`
    #[serde(default = "start_default")]
    pub start: bool,
    /// 逐项提示的配置字段
    #[serde(default)]
    pub fields: Vec<SetupField>,
}

const fn start_default() -> bool {
    true
}

/// 单个安装提示字段
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupField {
    /// 配置路径，点号分隔（如 `qq.appId`）
    pub path: String,
    /// 是否必填；必填项不允许留空
    #[serde(default)]
    pub required: bool,
    /// 输入时遮蔽；只能用于 `string` 字段
    #[serde(default)]
    pub secret: bool,
    /// 列表字段的可选值；声明后向导渲染成多选框
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    /// 选项由宿主动态提供（如已注册服务器）；与 `choices` 互斥
    #[serde(
        default,
        rename = "choicesFrom",
        skip_serializing_if = "Option::is_none"
    )]
    pub choices_from: Option<ChoicesSource>,
    /// 覆盖 schema 的 `x-mcsup-label`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 覆盖 schema 的 `description`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 提示默认值；省略时取 schema 的 `default`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// 插件与服务器的绑定声明（schema 3 起可用）
///
/// 列出的配置路径值就是服务器名（字符串，或字符串数组，如多服务器场景）。
/// 宿主据此让插件跟随服务器生命周期：绑定的服务器启动前先启动插件，
/// 最后一个绑定的服务器停止后再停止插件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    /// 指向服务器名的配置路径（点号分隔）
    #[serde(default)]
    pub servers: Vec<String>,
}

/// [`Descriptor::resolve_config_setup_and_bind`] 的产物
///
/// 三元组单独起名：返回类型直接展开会让签名难读，也让 clippy 报「类型过于复杂」。
type ResolvedConfigSetupBind = (
    Option<serde_json::Value>,
    Option<ResolvedSetup>,
    Vec<String>,
);

/// 校验通过的 setup
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSetup {
    /// 是否自动启动
    pub start: bool,
    /// 按声明顺序排列的字段
    pub fields: Vec<ResolvedSetupField>,
}

/// 校验通过的 setup 字段：类型与默认值已从 config schema 解析
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSetupField {
    /// 配置路径
    pub path: String,
    /// 展示标签
    pub label: String,
    /// 帮助行
    pub description: Option<String>,
    /// 是否必填
    pub required: bool,
    /// 是否遮蔽输入
    pub secret: bool,
    /// 默认值（声明优先，其次 schema）
    pub default: Option<serde_json::Value>,
    /// 选项由宿主动态提供；`None` 表示用 `kind` 里的静态选项
    pub choices_from: Option<ChoicesSource>,
    /// 输入形态
    pub kind: SetupKind,
    /// 该路径的 schema，用于逐项复校验
    pub schema: serde_json::Value,
}

/// setup 字段的动态选项来源
///
/// 有些字段的可选值装机器才知道（例如已注册的服务器名）。声明来源后宿主
/// 在提问时自行枚举，插件不必也不能写死一份会过期的 `choices`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoicesSource {
    /// 已注册的服务器名
    Servers,
}

/// setup 字段的输入形态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupKind {
    /// 文本，可选枚举
    Text {
        /// 枚举取值，空表示自由文本
        choices: Vec<String>,
    },
    /// 整数
    Integer,
    /// 浮点数
    Number,
    /// 布尔
    Boolean,
    /// 字符串列表；`choices` 非空时渲染成多选框
    StringList {
        /// 可选值，空表示自由输入
        choices: Vec<String>,
    },
}

impl SetupKind {
    fn from_schema(schema: &serde_json::Value) -> Option<Self> {
        let kind = schema.get("type").and_then(serde_json::Value::as_str)?;
        match kind {
            "string" => {
                let choices = schema
                    .get("enum")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                Some(Self::Text { choices })
            }
            "integer" => Some(Self::Integer),
            "number" => Some(Self::Number),
            "boolean" => Some(Self::Boolean),
            "array"
                if schema
                    .get("items")
                    .and_then(|items| items.get("type"))
                    .and_then(serde_json::Value::as_str)
                    == Some("string") =>
            {
                Some(Self::StringList {
                    choices: Vec::new(),
                })
            }
            _ => None,
        }
    }
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
    /// 安装向导（schema 2 起可用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// 服务器绑定（schema 3 起可用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<Bind>,
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
    /// 安装向导（已解析类型与默认值）
    pub setup: Option<ResolvedSetup>,
    /// 绑定服务器名的配置路径（已校验为字符串或字符串数组叶子）
    pub server_paths: Vec<String>,
    /// 按 id 排序的 op
    pub ops: Vec<ResolvedOp>,
}

impl ValidatedDescriptor {
    /// 从配置值中解析绑定到哪些服务器
    ///
    /// 路径在配置里缺失、类型不对或值为空时跳过该路径；结果去重并保持有序。
    /// 这里不判定「服务器是否已注册」：注册表属于宿主，descriptor 只声明
    /// 去哪里读服务器名。
    #[must_use]
    pub fn bound_servers(&self, config: &serde_json::Value) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for path in &self.server_paths {
            let mut node = Some(config);
            for segment in path.split('.') {
                node = node.and_then(|value| value.get(segment));
            }
            let Some(node) = node else {
                continue;
            };
            match node {
                serde_json::Value::String(name) => insert_server_name(&mut names, name),
                serde_json::Value::Array(items) => {
                    for item in items {
                        if let Some(name) = item.as_str() {
                            insert_server_name(&mut names, name);
                        }
                    }
                }
                _ => {}
            }
        }
        names
    }

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
        if !(1..=DESCRIPTOR_SCHEMA_VERSION).contains(&self.schema) {
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
        let (config, setup, server_paths) = self.resolve_config_setup_and_bind()?;
        Ok(ValidatedDescriptor {
            raw: self,
            config,
            setup,
            server_paths,
            ops: resolved,
        })
    }

    /// 归一化 config schema，并据它校验 setup 字段路径与 bind 服务器路径
    fn resolve_config_setup_and_bind(&self) -> Result<ResolvedConfigSetupBind, DescriptorError> {
        let config = match &self.config {
            Some(schema) => {
                let inlined = self.inline_refs(schema)?;
                validate_schema_root(&inlined, "<config>", "config")?;
                Some(inlined)
            }
            None => None,
        };
        let setup = match &self.setup {
            Some(setup) => {
                if self.schema < 2 {
                    return Err(DescriptorError::SetupRequiresSchema { found: self.schema });
                }
                let Some(config) = &config else {
                    return Err(DescriptorError::SetupWithoutConfig);
                };
                Some(resolve_setup(setup, config)?)
            }
            None => None,
        };
        let server_paths = match &self.bind {
            Some(bind) => {
                if self.schema < 3 {
                    return Err(DescriptorError::BindRequiresSchema { found: self.schema });
                }
                let Some(config) = &config else {
                    return Err(DescriptorError::BindWithoutConfig);
                };
                resolve_bind(bind, config)?
            }
            None => Vec::new(),
        };
        Ok((config, setup, server_paths))
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

/// 解析 setup：路径必须落在 config schema 的叶子字段上
fn resolve_setup(
    setup: &Setup,
    config: &serde_json::Value,
) -> Result<ResolvedSetup, DescriptorError> {
    if setup.fields.len() > MAX_SETUP_FIELDS {
        return Err(DescriptorError::SetupTooManyFields {
            count: setup.fields.len(),
            limit: MAX_SETUP_FIELDS,
        });
    }
    let mut seen = BTreeSet::new();
    let mut fields = Vec::with_capacity(setup.fields.len());
    for field in &setup.fields {
        let path = field.path.trim();
        if path.is_empty() || path.chars().count() > MAX_SETUP_PATH_CHARS {
            return Err(DescriptorError::InvalidSetupPath {
                path: field.path.clone(),
            });
        }
        let segments = path.split('.').collect::<Vec<_>>();
        if segments.iter().any(|segment| segment.is_empty()) {
            return Err(DescriptorError::InvalidSetupPath {
                path: path.to_owned(),
            });
        }
        if !seen.insert(path.to_owned()) {
            return Err(DescriptorError::DuplicateSetupPath {
                path: path.to_owned(),
            });
        }
        fields.push(resolve_setup_field(field, path, config)?);
    }
    Ok(ResolvedSetup {
        start: setup.start,
        fields,
    })
}

/// 解析 `bind`：每条路径必须是 config schema 里指向服务器名的叶子
///
/// 只接受 `string` 与 `string[]`：服务器名是标量，多服务器场景用列表。
/// 路径与 setup 共用长度 / 分段上限，避免插件声明一串不可读的路径。
fn resolve_bind(bind: &Bind, config: &serde_json::Value) -> Result<Vec<String>, DescriptorError> {
    if bind.servers.len() > MAX_BIND_PATHS {
        return Err(DescriptorError::BindTooManyPaths {
            count: bind.servers.len(),
            limit: MAX_BIND_PATHS,
        });
    }
    let mut seen = BTreeSet::new();
    let mut paths = Vec::with_capacity(bind.servers.len());
    for raw in &bind.servers {
        let path = raw.trim();
        if path.is_empty() || path.chars().count() > MAX_SETUP_PATH_CHARS {
            return Err(DescriptorError::InvalidBindPath {
                path: raw.clone(),
                reason: format!("each path must be 1..={MAX_SETUP_PATH_CHARS} characters"),
            });
        }
        if path.split('.').any(str::is_empty) {
            return Err(DescriptorError::InvalidBindPath {
                path: path.to_owned(),
                reason: "path segments must not be empty".to_owned(),
            });
        }
        if !seen.insert(path.to_owned()) {
            return Err(DescriptorError::DuplicateBindPath {
                path: path.to_owned(),
            });
        }
        let segments = path.split('.').collect::<Vec<_>>();
        let schema = resolve_config_path(config, &segments).ok_or_else(|| {
            DescriptorError::UnknownBindPath {
                path: path.to_owned(),
            }
        })?;
        if !is_server_name_leaf(schema) {
            return Err(DescriptorError::InvalidBindPath {
                path: path.to_owned(),
                reason: "bind path must point to a string or an array of strings".to_owned(),
            });
        }
        paths.push(path.to_owned());
    }
    Ok(paths)
}

/// `bind` 路径是否描述服务器名：`string` 或 `string[]`
fn is_server_name_leaf(schema: &serde_json::Value) -> bool {
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("string") => true,
        Some("array") => {
            schema
                .get("items")
                .and_then(|items| items.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("string")
        }
        _ => false,
    }
}

/// 校验 `choicesFrom`：与 `choices` 互斥、只能用于字符串字段、不能叠加静态选项
fn check_choices_source(
    field: &SetupField,
    path: &str,
    kind: &SetupKind,
) -> Result<(), DescriptorError> {
    if field.choices_from.is_none() {
        return Ok(());
    }
    let reject = |reason: &str| {
        Err(DescriptorError::InvalidSetupChoicesFrom {
            path: path.to_owned(),
            reason: reason.to_owned(),
        })
    };
    if field.choices.is_some() {
        return reject("`choices` and `choicesFrom` are mutually exclusive");
    }
    if field.secret {
        return reject("secret fields cannot take options from the host");
    }
    match kind {
        SetupKind::Text { choices } | SetupKind::StringList { choices } if choices.is_empty() => {
            Ok(())
        }
        SetupKind::Text { .. } | SetupKind::StringList { .. } => {
            reject("field already declares fixed choices")
        }
        _ => reject("only string or string-list fields can take options from the host"),
    }
}

/// 校验单个 setup 字段，返回可直接展示与提问的解析结果
fn resolve_setup_field(
    field: &SetupField,
    path: &str,
    config: &serde_json::Value,
) -> Result<ResolvedSetupField, DescriptorError> {
    let segments = path.split('.').collect::<Vec<_>>();
    let schema = resolve_config_path(config, &segments).ok_or_else(|| {
        DescriptorError::UnknownSetupPath {
            path: path.to_owned(),
        }
    })?;
    if schema.get("type").and_then(serde_json::Value::as_str) == Some("object")
        || schema.get("properties").is_some()
    {
        return Err(DescriptorError::SetupPathNotLeaf {
            path: path.to_owned(),
        });
    }
    let kind =
        SetupKind::from_schema(schema).ok_or_else(|| DescriptorError::SetupUnsupportedKind {
            path: path.to_owned(),
            kind: schema
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        })?;
    if field.secret && !matches!(kind, SetupKind::Text { .. }) {
        return Err(DescriptorError::SetupSecretNotString {
            path: path.to_owned(),
        });
    }
    let kind = match (kind, field.choices.as_ref()) {
        (SetupKind::StringList { .. }, choices) => SetupKind::StringList {
            choices: resolve_setup_choices(path, choices)?,
        },
        (_, Some(_)) => {
            return Err(DescriptorError::SetupChoicesNotList {
                path: path.to_owned(),
            });
        }
        (kind, None) => kind,
    };
    check_choices_source(field, path, &kind)?;
    let label = field
        .label
        .clone()
        .or_else(|| {
            schema
                .get("x-mcsup-label")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| path.to_owned());
    if label.trim().is_empty() || label.chars().count() > MAX_SETUP_LABEL_CHARS {
        return Err(DescriptorError::InvalidSetupLabel {
            path: path.to_owned(),
        });
    }
    if let Some(description) = &field.description
        && description.chars().count() > MAX_SETUP_DESCRIPTION_CHARS
    {
        return Err(DescriptorError::InvalidSetupDescription {
            path: path.to_owned(),
        });
    }
    let default = field
        .default
        .clone()
        .or_else(|| schema.get("default").cloned());
    if let Some(default) = &default {
        schema::validate_instance(schema, default, path).map_err(|err| {
            DescriptorError::SetupDefaultInvalid {
                path: path.to_owned(),
                reason: err.to_string(),
            }
        })?;
    }
    Ok(ResolvedSetupField {
        path: path.to_owned(),
        label,
        description: field.description.clone().or_else(|| {
            schema
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        }),
        required: field.required,
        secret: field.secret,
        default,
        choices_from: field.choices_from,
        kind,
        schema: schema.clone(),
    })
}

/// 校验列表字段的可选值：非空、去重、长度受限
fn resolve_setup_choices(
    path: &str,
    choices: Option<&Vec<String>>,
) -> Result<Vec<String>, DescriptorError> {
    let Some(choices) = choices else {
        return Ok(Vec::new());
    };
    if choices.len() > MAX_SETUP_CHOICES {
        return Err(DescriptorError::InvalidSetupChoices {
            path: path.to_owned(),
            reason: format!("at most {MAX_SETUP_CHOICES} choices are allowed"),
        });
    }
    let mut seen = BTreeSet::new();
    for choice in choices {
        if choice.trim().is_empty() || choice.chars().count() > MAX_SETUP_CHOICE_CHARS {
            return Err(DescriptorError::InvalidSetupChoices {
                path: path.to_owned(),
                reason: format!("each choice must be 1..={MAX_SETUP_CHOICE_CHARS} characters"),
            });
        }
        if !seen.insert(choice.as_str()) {
            return Err(DescriptorError::InvalidSetupChoices {
                path: path.to_owned(),
                reason: format!("duplicate choice: {choice}"),
            });
        }
    }
    Ok(choices.clone())
}

/// 沿 `properties` 找 config schema 的某个路径
fn resolve_config_path<'a>(
    root: &'a serde_json::Value,
    segments: &[&str],
) -> Option<&'a serde_json::Value> {
    let mut node = root;
    for segment in segments {
        node = node.get("properties")?.as_object()?.get(*segment)?;
    }
    Some(node)
}

/// 服务器名去空白后非空才收进集合
fn insert_server_name(names: &mut BTreeSet<String>, name: &str) {
    let name = name.trim();
    if !name.is_empty() {
        names.insert(name.to_owned());
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
    /// schema 版本不在支持范围内
    #[error("unsupported descriptor schema {found}, host supports 1..={supported}")]
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
    /// setup 需要 schema 2
    #[error("setup requires descriptor schema 2 or newer, found {found}")]
    SetupRequiresSchema {
        /// 声明值
        found: u32,
    },
    /// 声明了 setup 但没有 config schema
    #[error("setup requires a config schema")]
    SetupWithoutConfig,
    /// bind 需要 schema 3
    #[error("bind requires descriptor schema 3 or newer, found {found}")]
    BindRequiresSchema {
        /// 声明值
        found: u32,
    },
    /// 声明了 bind 但没有 config schema
    #[error("bind requires a config schema")]
    BindWithoutConfig,
    /// bind 路径过多
    #[error("bind declares {count} server paths, limit is {limit}")]
    BindTooManyPaths {
        /// 实际数量
        count: usize,
        /// 上限
        limit: usize,
    },
    /// bind 路径非法
    #[error("invalid bind path: {path} ({reason})")]
    InvalidBindPath {
        /// 路径
        path: String,
        /// 原因
        reason: String,
    },
    /// bind 路径重复
    #[error("duplicate bind path: {path}")]
    DuplicateBindPath {
        /// 路径
        path: String,
    },
    /// bind 路径不在 config schema 中
    #[error("bind path does not exist in config schema: {path}")]
    UnknownBindPath {
        /// 路径
        path: String,
    },
    /// setup 字段过多
    #[error("setup declares {count} fields, limit is {limit}")]
    SetupTooManyFields {
        /// 实际数量
        count: usize,
        /// 上限
        limit: usize,
    },
    /// setup 路径非法
    #[error("invalid setup path: {path}")]
    InvalidSetupPath {
        /// 路径
        path: String,
    },
    /// setup 路径重复
    #[error("duplicate setup path: {path}")]
    DuplicateSetupPath {
        /// 路径
        path: String,
    },
    /// setup 路径不在 config schema 中
    #[error("setup path does not exist in config schema: {path}")]
    UnknownSetupPath {
        /// 路径
        path: String,
    },
    /// setup 路径指向对象
    #[error("setup path must point to a leaf value, not an object: {path}")]
    SetupPathNotLeaf {
        /// 路径
        path: String,
    },
    /// setup 字段类型不支持提示
    #[error("setup path {path} has unsupported type {kind}")]
    SetupUnsupportedKind {
        /// 路径
        path: String,
        /// 类型
        kind: String,
    },
    /// secret 只能用于字符串字段
    #[error("setup path {path} is secret but its type is not string")]
    SetupSecretNotString {
        /// 路径
        path: String,
    },
    /// setup 标签非法
    #[error("invalid label for setup path {path}")]
    InvalidSetupLabel {
        /// 路径
        path: String,
    },
    /// setup 描述过长
    #[error("setup description too long for {path}")]
    InvalidSetupDescription {
        /// 路径
        path: String,
    },
    /// setup 默认值不符合字段 schema
    #[error("invalid default for setup path {path}: {reason}")]
    SetupDefaultInvalid {
        /// 路径
        path: String,
        /// 原因
        reason: String,
    },
    /// `choices` 只能用于 string 列表字段
    #[error("setup path {path} declares choices but is not a string list")]
    SetupChoicesNotList {
        /// 路径
        path: String,
    },
    /// `choices` 声明非法
    #[error("invalid choices for setup path {path}: {reason}")]
    InvalidSetupChoices {
        /// 路径
        path: String,
        /// 原因
        reason: String,
    },
    /// `choicesFrom` 声明非法
    #[error("invalid choicesFrom for setup path {path}: {reason}")]
    InvalidSetupChoicesFrom {
        /// 路径
        path: String,
        /// 原因
        reason: String,
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
        assert!(descriptor.setup.is_none());
    }

    fn with_config(properties: serde_json::Value) -> serde_json::Value {
        let mut value = base();
        value["schema"] = json!(2);
        value["config"] = object_schema(properties);
        value
    }

    #[test]
    fn setup_is_resolved_against_config_schema() {
        let mut value = with_config(json!({
            "token": { "type": "string", "x-mcsup-secret": true, "x-mcsup-label": "令牌" },
            "nested": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "appId": { "type": "string" },
                    "retries": { "type": "integer", "default": 3 }
                }
            }
        }));
        value["setup"] = json!({
            "start": false,
            "fields": [
                { "path": "token", "required": true, "secret": true },
                { "path": "nested.appId", "required": true, "default": "abc" },
                { "path": "nested.retries" }
            ]
        });
        let descriptor = parse(value).expect("valid setup");
        let setup = descriptor.setup.expect("setup");
        assert!(!setup.start);
        assert_eq!(setup.fields.len(), 3);
        // 未声明 label 时取 schema 的 x-mcsup-label
        assert_eq!(setup.fields[0].label, "令牌");
        assert_eq!(
            setup.fields[0].kind,
            SetupKind::Text {
                choices: Vec::new()
            }
        );
        assert_eq!(setup.fields[1].default, Some(json!("abc")));
        assert_eq!(setup.fields[2].kind, SetupKind::Integer);
        assert_eq!(setup.fields[2].default, Some(json!(3)));
        assert_eq!(setup.fields[2].schema["type"], "integer");
    }

    #[test]
    fn setup_rejects_bad_paths_defaults_and_schema1() {
        let mut value = with_config(json!({ "token": { "type": "string" } }));
        value["setup"] = json!({ "fields": [{ "path": "missing" }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::UnknownSetupPath { .. })
        ));

        // schema 1 不认识 setup
        let mut value = base();
        value["config"] = object_schema(json!({ "token": { "type": "string" } }));
        value["setup"] = json!({ "fields": [{ "path": "token" }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SetupRequiresSchema { .. })
        ));

        // setup 只能指向叶子
        let mut value = with_config(json!({
            "nested": {
                "type": "object",
                "properties": { "a": { "type": "string" } }
            }
        }));
        value["setup"] = json!({ "fields": [{ "path": "nested" }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SetupPathNotLeaf { .. })
        ));

        // secret 只能是字符串
        let mut value = with_config(json!({ "count": { "type": "integer" } }));
        value["setup"] = json!({ "fields": [{ "path": "count", "secret": true }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SetupSecretNotString { .. })
        ));

        // 默认值必须符合字段 schema
        let mut value = with_config(json!({ "token": { "type": "string" } }));
        value["setup"] = json!({ "fields": [{ "path": "token", "default": 1 }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SetupDefaultInvalid { .. })
        ));
    }

    #[test]
    fn setup_list_choices_become_multi_select() {
        let mut value = with_config(json!({
            "platforms": { "type": "array", "items": { "type": "string" }, "default": ["qq"] }
        }));
        value["setup"] = json!({
            "fields": [{ "path": "platforms", "required": true, "choices": ["qq", "log"] }]
        });
        let descriptor = parse(value).expect("valid");
        let setup = descriptor.setup.expect("setup");
        assert_eq!(
            setup.fields[0].kind,
            SetupKind::StringList {
                choices: vec!["qq".to_owned(), "log".to_owned()]
            }
        );

        // choices 只能用于字符串列表
        let mut value = with_config(json!({ "token": { "type": "string" } }));
        value["setup"] = json!({ "fields": [{ "path": "token", "choices": ["a"] }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::SetupChoicesNotList { .. })
        ));

        // 重复选项被拒绝
        let mut value = with_config(json!({
            "platforms": { "type": "array", "items": { "type": "string" } }
        }));
        value["setup"] = json!({
            "fields": [{ "path": "platforms", "choices": ["qq", "qq"] }]
        });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidSetupChoices { .. })
        ));
    }

    #[test]
    fn setup_choices_from_servers_is_resolved() {
        let mut value = with_config(json!({
            "server": { "type": "string" },
            "servers": { "type": "array", "items": { "type": "string" } }
        }));
        value["setup"] = json!({
            "fields": [
                { "path": "server", "required": true, "choicesFrom": "servers" },
                { "path": "servers", "choicesFrom": "servers" }
            ]
        });
        let descriptor = parse(value).expect("valid");
        let setup = descriptor.setup.expect("setup");
        // 单选：仍然是字符串字段，选项由宿主枚举
        assert_eq!(
            setup.fields[0].kind,
            SetupKind::Text {
                choices: Vec::new()
            }
        );
        assert_eq!(setup.fields[0].choices_from, Some(ChoicesSource::Servers));
        // 多选：列表字段同样可以取宿主动态选项
        assert_eq!(
            setup.fields[1].kind,
            SetupKind::StringList {
                choices: Vec::new()
            }
        );
        assert_eq!(setup.fields[1].choices_from, Some(ChoicesSource::Servers));
    }

    #[test]
    fn setup_choices_from_rejects_conflicts() {
        // 与静态 choices 互斥
        let mut value = with_config(json!({
            "servers": { "type": "array", "items": { "type": "string" }, "enum": ["a"] }
        }));
        value["setup"] = json!({
            "fields": [{ "path": "servers", "choices": ["a"], "choicesFrom": "servers" }]
        });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidSetupChoicesFrom { .. })
        ));

        // 只能用于字符串 / 字符串列表
        let mut value = with_config(json!({ "count": { "type": "integer" } }));
        value["setup"] = json!({ "fields": [{ "path": "count", "choicesFrom": "servers" }] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidSetupChoicesFrom { .. })
        ));

        // secret 字段不接受宿主选项
        let mut value = with_config(json!({ "token": { "type": "string" } }));
        value["setup"] = json!({
            "fields": [{ "path": "token", "secret": true, "choicesFrom": "servers" }]
        });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidSetupChoicesFrom { .. })
        ));

        // 未知来源被拒绝
        let mut value = with_config(json!({ "server": { "type": "string" } }));
        value["setup"] = json!({ "fields": [{ "path": "server", "choicesFrom": "worlds" }] });
        assert!(parse(value).is_err());
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

    #[test]
    fn bind_resolves_server_name_paths_and_reads_config() {
        let mut value = with_config(json!({
            "server": { "type": "string" },
            "extraServers": { "type": "array", "items": { "type": "string" } },
            "nested": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "primary": { "type": "string" }
                }
            }
        }));
        value["schema"] = json!(3);
        value["bind"] = json!({ "servers": ["server", "extraServers", "nested.primary"] });
        let descriptor = parse(value).expect("valid bind");
        assert_eq!(
            descriptor.server_paths,
            vec!["server", "extraServers", "nested.primary"]
        );

        // 标量、列表、嵌套路径都参与；空值、类型不符与缺失路径被忽略
        let config = json!({
            "server": "  survival ",
            "extraServers": ["creative", "", "  survival  ", 7],
            "nested": { "primary": "skyblock" }
        });
        let bound = descriptor.bound_servers(&config);
        assert_eq!(
            bound.into_iter().collect::<Vec<_>>(),
            vec!["creative", "skyblock", "survival"]
        );

        let empty = descriptor.bound_servers(&json!({
            "server": "",
            "extraServers": [],
            "nested": {}
        }));
        assert!(empty.is_empty());
    }

    #[test]
    fn bind_rejects_bad_paths_and_old_schema() {
        // schema 2 不认识 bind
        let mut value = with_config(json!({ "server": { "type": "string" } }));
        value["bind"] = json!({ "servers": ["server"] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::BindRequiresSchema { .. })
        ));

        // 路径必须存在于 config schema
        let mut value = with_config(json!({ "server": { "type": "string" } }));
        value["schema"] = json!(3);
        value["bind"] = json!({ "servers": ["missing"] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::UnknownBindPath { .. })
        ));

        // 只接受字符串或字符串数组
        let mut value = with_config(json!({ "retries": { "type": "integer" } }));
        value["schema"] = json!(3);
        value["bind"] = json!({ "servers": ["retries"] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::InvalidBindPath { .. })
        ));

        // 重复路径
        let mut value = with_config(json!({ "server": { "type": "string" } }));
        value["schema"] = json!(3);
        value["bind"] = json!({ "servers": ["server", " server "] });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::DuplicateBindPath { .. })
        ));

        // 路径条数上限
        let mut value = with_config(json!({ "server": { "type": "string" } }));
        value["schema"] = json!(3);
        let many = (0..=MAX_BIND_PATHS)
            .map(|_| "server".to_owned())
            .collect::<Vec<_>>();
        value["bind"] = json!({ "servers": many });
        assert!(matches!(
            parse(value),
            Err(DescriptorError::BindTooManyPaths { .. })
        ));
    }
}
