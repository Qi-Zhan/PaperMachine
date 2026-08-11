use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundarySchema {
    pub kind: SchemaKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchemaKind {
    Any,
    String,
    Bool,
    Int,
    Number,
    List {
        items: Box<BoundarySchema>,
    },
    Map {
        values: Box<BoundarySchema>,
    },
    Object {
        fields: Vec<SchemaField>,
        #[serde(default)]
        allow_extra: bool,
    },
    Enum {
        values: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaField {
    pub name: String,
    pub schema: BoundarySchema,
    #[serde(default)]
    pub optional: bool,
}

impl BoundarySchema {
    pub const fn new(kind: SchemaKind) -> Self {
        Self {
            kind,
            default: None,
            title: None,
            description: None,
            format: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
        }
    }

    pub fn object(fields: Vec<SchemaField>) -> Self {
        Self::new(SchemaKind::Object {
            fields,
            allow_extra: false,
        })
    }

    pub fn top_level_kind(&self) -> &'static str {
        match self.kind {
            SchemaKind::Any => "object",
            SchemaKind::String => "string",
            SchemaKind::Bool => "boolean",
            SchemaKind::Int => "integer",
            SchemaKind::Number => "number",
            SchemaKind::List { .. } => "array",
            SchemaKind::Map { .. } | SchemaKind::Object { .. } => "object",
            SchemaKind::Enum { ref values } => values.first().map_or("string", json_kind),
        }
    }

    pub fn to_json_schema(&self) -> Value {
        let mut object = match &self.kind {
            SchemaKind::Any => Map::new(),
            SchemaKind::String => map_with_type("string"),
            SchemaKind::Bool => map_with_type("boolean"),
            SchemaKind::Int => map_with_type("integer"),
            SchemaKind::Number => map_with_type("number"),
            SchemaKind::List { items } => {
                let mut value = map_with_type("array");
                value.insert("items".to_string(), items.to_json_schema());
                value
            }
            SchemaKind::Map { values } => {
                let mut value = map_with_type("object");
                value.insert("additionalProperties".to_string(), values.to_json_schema());
                value
            }
            SchemaKind::Object {
                fields,
                allow_extra,
            } => {
                let mut value = map_with_type("object");
                let mut properties = Map::new();
                let mut required = Vec::new();
                for (index, field) in fields.iter().enumerate() {
                    let mut property = field.schema.to_json_schema();
                    if let Some(property) = property.as_object_mut() {
                        property
                            .entry("x-ui-order".to_string())
                            .or_insert_with(|| json!(index));
                    }
                    properties.insert(field.name.clone(), property);
                    if !field.optional && field.schema.default.is_none() {
                        required.push(Value::String(field.name.clone()));
                    }
                }
                value.insert("properties".to_string(), Value::Object(properties));
                value.insert("required".to_string(), Value::Array(required));
                value.insert("additionalProperties".to_string(), json!(allow_extra));
                value
            }
            SchemaKind::Enum { values } => {
                let mut value = values
                    .first()
                    .map(json_kind)
                    .filter(|kind| *kind != "null")
                    .map(map_with_type)
                    .unwrap_or_default();
                value.insert("enum".to_string(), Value::Array(values.clone()));
                value
            }
        };
        if let Some(default) = &self.default {
            object.insert("default".to_string(), default.clone());
        }
        if let Some(title) = &self.title {
            object.insert("title".to_string(), Value::String(title.clone()));
        }
        if let Some(description) = &self.description {
            object.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
        }
        if let Some(format) = &self.format {
            object.insert("format".to_string(), Value::String(format.clone()));
        }
        if let Some(minimum) = self.minimum {
            object.insert("minimum".to_string(), json_number(minimum));
        }
        if let Some(maximum) = self.maximum {
            object.insert("maximum".to_string(), json_number(maximum));
        }
        let (minimum_key, maximum_key) = match self.kind {
            SchemaKind::List { .. } => ("minItems", "maxItems"),
            SchemaKind::Map { .. } | SchemaKind::Object { .. } => {
                ("minProperties", "maxProperties")
            }
            _ => ("minLength", "maxLength"),
        };
        if let Some(min_length) = self.min_length {
            object.insert(minimum_key.to_string(), json!(min_length));
        }
        if let Some(max_length) = self.max_length {
            object.insert(maximum_key.to_string(), json!(max_length));
        }
        Value::Object(object)
    }

    pub fn validate(&self, value: &Value, path: &str) -> Result<(), String> {
        validate_json_schema_value(&self.to_json_schema(), value, path)
    }

    pub fn apply_defaults(&self, value: &mut Value) {
        if value.is_null()
            && let Some(default) = &self.default
        {
            *value = default.clone();
        }
        match (&self.kind, value) {
            (SchemaKind::Object { fields, .. }, Value::Object(object)) => {
                for field in fields {
                    if !object.contains_key(&field.name)
                        && let Some(default) = &field.schema.default
                    {
                        object.insert(field.name.clone(), default.clone());
                    }
                    if let Some(value) = object.get_mut(&field.name) {
                        field.schema.apply_defaults(value);
                    }
                }
            }
            (SchemaKind::List { items }, Value::Array(values)) => {
                for value in values {
                    items.apply_defaults(value);
                }
            }
            (SchemaKind::Map { values }, Value::Object(object)) => {
                for value in object.values_mut() {
                    values.apply_defaults(value);
                }
            }
            _ => {}
        }
    }
}

/// Validate a JSON value against the controlled JSON Schema subset emitted by
/// Workflow boundary schemas and accepted for HumanRequest responses.
pub fn validate_json_schema_value(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path} schema must be an object"))?;
    if object.is_empty() {
        return Ok(());
    }
    if let Some(allowed) = object.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not one of the allowed values"));
    }
    match object.get("type").and_then(Value::as_str) {
        None => {}
        Some("object") => {
            let value = value
                .as_object()
                .ok_or_else(|| format!("{path} must be an object"))?;
            validate_size(object, value.len(), "minProperties", "maxProperties", path)?;
            let properties = object.get("properties").and_then(Value::as_object);
            if let Some(required) = object.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !value.contains_key(key) {
                        return Err(format!("{path}.{key} is required"));
                    }
                }
            }
            for (key, child) in value {
                if let Some(schema) = properties.and_then(|properties| properties.get(key)) {
                    validate_json_schema_value(schema, child, &format!("{path}.{key}"))?;
                } else {
                    match object.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            return Err(format!("{path}.{key} is not an allowed field"));
                        }
                        Some(schema @ Value::Object(_)) => {
                            validate_json_schema_value(schema, child, &format!("{path}.{key}"))?;
                        }
                        _ => {}
                    }
                }
            }
        }
        Some("array") => {
            let value = value
                .as_array()
                .ok_or_else(|| format!("{path} must be an array"))?;
            validate_size(object, value.len(), "minItems", "maxItems", path)?;
            if let Some(items) = object.get("items") {
                for (index, item) in value.iter().enumerate() {
                    validate_json_schema_value(items, item, &format!("{path}[{index}]"))?;
                }
            }
        }
        Some("string") => {
            let value = value
                .as_str()
                .ok_or_else(|| format!("{path} must be a string"))?;
            validate_size(
                object,
                value.chars().count(),
                "minLength",
                "maxLength",
                path,
            )?;
        }
        Some("integer") => {
            if !value.is_i64() && !value.is_u64() {
                return Err(format!("{path} must be an integer"));
            }
            validate_numeric_bounds(object, value, path)?;
        }
        Some("number") => {
            if !value.is_number() {
                return Err(format!("{path} must be a number"));
            }
            validate_numeric_bounds(object, value, path)?;
        }
        Some("boolean") if !value.is_boolean() => {
            return Err(format!("{path} must be a boolean"));
        }
        Some("null") if !value.is_null() => return Err(format!("{path} must be null")),
        Some("boolean" | "null") => {}
        Some(other) => return Err(format!("{path} schema has unsupported type `{other}`")),
    }
    Ok(())
}

/// Validate that a schema itself stays inside PaperMachine's controlled subset.
pub fn validate_json_schema_definition(schema: &Value, path: &str) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path} must be a schema object"))?;
    if object.is_empty() {
        return Ok(());
    }
    const ALLOWED_KEYS: &[&str] = &[
        "type",
        "enum",
        "default",
        "title",
        "description",
        "format",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "items",
        "properties",
        "required",
        "additionalProperties",
        "x-ui-order",
    ];
    if let Some(key) = object
        .keys()
        .find(|key| !ALLOWED_KEYS.contains(&key.as_str()))
    {
        return Err(format!("{path} has unsupported schema keyword `{key}`"));
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| format!("{path}.enum must be a non-empty array"))?;
        let kind = json_kind(&values[0]);
        if matches!(kind, "array" | "object" | "null")
            || values.iter().any(|value| json_kind(value) != kind)
        {
            return Err(format!("{path}.enum must contain one scalar JSON kind"));
        }
    }
    match object.get("type").and_then(Value::as_str) {
        None if object.contains_key("enum") => {}
        None => return Err(format!("{path} requires `type` or `enum`")),
        Some("object") => {
            let properties = object.get("properties").and_then(Value::as_object);
            if let Some(properties) = properties {
                for (name, child) in properties {
                    validate_json_schema_definition(child, &format!("{path}.properties.{name}"))?;
                }
            }
            if let Some(required) = object.get("required") {
                let required = required
                    .as_array()
                    .ok_or_else(|| format!("{path}.required must be an array"))?;
                for name in required {
                    let name = name
                        .as_str()
                        .ok_or_else(|| format!("{path}.required must contain strings"))?;
                    if !properties.is_some_and(|properties| properties.contains_key(name)) {
                        return Err(format!(
                            "{path}.required field `{name}` has no property schema"
                        ));
                    }
                }
            }
            if let Some(additional) = object.get("additionalProperties") {
                match additional {
                    Value::Bool(_) => {}
                    Value::Object(_) => validate_json_schema_definition(
                        additional,
                        &format!("{path}.additionalProperties"),
                    )?,
                    _ => {
                        return Err(format!(
                            "{path}.additionalProperties must be bool or schema"
                        ));
                    }
                }
            }
            validate_bound_pair(object, "minProperties", "maxProperties", path)?;
        }
        Some("array") => {
            let items = object
                .get("items")
                .ok_or_else(|| format!("{path}.items is required"))?;
            validate_json_schema_definition(items, &format!("{path}.items"))?;
            validate_bound_pair(object, "minItems", "maxItems", path)?;
        }
        Some("string") => validate_bound_pair(object, "minLength", "maxLength", path)?,
        Some("integer" | "number") => {
            for key in ["minimum", "maximum"] {
                if object
                    .get(key)
                    .is_some_and(|value| value.as_f64().is_none())
                {
                    return Err(format!("{path}.{key} must be a number"));
                }
            }
            if let (Some(minimum), Some(maximum)) = (
                object.get("minimum").and_then(Value::as_f64),
                object.get("maximum").and_then(Value::as_f64),
            ) && minimum > maximum
            {
                return Err(format!("{path}.minimum exceeds maximum"));
            }
        }
        Some("boolean" | "null") => {}
        Some(other) => return Err(format!("{path} has unsupported type `{other}`")),
    }
    if let Some(default) = object.get("default") {
        validate_json_schema_value(schema, default, &format!("{path}.default"))?;
    }
    Ok(())
}

/// Apply declared defaults without inventing values not present in the schema.
pub fn apply_json_schema_defaults(schema: &Value, value: &mut Value) {
    if value.is_null()
        && let Some(default) = schema.get("default")
    {
        *value = default.clone();
    }
    match (schema.get("type").and_then(Value::as_str), value) {
        (Some("object"), Value::Object(object)) => {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, child_schema) in properties {
                    if !object.contains_key(name)
                        && let Some(default) = child_schema.get("default")
                    {
                        object.insert(name.clone(), default.clone());
                    }
                    if let Some(child) = object.get_mut(name) {
                        apply_json_schema_defaults(child_schema, child);
                    }
                }
            }
            if let Some(additional) = schema
                .get("additionalProperties")
                .filter(|value| value.is_object())
            {
                let known = schema.get("properties").and_then(Value::as_object);
                for (name, child) in object.iter_mut() {
                    if !known.is_some_and(|properties| properties.contains_key(name)) {
                        apply_json_schema_defaults(additional, child);
                    }
                }
            }
        }
        (Some("array"), Value::Array(values)) => {
            if let Some(items) = schema.get("items") {
                for child in values {
                    apply_json_schema_defaults(items, child);
                }
            }
        }
        _ => {}
    }
}

fn validate_bound_pair(
    schema: &Map<String, Value>,
    minimum_key: &str,
    maximum_key: &str,
    path: &str,
) -> Result<(), String> {
    for key in [minimum_key, maximum_key] {
        if schema
            .get(key)
            .is_some_and(|value| value.as_u64().is_none())
        {
            return Err(format!("{path}.{key} must be a non-negative integer"));
        }
    }
    if let (Some(minimum), Some(maximum)) = (
        schema.get(minimum_key).and_then(Value::as_u64),
        schema.get(maximum_key).and_then(Value::as_u64),
    ) && minimum > maximum
    {
        return Err(format!("{path}.{minimum_key} exceeds {maximum_key}"));
    }
    Ok(())
}

fn validate_size(
    schema: &Map<String, Value>,
    actual: usize,
    minimum_key: &str,
    maximum_key: &str,
    path: &str,
) -> Result<(), String> {
    if schema
        .get(minimum_key)
        .and_then(Value::as_u64)
        .is_some_and(|minimum| actual < minimum as usize)
    {
        return Err(format!("{path} is shorter than {minimum_key}"));
    }
    if schema
        .get(maximum_key)
        .and_then(Value::as_u64)
        .is_some_and(|maximum| actual > maximum as usize)
    {
        return Err(format!("{path} exceeds {maximum_key}"));
    }
    Ok(())
}

fn validate_numeric_bounds(
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), String> {
    let value = value
        .as_f64()
        .ok_or_else(|| format!("{path} is outside the supported numeric range"))?;
    if schema
        .get("minimum")
        .and_then(Value::as_f64)
        .is_some_and(|minimum| value < minimum)
    {
        return Err(format!("{path} is less than the minimum"));
    }
    if schema
        .get("maximum")
        .and_then(Value::as_f64)
        .is_some_and(|maximum| value > maximum)
    {
        return Err(format!("{path} exceeds the maximum"));
    }
    Ok(())
}

fn map_with_type(kind: &str) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("type".to_string(), Value::String(kind.to_string()));
    value
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(value) if value.is_i64() || value.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn json_number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_nested_objects_and_applies_defaults() {
        let schema = BoundarySchema::object(vec![
            SchemaField {
                name: "name".to_string(),
                schema: BoundarySchema::new(SchemaKind::String),
                optional: false,
            },
            SchemaField {
                name: "count".to_string(),
                schema: BoundarySchema {
                    default: Some(json!(2)),
                    ..BoundarySchema::new(SchemaKind::Int)
                },
                optional: false,
            },
        ]);
        let mut value = json!({"name":"route"});
        schema.apply_defaults(&mut value);
        assert_eq!(value, json!({"name":"route","count":2}));
        schema
            .validate(&value, "value")
            .expect("value should match");
        assert!(schema.validate(&json!({"name":1}), "value").is_err());
    }
}
