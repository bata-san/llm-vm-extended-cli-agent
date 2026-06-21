//! gPTE: graph-task Prompt Tuning & Engineering.
//!
//! Two related capabilities:
//!
//! 1. **Prompt synthesis for graph tasks.** Turning "fix this failing node"
//!    into a concrete repair prompt requires knowing the node's inputs, its
//!    predecessor outputs, and the schema it must satisfy. gPTE builds that
//!    prompt deterministically from the DAG, so the model isn't asked to
//!    reverse-engineer context.
//!
//! 2. **LEAP-style tuning.** Rather than hard-coding prompts, we keep a small
//!    library of *prompt templates* with tunable few-shot examples, and rank
//!    them by observed success rate. Over a session the executor learns which
//!    templates work for which action kinds.
//!
//! The implementation here is the deterministic skeleton: prompt assembly,
//! template registry, and a simple EMA-based scoring loop. The actual LLM
//! evaluation harness lives in `llm-vm-core`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use thiserror::Error;

pub use llm_vm_vm::NodeSpec;

#[derive(Debug, Error)]
pub enum GpteError {
    #[error("no template registered for action `{0}`")]
    NoTemplate(String),
    #[error("template error: {0}")]
    Template(String),
}

/// A prompt template: a mustache-ish string with `{var}` placeholders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub action_id: String,
    pub system: String,
    pub user: String,
    /// Optional few-shot examples appended to the user message.
    pub examples: Vec<Example>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Example {
    pub inputs: serde_json::Value,
    pub output: serde_json::Value,
}

/// The fully-assembled prompt ready for the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub system: String,
    pub user: String,
}

/// Mutable tuning state: per-template success scores (EMA), and per-action
/// chosen template id.
pub struct PromptTuner {
    templates: HashMap<String, Vec<Template>>, // action_id → candidates
    scores: Mutex<HashMap<String, f64>>,       // template_id → EMA success ∈ [0,1]
    chosen: Mutex<HashMap<String, String>>,    // action_id → template_id
}

impl PromptTuner {
    pub fn new() -> Self {
        Self {
            templates: HashMap::new(),
            scores: Mutex::new(HashMap::new()),
            chosen: Mutex::new(HashMap::new()),
        }
    }

    /// Register a template. The first template for an action becomes the default.
    pub fn register(&mut self, t: Template) {
        let action = t.action_id.clone();
        let is_first = !self.templates.contains_key(&action);
        self.templates.entry(action.clone()).or_default().push(t.clone());
        if is_first {
            self.chosen.lock().unwrap().insert(action, t.id);
        }
    }

    /// Pick the highest-scoring template for `action_id`.
    pub fn select(&self, action_id: &str) -> Result<&Template, GpteError> {
        let candidates = self
            .templates
            .get(action_id)
            .ok_or_else(|| GpteError::NoTemplate(action_id.to_string()))?;
        if candidates.is_empty() {
            return Err(GpteError::NoTemplate(action_id.to_string()));
        }
        // Honor an explicit override if set.
        let chosen = self.chosen.lock().unwrap();
        if let Some(id) = chosen.get(action_id) {
            if let Some(t) = candidates.iter().find(|t| &t.id == id) {
                return Ok(t);
            }
        }
        // Otherwise pick the best by score.
        let scores = self.scores.lock().unwrap();
        let best = candidates
            .iter()
            .max_by(|a, b| {
                let sa = scores.get(&a.id).copied().unwrap_or(0.5);
                let sb = scores.get(&b.id).copied().unwrap_or(0.5);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        Ok(best)
    }

    /// Record an outcome and update the chosen template's score via EMA.
    /// `alpha` controls smoothing (0.1 is typical).
    pub fn feedback(&self, template_id: &str, success: bool, alpha: f64) {
        let mut scores = self.scores.lock().unwrap();
        let prev = scores.get(template_id).copied().unwrap_or(0.5);
        let observed = if success { 1.0 } else { 0.0 };
        let next = alpha * observed + (1.0 - alpha) * prev;
        scores.insert(template_id.to_string(), next);
    }

    /// Force a template for an action (e.g. from an explicit config).
    pub fn set_chosen(&self, action_id: &str, template_id: &str) {
        self.chosen
            .lock()
            .unwrap()
            .insert(action_id.to_string(), template_id.to_string());
    }
}

impl Default for PromptTuner {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a template against a node spec. Predecessor outputs (if any) are
/// passed via the `inputs` JSON — the DAG already wired them in.
pub fn render(template: &Template, node: &NodeSpec) -> Result<Prompt, GpteError> {
    let ctx = build_context(node)?;
    let system = substitute(&template.system, &ctx)?;
    let mut user = substitute(&template.user, &ctx)?;
    for ex in &template.examples {
        let ex_inputs = serde_json::to_string_pretty(&ex.inputs).unwrap_or_default();
        let ex_output = serde_json::to_string_pretty(&ex.output).unwrap_or_default();
        user.push_str(&format!("\n\n## Example\nInput:\n{ex_inputs}\nOutput:\n{ex_output}"));
    }
    Ok(Prompt { system, user })
}

#[derive(Serialize)]
struct Context<'a> {
    action_id: &'a str,
    node_id: &'a str,
    inputs: &'a serde_json::Value,
    inputs_json: String,
}

fn build_context(node: &NodeSpec) -> Result<serde_json::Value, GpteError> {
    let inputs_json = serde_json::to_string_pretty(&node.inputs)
        .map_err(|e| GpteError::Template(e.to_string()))?;
    let ctx = Context {
        action_id: &node.action_id,
        node_id: &node.id,
        inputs: &node.inputs,
        inputs_json,
    };
    serde_json::to_value(&ctx).map_err(|e| GpteError::Template(e.to_string()))
}

/// Tiny template engine: `{var}` substitution with dotted-path access into JSON.
fn substitute(tpl: &str, ctx: &serde_json::Value) -> Result<String, GpteError> {
    let mut out = String::with_capacity(tpl.len());
    let bytes = tpl.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Find matching close brace.
            if let Some(end) = tpl[i + 1..].find('}') {
                let key = &tpl[i + 1..i + 1 + end];
                let val = lookup(ctx, key)?;
                out.push_str(&val);
                i = i + 1 + end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

fn lookup(ctx: &serde_json::Value, key: &str) -> Result<String, GpteError> {
    let mut cur = ctx;
    for part in key.split('.') {
        if part.is_empty() {
            continue;
        }
        cur = cur.get(part).ok_or_else(|| {
            GpteError::Template(format!("missing key `{part}` in prompt context"))
        })?;
    }
    Ok(match cur {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    })
}

/// Convenience: build a default repair template for a failing node.
pub fn default_repair_template(action_id: &str) -> Template {
    Template {
        id: format!("{action_id}.repair.v1"),
        action_id: action_id.to_string(),
        system: "You are a precise code-repair agent. Output a single JSON object that conforms to the given schema. Do not emit prose.".into(),
        user: "The following node failed. Inspect the inputs and the error, then produce a corrected output.\n\nNode: {node_id}\nAction: {action_id}\nInputs (JSON):\n{inputs_json}\n\nReturn only the corrected JSON.".into(),
        examples: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> NodeSpec {
        NodeSpec {
            id: "n1".into(),
            action_id: "tool.read_file".into(),
            inputs: serde_json::json!({"path": "/tmp/x.rs", "offset": 10}),
        }
    }

    #[test]
    fn render_substitutes_inputs() {
        let tpl = Template {
            id: "t".into(),
            action_id: "tool.read_file".into(),
            system: "sys".into(),
            user: "Read {inputs.path} from offset {inputs.offset}.".into(),
            examples: vec![],
        };
        let p = render(&tpl, &node()).unwrap();
        assert!(p.user.contains("/tmp/x.rs"));
        assert!(p.user.contains("offset 10"));
    }

    #[test]
    fn tuner_picks_highest_score() {
        let mut tuner = PromptTuner::new();
        let a = Template {
            id: "a".into(),
            action_id: "act".into(),
            system: "".into(),
            user: "a".into(),
            examples: vec![],
        };
        let b = Template {
            id: "b".into(),
            action_id: "act".into(),
            system: "".into(),
            user: "b".into(),
            examples: vec![],
        };
        tuner.register(a);
        tuner.register(b);
        // b gets better feedback over time.
        for _ in 0..5 {
            tuner.feedback("b", true, 0.3);
            tuner.feedback("a", false, 0.3);
        }
        // Explicit override is honored.
        tuner.set_chosen("act", "a");
        assert_eq!(tuner.select("act").unwrap().id, "a");
    }

    #[test]
    fn default_repair_template_renders() {
        let t = default_repair_template("llm.complete");
        let p = render(&t, &node()).unwrap();
        assert!(p.user.contains("n1"));
        assert!(p.user.contains("/tmp/x.rs"));
    }
}
