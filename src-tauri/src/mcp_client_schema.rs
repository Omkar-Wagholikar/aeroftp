//! Backend-owned, fail-closed subset matching the MCP registry contract.
// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use std::collections::HashSet;

// Includes our 77 primary/compatibility names while keeping discovery bounded.
const MAX_TOOLS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaError {
    Unavailable,
    Unsupported,
    Arguments,
}

fn keys(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str())))
}
fn text(value: Option<&Value>, max: usize) -> bool {
    value.is_none_or(|v| {
        v.as_str()
            .is_some_and(|s| s.len() <= max && !s.chars().any(char::is_control))
    })
}
fn parameter(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && (name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// No caller-supplied schema enters this function: only a fresh tools/list reply.
/// Incomplete/paginated catalogs and duplicate names are unavailable.
pub(crate) fn discover(result: &Value, name: &str) -> Result<Value, SchemaError> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or(SchemaError::Unavailable)?;
    if tools.len() > MAX_TOOLS
        || result.get("nextCursor").is_some()
        || name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
        || !name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(SchemaError::Unavailable);
    }
    let matching: Vec<_> = tools
        .iter()
        .filter(|t| t.get("name").and_then(Value::as_str) == Some(name))
        .collect();
    if matching.len() != 1 {
        return Err(SchemaError::Unavailable);
    }
    let tool = matching[0];
    if !text(tool.get("description"), 512) {
        return Err(SchemaError::Unsupported);
    }
    let schema = tool.get("inputSchema").ok_or(SchemaError::Unsupported)?;
    validate_schema(schema)?;
    Ok(schema.clone())
}

fn validate_schema(schema: &Value) -> Result<(), SchemaError> {
    let invalid = SchemaError::Unsupported;
    if serde_json::to_vec(schema).map_err(|_| invalid)?.len() > 8192
        || !keys(
            schema,
            &[
                "type",
                "properties",
                "required",
                "additionalProperties",
                "description",
            ],
        )
        || schema.get("type").and_then(Value::as_str) != Some("object")
        || !text(schema.get("description"), 512)
        || schema
            .get("additionalProperties")
            .is_some_and(|v| v != &Value::Bool(false))
    {
        return Err(invalid);
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or(invalid)?;
    if properties.len() > 16 || !properties.keys().all(|k| parameter(k)) {
        return Err(invalid);
    }
    if let Some(required) = schema.get("required") {
        let required = required.as_array().ok_or(invalid)?;
        let mut seen = HashSet::new();
        for name in required {
            let name = name.as_str().ok_or(invalid)?;
            if !properties.contains_key(name) || !seen.insert(name) {
                return Err(invalid);
            }
        }
    }
    let mut headers = HashSet::new();
    for property in properties.values() {
        if !keys(property, &["type", "description", "items", "x-mcp-header"])
            || !text(property.get("description"), 512)
        {
            return Err(invalid);
        }
        let kind = property
            .get("type")
            .and_then(Value::as_str)
            .ok_or(invalid)?;
        match kind {
            "string" | "number" | "integer" | "boolean" => {
                if property.get("items").is_some() {
                    return Err(invalid);
                }
            }
            "array" => {
                let items = property.get("items").ok_or(invalid)?;
                if !keys(items, &["type"])
                    || items.get("type").and_then(Value::as_str) != Some("string")
                {
                    return Err(invalid);
                }
            }
            _ => return Err(invalid),
        }
        if let Some(header) = property.get("x-mcp-header") {
            let header = header.as_str().ok_or(invalid)?;
            if !matches!(kind, "string" | "integer" | "boolean")
                || header.is_empty()
                || header.len() > 64
                || !header
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&b))
                || !headers.insert(header.to_ascii_lowercase())
            {
                return Err(invalid);
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_arguments(schema: &Value, arguments: &Value) -> Result<(), SchemaError> {
    validate_schema(schema)?;
    let invalid = SchemaError::Arguments;
    let arguments = arguments.as_object().ok_or(invalid)?;
    if serde_json::to_vec(arguments).map_err(|_| invalid)?.len() > 60 * 1024 {
        return Err(invalid);
    }
    let properties = schema["properties"].as_object().ok_or(invalid)?;
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        if required
            .iter()
            .any(|n| !arguments.contains_key(n.as_str().unwrap_or("")))
        {
            return Err(invalid);
        }
    }
    for (name, value) in arguments {
        let Some(property) = properties.get(name) else {
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(invalid);
            }
            continue;
        };
        let valid = match property["type"].as_str() {
            Some("string") => value.is_string(),
            Some("number") => value.is_number(),
            Some("integer") => {
                value.as_i64().is_some()
                    || value.as_u64().is_some()
                    || value
                        .as_f64()
                        .is_some_and(|v| v.is_finite() && v.fract() == 0.0)
            }
            Some("boolean") => value.is_boolean(),
            Some("array") => value
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            _ => false,
        };
        if !valid {
            return Err(invalid);
        }
        // Wire header integers use the exact JSON-safe range, and strings
        // must fit the transport's bounded mirror before approval is asked.
        if property.get("x-mcp-header").is_some() {
            match property["type"].as_str() {
                Some("integer")
                    if !value.as_i64().is_some_and(|n| {
                        (-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&n)
                    }) =>
                {
                    return Err(invalid)
                }
                Some("string") if value.as_str().is_none_or(|s| s.len() > 4096) => {
                    return Err(invalid)
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn revision(schema: &Value, key: &[u8; 32]) -> String {
    blake3::keyed_hash(
        key,
        &serde_json::to_vec(schema).expect("validated JSON schema"),
    )
    .to_hex()
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn schema() -> Value {
        json!({"type":"object","properties":{"count":{"type":"integer","x-mcp-header":"Count"},"tags":{"type":"array","items":{"type":"string"}}},"required":["count"],"additionalProperties":false})
    }
    #[test]
    fn backend_enforces_required_integer_array_and_unknown_arguments() {
        assert!(validate_arguments(&schema(), &json!({"count":2,"tags":["a"]})).is_ok());
        for args in [
            json!({}),
            json!({"count":2.5}),
            json!({"count":null}),
            json!({"count":true}),
            json!({"count":2,"tags":[1]}),
            json!({"count":2,"unknown":1}),
        ] {
            assert_eq!(
                validate_arguments(&schema(), &args),
                Err(SchemaError::Arguments)
            );
        }
    }
    #[test]
    fn arguments_leave_room_for_the_complete_transport_envelope() {
        let schema = serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}});
        assert_eq!(
            validate_arguments(&schema, &serde_json::json!({"text":"x".repeat(60 * 1024)})),
            Err(SchemaError::Arguments)
        );
        let arguments = serde_json::json!({"text":"x".repeat(60 * 1024 - 32)});
        validate_arguments(&schema, &arguments).unwrap();
        let mut params = serde_json::Map::new();
        params.insert("name".into(), serde_json::json!("x".repeat(128)));
        params.insert("arguments".into(), arguments);
        for era in [
            crate::mcp_client_protocol::Era::Modern,
            crate::mcp_client_protocol::Era::Legacy(crate::mcp_client_protocol::LEGACY_PREFERRED),
        ] {
            let request = crate::mcp_client_protocol::request(
                era,
                u64::MAX,
                "tools/call",
                params.clone(),
                "AeroFTP",
                env!("CARGO_PKG_VERSION"),
            )
            .unwrap();
            assert!(serde_json::to_vec(&request).unwrap().len() < 64 * 1024);
        }
    }

    #[test]
    fn unsupported_constraints_and_header_collisions_fail_closed() {
        for key in ["$ref", "oneOf", "minimum", "pattern"] {
            let mut s = schema();
            s["properties"]["count"][key] = json!(1);
            assert_eq!(validate_schema(&s), Err(SchemaError::Unsupported));
        }
        let mut s = schema();
        s["properties"]["other"] = json!({"type":"string","x-mcp-header":"count"});
        assert_eq!(validate_schema(&s), Err(SchemaError::Unsupported));
    }
    #[test]
    fn duplicate_paginated_and_oversized_catalogs_are_unavailable() {
        let tool = json!({"name":"echo","inputSchema":schema()});
        for result in [
            json!({"tools":[tool.clone(),tool.clone()]}),
            json!({"tools":[tool.clone()],"nextCursor":"more"}),
            json!({"tools":vec![tool;MAX_TOOLS + 1]}),
        ] {
            assert_eq!(discover(&result, "echo"), Err(SchemaError::Unavailable));
        }
    }
    #[test]
    fn own_server_catalog_including_compatibility_names_can_discover_diagnostics() {
        let tools: Vec<_> = crate::mcp::tools::tool_definitions().into_iter().map(|tool| {
            json!({"name":tool.name,"description":tool.description,"inputSchema":tool.input_schema})
        }).collect();
        assert!(tools.len() > 64 && tools.len() <= MAX_TOOLS);
        let schema = discover(&json!({"tools":tools}), "aeroftp_mcp_info").unwrap();
        validate_arguments(&schema, &json!({})).unwrap();
    }

    #[test]
    fn catalog_limit_accepts_128_unique_tools_and_refuses_129() {
        let mut tools: Vec<_> = (0..MAX_TOOLS)
            .map(|i| json!({"name":format!("tool_{i}"),"inputSchema":schema()}))
            .collect();
        assert!(discover(&json!({"tools":tools}), "tool_0").is_ok());
        tools.push(json!({"name":"overflow","inputSchema":schema()}));
        assert_eq!(
            discover(&json!({"tools":tools}), "tool_0"),
            Err(SchemaError::Unavailable)
        );
    }

    #[test]
    fn header_only_change_invalidates_backend_schema_revision() {
        let mut s = schema();
        let first = revision(&s, &[3; 32]);
        s["properties"]["count"]["x-mcp-header"] = json!("Other");
        assert_ne!(first, revision(&s, &[3; 32]));
    }
}
