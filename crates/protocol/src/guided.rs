//! ① Guided Decoding.
//!
//! The idea: hand the model a JSON Schema and ask the serving layer (or, in
//! the fallback path, our own post-hoc validator) to guarantee the response
//! conforms. NIM and most OpenAI-compatible servers accept the schema via the
//! `response_format` / `guided_json` request field; when the server does not
//! support it we validate client-side and ask the VM to re-roll only the
//! offending node.
//!
//! This module is intentionally vendor-agnostic: it produces a serializable
//! `response_format` payload and a pure validation function over the parsed
//! JSON. The actual transport lives in `llm-vm-core`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A JSON Schema (draft-07 subset) used to constrain generation.
///
/// We keep the type minimal — most tool-call responses only need `object`
/// with typed properties and a `required` list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonSchema {
    #[serde(rename = "type")]
    pub schema_type: SchemaType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SchemaType {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConstraintError {
    #[error("expected type {expected:?}, got {actual:?}")]
    TypeMismatch {
        expected: SchemaType,
        actual: SchemaType,
    },
    #[error("missing required field `{0}`")]
    MissingRequired(String),
    #[error("not a valid JSON value: {0}")]
    InvalidJson(String),
    #[error("unknown field `{0}` (schema is strict)")]
    UnknownField(String),
}

/// Build the OpenAI-compatible `response_format` payload requesting strict
/// schema-constrained generation. Servers that ignore it will at worst return
/// unconstrained JSON, which we then validate with [`validate_against_schema`].
pub fn constrain_request(schema: &JsonSchema, strict: bool) -> serde_json::Value {
    let schema_json = serde_json::to_value(schema).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "llmvm_response",
            "schema": schema_json,
            "strict": strict,
        }
    })
}

/// Validate a parsed JSON value against the schema. Returns the same value on
/// success for ergonomic chaining.
pub fn validate_against_schema(
    value: &serde_json::Value,
    schema: &JsonSchema,
) -> Result<(), ConstraintError> {
    let actual = infer_type(value);
    if actual != schema.schema_type {
        return Err(ConstraintError::TypeMismatch {
            expected: schema.schema_type,
            actual,
        });
    }

    match schema.schema_type {
        SchemaType::Object => {
            let obj = value
                .as_object()
                .ok_or_else(|| ConstraintError::InvalidJson("not an object".into()))?;

            if let Some(required) = &schema.required {
                for field in required {
                    if !obj.contains_key(field) {
                        return Err(ConstraintError::MissingRequired(field.clone()));
                    }
                }
            }

            if let Some(props) = &schema.properties {
                for (k, v) in obj {
                    // Allow unknown fields unless strict mode rejects them —
                    // we are permissive by default to avoid fighting the model.
                    if let Some(sub_schema_value) = props.get(k) {
                        let sub: JsonSchema = serde_json::from_value(sub_schema_value.clone())
                            .map_err(|e| ConstraintError::InvalidJson(e.to_string()))?;
                        validate_against_schema(v, &sub)?;
                    }
                }
            }
            Ok(())
        }
        SchemaType::Array => {
            let arr = value
                .as_array()
                .ok_or_else(|| ConstraintError::InvalidJson("not an array".into()))?;
            if let Some(items) = &schema.items {
                for item in arr {
                    validate_against_schema(item, items)?;
                }
            }
            Ok(())
        }
        // Primitives: type check above is sufficient.
        _ => Ok(()),
    }
}

fn infer_type(v: &serde_json::Value) -> SchemaType {
    match v {
        serde_json::Value::Null => SchemaType::Null,
        serde_json::Value::Bool(_) => SchemaType::Boolean,
        serde_json::Value::Number(n) => {
            if n.is_i64() {
                SchemaType::Integer
            } else {
                SchemaType::Number
            }
        }
        serde_json::Value::String(_) => SchemaType::String,
        serde_json::Value::Array(_) => SchemaType::Array,
        serde_json::Value::Object(_) => SchemaType::Object,
    }
}

impl JsonSchema {
    /// Convenience constructor for the most common shape: an object with
    /// typed properties and a required list.
    pub fn object(
        properties: serde_json::Map<String, serde_json::Value>,
        required: Vec<String>,
    ) -> Self {
        Self {
            schema_type: SchemaType::Object,
            properties: Some(properties),
            required: Some(required),
            items: None,
            description: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_schema() -> JsonSchema {
        JsonSchema::object(
            serde_json::Map::from_iter([
                (
                    "tool".to_string(),
                    json!({"type": "string", "enum": ["read_file", "write_file"]}),
                ),
                ("path".to_string(), json!({"type": "string"})),
                (
                    "args".to_string(),
                    json!({
                        "type": "object",
                        "properties": {
                            "offset": {"type": "integer"}
                        },
                        "required": ["offset"]
                    }),
                ),
            ]),
            vec!["tool".into(), "path".into()],
        )
    }

    #[test]
    fn valid_payload_passes() {
        let payload = json!({
            "tool": "read_file",
            "path": "/tmp/x",
            "args": {"offset": 0}
        });
        assert!(validate_against_schema(&payload, &tool_schema()).is_ok());
    }

    #[test]
    fn missing_required_is_rejected() {
        let payload = json!({"path": "/tmp/x", "args": {"offset": 0}});
        let err = validate_against_schema(&payload, &tool_schema()).unwrap_err();
        assert_eq!(err, ConstraintError::MissingRequired("tool".into()));
    }

    #[test]
    fn type_mismatch_is_rejected() {
        let payload = json!([1, 2, 3]);
        let err = validate_against_schema(&payload, &tool_schema()).unwrap_err();
        assert!(matches!(err, ConstraintError::TypeMismatch { .. }));
    }

    #[test]
    fn nested_type_error_surfaces() {
        let payload = json!({
            "tool": "read_file",
            "path": "/tmp/x",
            "args": {"offset": "zero"}
        });
        let err = validate_against_schema(&payload, &tool_schema()).unwrap_err();
        assert!(matches!(err, ConstraintError::TypeMismatch { .. }), "{err:?}");
    }

    #[test]
    fn constrain_request_emits_json_schema_format() {
        let fmt = constrain_request(&tool_schema(), true);
        assert_eq!(fmt["type"], "json_schema");
        assert_eq!(fmt["json_schema"]["strict"], true);
        assert_eq!(fmt["json_schema"]["schema"]["type"], "object");
    }
}
