//! OpenCode-style core: LLM client (OpenAI/NIM-compatible) + tool execution.
//!
//! This crate is the bridge between the deterministic VM (which only knows
//! about graphs of actions) and the actual LLM endpoints + filesystem tools.
//! It provides:
//!
//! - [`LlmClient`]: an HTTP client speaking the OpenAI Chat Completions API
//!   (also used by Nvidia NIM, vLLM, llama.cpp server, Ollama, etc.), with
//!   streaming, guided decoding request shaping, and the sentinel/hedge
//!   protocols wired in.
//! - [`ToolRegistry`]: a typed registry of tools the agent can call, with a
//!   minimal built-in set (read_file, write_file, list_dir, run_shell) and
//!   an extension point for custom tools.
//! - [`NodeAction`]: the bridge that turns a VM [`NodeSpec`] into either an
//!   LLM completion or a tool execution, so the same DAG runtime drives both.

pub mod client;
pub mod tools;

pub use client::{LlmClient, LlmClientConfig, LlmError, Message, Role, CompletionRequest};
pub use tools::{
    BuiltinTools, FsContext, RunShellOutput, Tool, ToolContext, ToolError, ToolRegistry,
    ToolSuccess,
};

use llm_vm_vm::async_trait;
use llm_vm_protocol::{validate_against_schema, ConstraintError, JsonSchema};
use llm_vm_vm::{Action, VmError};
use std::sync::Arc;

/// Dispatch table mapping `action_id` to either an LLM completion or a tool.
pub struct NodeAction {
    llm: Arc<LlmClient>,
    tools: Arc<ToolRegistry>,
    /// Schema required for `llm.complete` outputs. Defaults to a permissive
    /// object schema; callers override for stricter validation.
    complete_schema: JsonSchema,
}

impl NodeAction {
    pub fn new(llm: Arc<LlmClient>, tools: Arc<ToolRegistry>) -> Self {
        Self {
            llm,
            tools,
            complete_schema: JsonSchema::object(serde_json::Map::new(), vec![]),
        }
    }

    pub fn with_complete_schema(mut self, schema: JsonSchema) -> Self {
        self.complete_schema = schema;
        self
    }
}

#[async_trait]
impl Action for NodeAction {
    async fn run(
        &self,
        action_id: &str,
        inputs: &serde_json::Value,
    ) -> Result<serde_json::Value, VmError> {
        match action_id {
            "llm.complete" => self.run_complete(inputs).await,
            other => {
                // Treat as a tool call: {tool: "<id>", args: {...}}.
                let tool_id = inputs
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| VmError::ActionFailed(format!("missing `tool` in inputs for action `{other}`")))?;
                let args = inputs.get("args").cloned().unwrap_or(serde_json::Value::Null);
                let result = self
                    .tools
                    .call(tool_id, &args)
                    .await
                    .map_err(|e| VmError::ActionFailed(e.to_string()))?;
                Ok(result)
            }
        }
    }
}

impl NodeAction {
    async fn run_complete(&self, inputs: &serde_json::Value) -> Result<serde_json::Value, VmError> {
        // Wire format: {messages: [{role, content}], schema?: {...}, model?: "..."}
        let messages_value = inputs
            .get("messages")
            .ok_or_else(|| VmError::ActionFailed("llm.complete requires `messages`".into()))?;
        let messages: Vec<Message> = serde_json::from_value(messages_value.clone())
            .map_err(|e| VmError::ActionFailed(format!("invalid messages: {e}")))?;

        let schema = inputs
            .get("schema")
            .map(|s| serde_json::from_value::<JsonSchema>(s.clone()))
            .transpose()
            .map_err(|e| VmError::ActionFailed(format!("invalid schema: {e}")))?
            .unwrap_or_else(|| self.complete_schema.clone());

        let model = inputs
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.to_string());

        let req = CompletionRequest {
            messages,
            schema: Some(schema.clone()),
            model,
            max_tokens: inputs.get("max_tokens").and_then(serde_json::Value::as_u64).map(|n| n as u32),
            temperature: inputs
                .get("temperature")
                .and_then(serde_json::Value::as_f64)
                .map(|f| f as f32),
        };

        let response = self
            .llm
            .complete(&req)
            .await
            .map_err(|e| VmError::ActionFailed(e.to_string()))?;

        // Validate against the schema; if it fails, surface as ActionFailed so
        // the VM can retry just this node.
        validate_against_schema(&response.content_json, &schema).map_err(|e: ConstraintError| {
            VmError::ActionFailed(format!("guided-decoding validation failed: {e}"))
        })?;

        Ok(serde_json::to_value(response).map_err(|e| VmError::Serde(e))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_vm_protocol::constrain_request;

    #[test]
    fn constrain_request_is_emitted_on_complete() {
        // Smoke: ensure the schema payload is shaped correctly via the protocol.
        let schema = JsonSchema::object(
            serde_json::Map::from_iter([("ok".to_string(), serde_json::json!({"type": "boolean"}))]),
            vec!["ok".into()],
        );
        let fmt = constrain_request(&schema, true);
        assert_eq!(fmt["type"], "json_schema");
    }
}
