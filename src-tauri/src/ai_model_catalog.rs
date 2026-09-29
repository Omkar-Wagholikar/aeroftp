// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

//! Optional capability metadata from provider model discovery. Availability alone
//! never grants tool/vision support. Only the documented OpenRouter schema is read.
use crate::ai::{AIError, AIProviderType};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelInfo {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_parallel_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_context_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

fn positive(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}

fn parse_model(value: &Value) -> Option<ModelInfo> {
    let id = value["id"].as_str()?.to_owned();
    if id.is_empty() {
        return None;
    }
    let parameters = value["supported_parameters"].as_array();
    let has = |name: &str| parameters.map(|p| p.iter().any(|v| v.as_str() == Some(name)));
    Some(ModelInfo {
        id,
        supports_tools: has("tools"),
        supports_vision: value["architecture"]["input_modalities"]
            .as_array()
            .map(|a| a.iter().any(|v| v.as_str() == Some("image"))),
        supports_thinking: parameters.map(|p| {
            p.iter()
                .any(|v| matches!(v.as_str(), Some("reasoning" | "reasoning_effort")))
        }),
        supports_parallel_tools: has("parallel_tool_calls"),
        max_context_tokens: positive(&value["context_length"]),
        max_tokens: positive(&value["top_provider"]["max_completion_tokens"]),
    })
}

pub(crate) async fn list(
    provider: AIProviderType,
    base_url: String,
    api_key: Option<String>,
) -> Result<Value, AIError> {
    if provider == AIProviderType::OpenRouter
        && base_url.trim_end_matches('/') == "https://openrouter.ai/api/v1"
    {
        // This catalog is public. Do not attach the user's credential to a public metadata request.
        let response = crate::ai::AI_HTTP_CLIENT
            .get("https://openrouter.ai/api/v1/models")
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(AIError::Api(format!(
                "Model catalog HTTP {}",
                response.status()
            )));
        }
        let body: Value = response.json().await?;
        let entries = body["data"]
            .as_array()
            .ok_or_else(|| AIError::InvalidResponse("Missing model catalog data".into()))?;
        Ok(serde_json::json!(entries
            .iter()
            .filter_map(parse_model)
            .collect::<Vec<_>>()))
    } else {
        let names = crate::ai::list_models(provider, base_url, api_key).await?;
        Ok(serde_json::json!(names
            .into_iter()
            .map(|id| serde_json::json!({"id":id}))
            .collect::<Vec<_>>()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_distinguishes_declared_capabilities_from_absent_metadata() {
        let known = parse_model(&serde_json::json!({"id":"vendor/model:free","supported_parameters":["tools","reasoning"],"architecture":{"input_modalities":["text"]},"context_length":1000000,"top_provider":{"max_completion_tokens":32768}})).unwrap();
        assert_eq!(known.supports_tools, Some(true));
        assert_eq!(known.supports_vision, Some(false));
        assert_eq!(known.supports_thinking, Some(true));
        assert_eq!(known.supports_parallel_tools, Some(false));
        assert_eq!(known.max_context_tokens, Some(1000000));
        let absent = parse_model(&serde_json::json!({"id":"unknown","context_length":-1})).unwrap();
        assert_eq!(absent.supports_tools, None);
        assert_eq!(absent.supports_vision, None);
        assert_eq!(absent.max_context_tokens, None);
        assert!(parse_model(&serde_json::json!({"id":""})).is_none());
    }
}
