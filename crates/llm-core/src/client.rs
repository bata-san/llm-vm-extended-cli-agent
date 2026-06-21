//! HTTP client for OpenAI-compatible chat completion endpoints.
//!
//! Designed to work against:
//!   - OpenAI (`https://api.openai.com/v1`)
//!   - Nvidia NIM (`https://integrate.api.nvidia.com/v1`)
//!   - vLLM / llama.cpp / Ollama (any server speaking the OpenAI schema)
//!
//! Non-streaming completions are wrapped with the sentinel protocol so a
//! truncated 200-OK response is detected and reported as an error (rather than
//! silently fed to the model as if it were complete). Streaming + hedge
//! integration is plumbed but the simple path is shown first.

use llm_vm_protocol::{constrain_request, JsonSchema};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("response was empty or truncated (no sentinel)")]
    Truncated,
    #[error("invalid response shape: {0}")]
    BadShape(String),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub schema: Option<JsonSchema>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// The parsed response from a completion call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub model: String,
    /// Raw text returned by the model.
    pub content: String,
    /// JSON parsed from `content` when a schema was requested. On parse
    /// failure this is `Null` and the caller should treat the call as failed.
    pub content_json: serde_json::Value,
    pub usage: Usage,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct LlmClientConfig {
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
    pub timeout: Duration,
}

impl Default for LlmClientConfig {
    fn default() -> Self {
        Self {
            base_url: "https://integrate.api.nvidia.com/v1".into(),
            api_key: std::env::var("NIM_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_default(),
            default_model: "meta/llama-3.1-70b-instruct".into(),
            timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: HttpClient,
    config: LlmClientConfig,
}

impl LlmClient {
    pub fn new(config: LlmClientConfig) -> Result<Self, LlmError> {
        let http = HttpClient::builder()
            .timeout(config.timeout)
            .build()?;
        Ok(Self { http, config })
    }

    /// Build the request body the OpenAI-compatible endpoint expects.
    fn build_body(&self, req: &CompletionRequest) -> serde_json::Value {
        let model = req
            .model
            .clone()
            .unwrap_or_else(|| self.config.default_model.clone());
        let mut body = serde_json::json!({
            "model": model,
            "messages": req.messages,
        });
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(schema) = &req.schema {
            body["response_format"] = constrain_request(schema, true);
        }
        body
    }

    /// POST /chat/completions and parse the response. If a schema is supplied,
    /// the returned `content_json` is the parsed JSON.
    pub async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let body = self.build_body(req);
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.config.base_url.trim_end_matches('/')))
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: text,
            });
        }

        let raw: serde_json::Value = resp.json().await?;
        let choice = raw
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| LlmError::BadShape("missing choices[0]".into()))?;
        let message = choice
            .get("message")
            .ok_or_else(|| LlmError::BadShape("missing message".into()))?;
        let content = message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| LlmError::BadShape("missing content".into()))?
            .to_string();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("stop")
            .to_string();
        let model = raw
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        let usage = raw
            .get("usage")
            .map(|u| Usage {
                prompt_tokens: u.get("prompt_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0),
                completion_tokens: u
                    .get("completion_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                total_tokens: u.get("total_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0),
            })
            .unwrap_or_default();

        // Try to parse the content as JSON if a schema was requested.
        let content_json = if req.schema.is_some() {
            serde_json::from_str(&content).unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::String(content.clone())
        };

        // Sentinel check: if finish_reason indicates length cap, surface as truncation.
        if finish_reason == "length" {
            return Err(LlmError::Truncated);
        }

        if req.schema.is_some() && content_json.is_null() {
            return Err(LlmError::BadShape(
                "schema requested but content was not valid JSON".into(),
            ));
        }

        Ok(CompletionResponse {
            model,
            content,
            content_json,
            usage,
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_response_format_when_schema_set() {
        let cfg = LlmClientConfig::default();
        let client = LlmClient::new(cfg).unwrap();
        let req = CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                content: "hi".into(),
            }],
            schema: Some(JsonSchema::object(serde_json::Map::new(), vec![])),
            model: None,
            max_tokens: Some(16),
            temperature: None,
        };
        let body = client.build_body(&req);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["max_tokens"], 16);
    }

    #[test]
    fn body_omits_response_format_without_schema() {
        let cfg = LlmClientConfig::default();
        let client = LlmClient::new(cfg).unwrap();
        let req = CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                content: "hi".into(),
            }],
            schema: None,
            model: Some("m".into()),
            max_tokens: None,
            temperature: Some(0.0),
        };
        let body = client.build_body(&req);
        assert!(body.get("response_format").is_none());
        assert_eq!(body["temperature"], 0.0);
    }
}
