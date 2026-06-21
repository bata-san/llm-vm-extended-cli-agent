//! Tool execution layer.
//!
//! Tools are typed, async callables registered by name. The VM invokes them
//! via `action_id == "tool.<name>"` (or any action whose inputs contain a
//! `tool` field). Built-ins cover filesystem reads/writes and shell commands;
//! the registry is open for extension.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::fs;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("shell command failed ({code}): {stderr}")]
    Shell { code: i32, stderr: String },
    #[error("path escapes workspace root: {0}")]
    PathEscape(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSuccess {
    pub output: serde_json::Value,
}

/// Shared context passed to every tool invocation (cwd, env, limits).
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub allow_shell: bool,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            allow_shell: false,
        }
    }
}

/// Filesystem root alias used by built-ins.
pub type FsContext = ToolContext;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn call(&self, args: &serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    ctx: ToolContext,
}

impl ToolRegistry {
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            tools: HashMap::new(),
            ctx,
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    pub async fn call(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        tool.call(args, &self.ctx).await
    }
}

/// Resolve `path` against the workspace root, rejecting escapes.
///
/// We lexically normalize `..` / `.` segments and require the result to stay
/// inside the workspace root. This avoids relying on `canonicalize`, which
/// fails on non-existent paths (so a freshly-written file can be re-read).
fn resolve(root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    use std::path::Component;

    // Start from root, then push segments after normalizing.
    let mut buf = root.to_path_buf();
    let incoming = Path::new(path);
    let is_absolute = incoming.is_absolute();

    for comp in incoming.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop only if we stay within root; otherwise it's an escape.
                if buf == root || !buf.starts_with(root) {
                    return Err(ToolError::PathEscape(path.to_string()));
                }
                buf.pop();
                if !buf.starts_with(root) {
                    return Err(ToolError::PathEscape(path.to_string()));
                }
            }
            Component::Normal(seg) => buf.push(seg),
            Component::RootDir | Component::Prefix(_) => {
                if is_absolute {
                    // Absolute paths: restart from root.
                    buf = root.to_path_buf();
                }
            }
        }
    }
    Ok(buf)
}

// ----- Built-in tools -------------------------------------------------------

pub struct BuiltinTools;

impl BuiltinTools {
    /// Register read_file / write_file / list_dir / run_shell on `registry`.
    pub fn register_all(registry: &mut ToolRegistry) {
        registry.register(Arc::new(ReadFile));
        registry.register(Arc::new(WriteFile));
        registry.register(Arc::new(ListDir));
        registry.register(Arc::new(RunShell));
    }
}

pub struct ReadFile;
#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a file relative to the workspace root."
    }
    async fn call(&self, args: &serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArg("path (string) required".into()))?;
        let resolved = resolve(&ctx.workspace_root, path)?;
        let bytes = fs::read(&resolved).await?;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        Ok(serde_json::json!({
            "path": path,
            "bytes": bytes.len(),
            "content": content,
        }))
    }
}

pub struct WriteFile;
#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write text content to a file relative to the workspace root."
    }
    async fn call(&self, args: &serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArg("path (string) required".into()))?;
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArg("content (string) required".into()))?;
        let resolved = resolve(&ctx.workspace_root, path)?;
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&resolved, content).await?;
        Ok(serde_json::json!({
            "path": path,
            "bytes": content.len(),
        }))
    }
}

pub struct ListDir;
#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List the entries of a directory relative to the workspace root."
    }
    async fn call(&self, args: &serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        let resolved = resolve(&ctx.workspace_root, path)?;
        let mut entries = Vec::new();
        let mut reader = fs::read_dir(&resolved).await?;
        while let Some(entry) = reader.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false);
            entries.push(serde_json::json!({
                "name": name,
                "is_dir": is_dir,
            }));
        }
        entries.sort_by(|a, b| {
            let an = a["name"].as_str().unwrap_or("");
            let bn = b["name"].as_str().unwrap_or("");
            an.cmp(bn)
        });
        Ok(serde_json::json!({ "path": path, "entries": entries }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

pub struct RunShell;
#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &str {
        "run_shell"
    }
    fn description(&self) -> &str {
        "Run a shell command in the workspace root. Disabled unless ctx.allow_shell is true."
    }
    async fn call(&self, args: &serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
        if !ctx.allow_shell {
            return Err(ToolError::InvalidArg("shell execution is disabled".into()));
        }
        let cmd = args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArg("command (string) required".into()))?;
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&ctx.workspace_root)
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let code = output.status.code();
        let result = RunShellOutput {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            code,
        };
        if !output.status.success() {
            return Err(ToolError::Shell {
                code: code.unwrap_or(-1),
                stderr,
            });
        }
        Ok(serde_json::to_value(result).unwrap_or(serde_json::Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_read_roundtrips() {
        let tmp = std::env::temp_dir().join(format!("llmvm-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp).await;
        fs::create_dir_all(&tmp).await.unwrap();
        let ctx = ToolContext {
            workspace_root: tmp.clone(),
            allow_shell: false,
        };
        let mut reg = ToolRegistry::new(ctx);
        BuiltinTools::register_all(&mut reg);

        let written = reg
            .call(
                "write_file",
                &serde_json::json!({"path": "nested/a.txt", "content": "hello"}),
            )
            .await
            .unwrap();
        assert_eq!(written["bytes"], 5);

        let read = reg
            .call("read_file", &serde_json::json!({"path": "nested/a.txt"}))
            .await
            .unwrap();
        assert_eq!(read["content"], "hello");
    }

    #[tokio::test]
    async fn path_escape_rejected() {
        let tmp = std::env::temp_dir().join(format!("llmvm-esc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp).await;
        fs::create_dir_all(&tmp).await.unwrap();
        let ctx = ToolContext {
            workspace_root: tmp,
            allow_shell: false,
        };
        let mut reg = ToolRegistry::new(ctx);
        BuiltinTools::register_all(&mut reg);
        let err = reg
            .call(
                "read_file",
                &serde_json::json!({"path": "../../etc/passwd"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscape(_)), "{err:?}");
    }
}
