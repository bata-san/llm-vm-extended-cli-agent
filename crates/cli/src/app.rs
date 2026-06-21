//! REPL application logic: the state machine that owns the VM, tools, and LLM
//! client for an interactive session.

use crate::tui;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use llm_vm_core::{LlmClient, LlmClientConfig, NodeAction, ToolContext, ToolRegistry, BuiltinTools};
use llm_vm_vm::{DeterministicVm, Graph, NodeSpec, NodeResult};
use llm_vm_wal::Wal;

pub struct App {
    pub vm: DeterministicVm,
    pub history: Vec<Turn>,
}

pub struct Turn {
    pub prompt: String,
    pub result: NodeResult,
}

impl App {
    pub fn new(wal: Arc<Wal>, root: PathBuf) -> Result<Self> {
        let cfg = LlmClientConfig::default();
        let llm = Arc::new(LlmClient::new(cfg)?);
        let mut tools_reg = ToolRegistry::new(ToolContext {
            workspace_root: root,
            allow_shell: false,
        });
        BuiltinTools::register_all(&mut tools_reg);
        let tools = Arc::new(tools_reg);
        let action = Arc::new(NodeAction::new(llm, tools));
        let vm = DeterministicVm::new(wal, action);
        Ok(Self { vm, history: Vec::new() })
    }

    pub async fn run_prompt(&mut self, prompt: &str) -> Result<NodeResult> {
        // Build a tiny graph: a single LLM completion node seeded with prior
        // turns as context. The id is hashed from the prompt + history length
        // so distinct prompts get distinct WAL entries.
        let id = format!("turn-{}", self.history.len());
        let mut messages = vec![llm_vm_core::Message {
            role: llm_vm_core::Role::System,
            content: "You are a helpful coding assistant. Reply as JSON.".into(),
        }];
        for turn in &self.history {
            messages.push(llm_vm_core::Message {
                role: llm_vm_core::Role::User,
                content: turn.prompt.clone(),
            });
            messages.push(llm_vm_core::Message {
                role: llm_vm_core::Role::Assistant,
                content: turn.result.value.to_string(),
            });
        }
        messages.push(llm_vm_core::Message {
            role: llm_vm_core::Role::User,
            content: prompt.to_string(),
        });

        let node = NodeSpec {
            id,
            action_id: "llm.complete".into(),
            inputs: serde_json::json!({ "messages": messages }),
        };
        let graph = Graph::from_edges(vec![node], &[])?;
        let mut results = self.vm.execute(&graph).await?;
        let result = results
            .remove(&format!("turn-{}", self.history.len()))
            .or_else(|| results.values().next().cloned())
            .ok_or_else(|| anyhow::anyhow!("no result produced"))?;
        let result_clone = result.clone();
        self.history.push(Turn {
            prompt: prompt.to_string(),
            result,
        });
        Ok(result_clone)
    }
}

/// Drive the interactive REPL using the ratatui TUI.
pub async fn run_repl(wal: Arc<Wal>, root: PathBuf) -> Result<()> {
    let mut app = App::new(wal, root)?;
    tui::run(&mut app).await
}
