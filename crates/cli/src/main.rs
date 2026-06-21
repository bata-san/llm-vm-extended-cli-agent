//! `llmvm` — entry point for the LLM-VM Extended CLI Agent.
//!
//! Subcommands:
//!   - `run`     : execute a one-shot prompt through the full stack
//!   - `dag`     : load a DAG spec from JSON and execute it
//!   - `repl`    : interactive TUI session
//!   - `inspect` : dump the WAL contents
//!   - `version` : print build info

mod app;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use llm_vm_core::{LlmClient, LlmClientConfig, NodeAction, ToolContext, ToolRegistry, BuiltinTools};
use llm_vm_vm::{DeterministicVm, Graph, NodeSpec};
use llm_vm_wal::Wal;

#[derive(Debug, Parser)]
#[command(
    name = "llmvm",
    version,
    about = "Deterministic coding agent for flaky / rate-limited LLM APIs"
)]
struct Cli {
    /// Path to the WAL database (default: ./.llmvm/wal.db).
    #[arg(long, env = "LLMVM_WAL", global = true)]
    wal: Option<PathBuf>,

    /// Workspace root for filesystem tools (default: cwd).
    #[arg(long, env = "LLMVM_ROOT", global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a single prompt end-to-end through the LLM.
    Run {
        /// The prompt text.
        prompt: String,
        /// Override the default model.
        #[arg(long)]
        model: Option<String>,
    },
    /// Execute a DAG described in JSON.
    Dag {
        /// Path to a JSON file with `{nodes: [...], edges: [["a","b"], ...]}`.
        #[arg(short, long)]
        file: PathBuf,
    },
    /// Interactive TUI session.
    Repl,
    /// Inspect WAL records.
    Inspect,
    /// Print version + build info.
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let wal_path = cli
        .wal
        .clone()
        .unwrap_or_else(|| PathBuf::from(".llmvm/wal.db"));
    if let Some(parent) = wal_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let root = cli
        .root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let wal = Arc::new(Wal::open(&wal_path).context("opening WAL")?);

    match cli.command {
        Command::Version => {
            println!("llmvm {}", env!("CARGO_PKG_VERSION"));
            println!("repository: {}", env!("CARGO_PKG_REPOSITORY"));
        }
        Command::Inspect => {
            for rec in wal.all()? {
                println!(
                    "{:16} action={:<24} key={:.12}..",
                    rec.created_at, rec.action_id, rec.key
                );
            }
        }
        Command::Run { prompt, model } => {
            let cfg = LlmClientConfig::default();
            let llm = Arc::new(LlmClient::new(cfg)?);
            let mut tools_reg = ToolRegistry::new(ToolContext {
                workspace_root: root.clone(),
                allow_shell: false,
            });
            BuiltinTools::register_all(&mut tools_reg);
            let tools = Arc::new(tools_reg);
            let action = Arc::new(NodeAction::new(llm, tools));
            let vm = DeterministicVm::new(wal, action);

            let messages = vec![
                llm_vm_core::Message {
                    role: llm_vm_core::Role::System,
                    content: "You are a helpful coding assistant. Reply as JSON.".into(),
                },
                llm_vm_core::Message {
                    role: llm_vm_core::Role::User,
                    content: prompt,
                },
            ];
            let node = NodeSpec {
                id: "root".into(),
                action_id: "llm.complete".into(),
                inputs: serde_json::json!({
                    "messages": messages,
                    "model": model,
                }),
            };
            let graph = Graph::from_edges(vec![node], &[])?;
            let results = vm.execute(&graph).await?;
            let root_result = results.get("root").context("missing root result")?;
            println!("{}", serde_json::to_string_pretty(&root_result.value)?);
        }
        Command::Dag { file } => {
            let raw = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let spec: DagSpec = serde_json::from_str(&raw).context("parsing DAG spec")?;
            let nodes: Vec<NodeSpec> = spec.nodes.into_iter().map(Into::into).collect();
            let edges: Vec<(String, String)> = spec
                .edges
                .unwrap_or_default()
                .into_iter()
                .map(|e| (e[0].clone(), e[1].clone()))
                .collect();
            let graph = Graph::from_edges(nodes, &edges)?;

            let cfg = LlmClientConfig::default();
            let llm = Arc::new(LlmClient::new(cfg)?);
            let mut tools_reg = ToolRegistry::new(ToolContext {
                workspace_root: root.clone(),
                allow_shell: false,
            });
            BuiltinTools::register_all(&mut tools_reg);
            let tools = Arc::new(tools_reg);
            let action = Arc::new(NodeAction::new(llm, tools));
            let vm = DeterministicVm::new(wal, action);
            let results = vm.execute(&graph).await?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }
        Command::Repl => {
            app::run_repl(wal, root).await?;
        }
    }

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct DagSpec {
    nodes: Vec<DagNode>,
    edges: Option<Vec<[String; 2]>>,
}

#[derive(Debug, serde::Deserialize)]
struct DagNode {
    id: String,
    #[serde(rename = "action")]
    action_id: String,
    #[serde(default)]
    inputs: serde_json::Value,
}

impl From<DagNode> for NodeSpec {
    fn from(n: DagNode) -> Self {
        Self {
            id: n.id,
            action_id: n.action_id,
            inputs: n.inputs,
        }
    }
}
