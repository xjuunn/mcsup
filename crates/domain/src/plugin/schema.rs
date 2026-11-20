//! 配置值的实例校验与密钥打码
//!
//! manifest 的 `config` 与 op 的 `input` / `output` 是同一个 schema 子集
//! （`docs/plugins.md` 11.5）。这里只做**实例**校验：schema 形状在安装阶段
//! 已由 [`crate::plugin::descriptor`] 校验过，运行期只回答「这组值是否满足
//! 声明」。
//!
//! 校验规则与安装阶段共用同一张关键字表；`oneOf` / `$ref` 等构造在安装时
//! 已被拒绝，到这里只可能是已知子集。遇到未知关键字按注解处理（JSON Schema
//! 的默认语义），不因为宿主更新而拒绝旧插件。
//!
//! `x-mcsup-secret` 的语义在这里收敛：字段值可以写、可以向插件下发，但任何
//! 面向入口的读取都必须先经 [`mask_secrets`] 变成 `***`。

use serde_json::{Map, Value};

/// 密钥字段的打码文本
pub const SECRET_MASK: &str = "***";

/// 实例校验失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("config {path}: {reason}")]
pub struct SchemaError {
    /// 字段路径，如 `values.token`
    pub path: String,
    /// 拒绝原因
    pub reason: String,
}

impl SchemaError {
    fn new(path: &str, reason: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            reason: reason.into(),
        }
    }
}

/// 按 schema 校验一个值
///
/// `path` 是根路径，用于错误信息（配置传 `config`）。成功返回 `Ok(())`。
///
/// # Errors
///
/// 类型不符、必填缺失、未知字段、枚举越界或数值 / 长度 / 数组约束不满足时
/// 返回 [`SchemaError`]，并带上出错字段的路径。
pub fn validate_instance(schema: &Value, value: &Value, path: &str) -> Result<(), SchemaError> {
    validate(schema, value, path)
}

fn validate(schema: &Value, value: &Value, path: &str) -> Result<(), SchemaError> {
    let Some(map) = schema.as_object() else {
        // 非对象 schema 在安装阶段已被拒绝；这里按「无约束」处理，避免
        // 运行期因为一份旧 descriptor 把配置锁死。
        return Ok(());
    };
    if let Some(values) = map.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(SchemaError::new(
            path,
            "value is not one of the declared enum values",
        ));
    }
    match map.get("type").and_then(Value::as_str) {
        Some("object") => validate_object(map, value, path),
        Some("array") => validate_array(map, value, path),
        Some("string") => validate_string(map, value, path),
        Some("number") => validate_number(map, value, path, false),
        Some("integer") => validate_number(map, value, path, true),
        Some("boolean") => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(SchemaError::new(path, "expected a boolean"))
            }
        }
        // 没有 `type`：只可能是纯 `enum` / 纯注解，已在上面处理。
        _ => Ok(()),
    }
}

fn validate_object(map: &Map<String, Value>, value: &Value, path: &str) -> Result<(), SchemaError> {
    let Some(object) = value.as_object() else {
        return Err(SchemaError::new(path, "expected an object"));
    };
    let properties = map.get("properties").and_then(Value::as_object);
    if let Some(required) = map.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(SchemaError::new(
                    &join(path, name),
                    "required field is missing",
                ));
            }
        }
    }
    let additional = map.get("additionalProperties");
    for (name, item) in object {
        match properties.and_then(|properties| properties.get(name)) {
            Some(child) => validate(child, item, &join(path, name))?,
            None if additional == Some(&Value::Bool(false)) => {
                return Err(SchemaError::new(
                    &join(path, name),
                    "unknown field (additionalProperties is false)",
                ));
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_array(map: &Map<String, Value>, value: &Value, path: &str) -> Result<(), SchemaError> {
    let Some(items) = value.as_array() else {
        return Err(SchemaError::new(path, "expected an array"));
    };
    if let Some(min) = map.get("minItems").and_then(Value::as_u64)
        && items.len() < usize::try_from(min).unwrap_or(usize::MAX)
    {
        return Err(SchemaError::new(
            path,
            format!("array has {} items; minimum is {min}", items.len()),
        ));
    }
    if let Some(max) = map.get("maxItems").and_then(Value::as_u64)
        && items.len() > usize::try_from(max).unwrap_or(0)
    {
        return Err(SchemaError::new(
            path,
            format!("array has {} items; maximum is {max}", items.len()),
        ));
    }
    if let Some(item_schema) = map.get("items") {
        for (index, item) in items.iter().enumerate() {
            validate(item_schema, item, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn validate_string(map: &Map<String, Value>, value: &Value, path: &str) -> Result<(), SchemaError> {
    let Some(text) = value.as_str() else {
        return Err(SchemaError::new(path, "expected a string"));
    };
    let length = text.chars().count();
    if let Some(min) = map.get("minLength").and_then(Value::as_u64)
        && length < usize::try_from(min).unwrap_or(usize::MAX)
    {
        return Err(SchemaError::new(
            path,
            format!("string has {length} characters; minimum is {min}"),
        ));
    }
    if let Some(max) = map.get("maxLength").and_then(Value::as_u64)
        && length > usize::try_from(max).unwrap_or(0)
    {
        return Err(SchemaError::new(
            path,
            format!("string has {length} characters; maximum is {max}"),
        ));
    }
    if let Some(pattern) = map.get("pattern").and_then(Value::as_str) {
        // 安装阶段已按同一套 schema 子集校验过；pattern 在运行期才真正使用，
        // 非法表达式按 schema 错误报告，而不是静默放行。
        let compiled = regex::Regex::new(pattern)
            .map_err(|err| SchemaError::new(path, format!("invalid pattern: {err}")))?;
        if !compiled.is_match(text) {
            return Err(SchemaError::new(
                path,
                "string does not match the declared pattern",
            ));
        }
    }
    Ok(())
}

fn validate_number(
    map: &Map<String, Value>,
    value: &Value,
    path: &str,
    integer: bool,
) -> Result<(), SchemaError> {
    let number = if integer {
        if value.as_i64().is_none() && value.as_u64().is_none() {
            return Err(SchemaError::new(path, "expected an integer"));
        }
        value.as_f64().unwrap_or_default()
    } else {
        let Some(number) = value.as_f64() else {
            return Err(SchemaError::new(path, "expected a number"));
        };
        number
    };
    check_bound(map, path, number, "minimum", |number, bound| number < bound)?;
    check_bound(map, path, number, "exclusiveMinimum", |number, bound| {
        number <= bound
    })?;
    check_bound(map, path, number, "maximum", |number, bound| number > bound)?;
    check_bound(map, path, number, "exclusiveMaximum", |number, bound| {
        number >= bound
    })?;
    Ok(())
}

fn check_bound(
    map: &Map<String, Value>,
    path: &str,
    number: f64,
    keyword: &str,
    violated: impl Fn(f64, f64) -> bool,
) -> Result<(), SchemaError> {
    let Some(bound) = map.get(keyword).and_then(Value::as_f64) else {
        return Ok(());
    };
    if violated(number, bound) {
        return Err(SchemaError::new(
            path,
            format!("{number} violates {keyword} {bound}"),
        ));
    }
    Ok(())
}

fn join(path: &str, field: &str) -> String {
    if path.is_empty() {
        field.to_owned()
    } else {
        format!("{path}.{field}")
    }
}

/// schema 声明的默认值（递归取对象属性）
///
/// 只取 `default`，不臆造类型默认值：没写默认就是「未设置」，由调用方决定
/// 是否必填。返回对象；schema 顶层不是对象时返回空对象。
#[must_use]
pub fn defaults(schema: &Value) -> Value {
    let mut object = Map::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            if let Some(default) = child.get("default") {
                object.insert(name.clone(), default.clone());
            } else if child.get("type").and_then(Value::as_str) == Some("object") {
                let nested = defaults(child);
                if nested.as_object().is_some_and(|map| !map.is_empty()) {
                    object.insert(name.clone(), nested);
                }
            }
        }
    }
    Value::Object(object)
}

/// 把 schema 标记为密钥的字段替换成 `***`
///
/// 只处理输入方向（配置与 op 输入）；输出方向在安装阶段就禁止
/// `x-mcsup-secret`，因此这里不需要考虑输出。
#[must_use]
pub fn mask_secrets(schema: &Value, value: &Value) -> Value {
    mask(schema, value)
}

fn mask(schema: &Value, value: &Value) -> Value {
    let Some(map) = schema.as_object() else {
        return value.clone();
    };
    match map.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(object) = value.as_object() else {
                return value.clone();
            };
            let properties = map.get("properties").and_then(Value::as_object);
            let mut out = Map::with_capacity(object.len());
            for (name, item) in object {
                let masked = match properties.and_then(|properties| properties.get(name)) {
                    Some(child) if is_secret(child) => Value::String(SECRET_MASK.to_owned()),
                    Some(child) => mask(child, item),
                    None => item.clone(),
                };
                out.insert(name.clone(), masked);
            }
            Value::Object(out)
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return value.clone();
            };
            let Some(item_schema) = map.get("items") else {
                return value.clone();
            };
            Value::Array(items.iter().map(|item| mask(item_schema, item)).collect())
        }
        _ => value.clone(),
    }
}

fn is_secret(schema: &Value) -> bool {
    schema
        .get("x-mcsup-secret")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 找出 schema 中标记为密钥的顶层字段
#[must_use]
pub fn secret_keys(schema: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            if is_secret(child) {
                keys.push(name.clone());
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["interval_secs"],
            "properties": {
                "interval_secs": { "type": "integer", "minimum": 5, "maximum": 3600 },
                "token": { "type": "string", "x-mcsup-secret": true },
                "channels": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["global", "local"] },
                    "minItems": 1
                },
                "mode": { "type": "string", "pattern": "^[a-z]+$" }
            }
        })
    }

    #[test]
    fn valid_values_pass() {
        let value = json!({
            "interval_secs": 60,
            "token": "s3cret",
            "channels": ["global"],
            "mode": "fast"
        });
        validate_instance(&config_schema(), &value, "config").expect("valid");
    }

    #[test]
    fn missing_required_and_unknown_fields_are_rejected() {
        let err = validate_instance(&config_schema(), &json!({}), "config").expect_err("required");
        assert_eq!(err.path, "config.interval_secs");
        let err = validate_instance(
            &config_schema(),
            &json!({ "interval_secs": 60, "extra": 1 }),
            "config",
        )
        .expect_err("unknown");
        assert_eq!(err.path, "config.extra");
    }

    #[test]
    fn type_and_range_violations_name_the_field() {
        let err = validate_instance(
            &config_schema(),
            &json!({ "interval_secs": 60.5 }),
            "config",
        )
        .expect_err("integer");
        assert_eq!(err.path, "config.interval_secs");
        let err = validate_instance(&config_schema(), &json!({ "interval_secs": 1 }), "config")
            .expect_err("minimum");
        assert_eq!(err.path, "config.interval_secs");
        assert!(err.reason.contains("minimum"));
    }

    #[test]
    fn enum_pattern_and_array_limits_are_enforced() {
        let err = validate_instance(
            &config_schema(),
            &json!({ "interval_secs": 60, "mode": "Fast" }),
            "config",
        )
        .expect_err("pattern");
        assert_eq!(err.path, "config.mode");
        let err = validate_instance(
            &config_schema(),
            &json!({ "interval_secs": 60, "channels": ["elsewhere"] }),
            "config",
        )
        .expect_err("enum");
        assert_eq!(err.path, "config.channels[0]");
        let err = validate_instance(
            &config_schema(),
            &json!({ "interval_secs": 60, "channels": [] }),
            "config",
        )
        .expect_err("minItems");
        assert_eq!(err.path, "config.channels");
    }

    #[test]
    fn masking_replaces_only_secret_fields() {
        let value = json!({
            "interval_secs": 60,
            "token": "s3cret",
            "channels": ["global"]
        });
        let masked = mask_secrets(&config_schema(), &value);
        assert_eq!(masked["token"], SECRET_MASK);
        assert_eq!(masked["interval_secs"], 60);
        assert_eq!(masked["channels"], json!(["global"]));
        assert_eq!(secret_keys(&config_schema()), vec!["token".to_owned()]);
    }

    #[test]
    fn defaults_are_collected_from_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "interval_secs": { "type": "integer", "default": 60 },
                "nested": {
                    "type": "object",
                    "properties": { "enabled": { "type": "boolean", "default": true } }
                }
            }
        });
        assert_eq!(
            defaults(&schema),
            json!({ "interval_secs": 60, "nested": { "enabled": true } })
        );
    }
}
