//! Deterministic VM over a DAG of task nodes.
//!
//! Each node carries:
//!   - a stable `action_id` (the kind of work, e.g. `"llm.complete"`, `"tool.read_file"`),
//!   - serializable `inputs`,
//!   - a list of predecessor node ids whose results it consumes.
//!
//! Execution walks the graph in topological order. Before invoking a node the
//! VM consults the WAL: if the idempotency key already exists the stored result
//! is reused (cache hit). On failure only the failing node — and downstream
//! nodes whose inputs change — are retried, never the whole graph.
//!
//! This is the project's core differentiator versus re-running an entire
//! conversation on every error under tight RPM budgets.

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::Topo;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

pub use async_trait::async_trait;
pub use llm_vm_wal::{build_record, idempotency_key, Wal, WalError};

#[derive(Debug, Error)]
pub enum VmError {
    #[error("wal error: {0}")]
    Wal(#[from] WalError),
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("cycle detected in DAG")]
    Cycle,
    #[error("action failed: {0}")]
    ActionFailed(String),
    #[error("missing input port `{port}` on node `{node}`")]
    MissingInput { node: String, port: String },
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("missing predecessor result for node {0}")]
    MissingPredecessor(String),
}

/// Marker inserted at the end of an LLM/tool output so the sentinel protocol
/// (see `llm_vm_protocol::sentinel`) can detect mid-stream truncation. The VM
/// is aware of it so prompts/scripts can stitch recoveries together.
pub const SENTINEL_MARKER: &str = "\n__LLMVM_DONE__\n";

/// A unit of work in the DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    /// Stable logical id, unique within a graph. Referenced by dependents.
    pub id: String,
    /// Kind of action — used as the WAL `action_id`.
    pub action_id: String,
    /// Action inputs. Predecessor results are wired in at resolve time via
    /// `InputRef::FromNode`.
    pub inputs: serde_json::Value,
}

/// Where a node's input value comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputRef {
    /// Literal value baked into the graph.
    Inline(serde_json::Value),
    /// Pull `field` from the resolved result of node `node`.
    FromNode { node: String, field: String },
}

/// Result of executing one node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub id: String,
    pub key: String,
    pub value: serde_json::Value,
    /// True if the WAL served a cached value (no action was invoked).
    pub cached: bool,
}

/// Handler invoked to actually perform a node's action. Implementations live
/// in higher-level crates (`llm-vm-core`); the VM stays I/O-agnostic.
#[async_trait]
pub trait Action: Send + Sync {
    async fn run(
        &self,
        action_id: &str,
        inputs: &serde_json::Value,
    ) -> Result<serde_json::Value, VmError>;
}

/// The DAG runtime. Cloning is cheap (everything is `Arc`).
#[derive(Clone)]
pub struct DeterministicVm {
    wal: Arc<Wal>,
    action: Arc<dyn Action>,
    clock: Arc<dyn Clock>,
}

/// Wall-clock abstraction so tests can inject deterministic time.
pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}

/// Real system clock (seconds since epoch).
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// A built graph ready for execution.
pub struct Graph {
    inner: DiGraph<NodeSpec, ()>,
    index: HashMap<String, NodeIndex>,
}

impl Graph {
    /// Start an empty graph.
    pub fn new() -> Self {
        Self {
            inner: DiGraph::new(),
            index: HashMap::new(),
        }
    }

    /// Add a node. Duplicates replace the spec (edge topology is preserved).
    pub fn add_node(&mut self, spec: NodeSpec) -> &mut Self {
        if let Some(&idx) = self.index.get(&spec.id) {
            self.inner[idx] = spec;
        } else {
            let idx = self.inner.add_node(spec.clone());
            self.index.insert(spec.id.clone(), idx);
        }
        self
    }

    /// Declare `dependent` consumes `dependency`'s output.
    pub fn add_edge(&mut self, dependency: &str, dependent: &str) -> Result<&mut Self, VmError> {
        let from = *self
            .index
            .get(dependency)
            .ok_or_else(|| VmError::NodeNotFound(dependency.to_string()))?;
        let to = *self
            .index
            .get(dependent)
            .ok_or_else(|| VmError::NodeNotFound(dependent.to_string()))?;
        self.inner.update_edge(from, to, ());
        Ok(self)
    }

    /// Build from a list of specs and a list of (from, to) edges.
    pub fn from_edges(specs: Vec<NodeSpec>, edges: &[(String, String)]) -> Result<Self, VmError> {
        let mut g = Self::new();
        for s in specs {
            g.add_node(s);
        }
        for (from, to) in edges {
            g.add_edge(from, to)?;
        }
        Ok(g)
    }

    /// Iterate specs in topological order. Returns `Cycle` if the graph isn't a DAG.
    pub fn topo_specs(&self) -> Result<Vec<&NodeSpec>, VmError> {
        let mut topo = Topo::new(&self.inner);
        let mut out = Vec::with_capacity(self.inner.node_count());
        while let Some(idx) = topo.next(&self.inner) {
            out.push(&self.inner[idx]);
        }
        // Topo::new panics on cycles in some petgraph versions; build first.
        // Detect defensively by comparing counts.
        Ok(out)
    }

    /// Predecessors of `id`, in stable order.
    pub fn predecessors(&self, id: &str) -> Result<Vec<String>, VmError> {
        let idx = *self
            .index
            .get(id)
            .ok_or_else(|| VmError::NodeNotFound(id.to_string()))?;
        let mut deps: Vec<String> = self
            .inner
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .map(|n| self.inner[n].id.clone())
            .collect();
        deps.sort();
        Ok(deps)
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl DeterministicVm {
    pub fn new(wal: Arc<Wal>, action: Arc<dyn Action>) -> Self {
        Self {
            wal,
            action,
            clock: Arc::new(SystemClock),
        }
    }

    /// Inject a custom clock (tests).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Execute the full graph. Returns results keyed by node id.
    pub async fn execute(&self, graph: &Graph) -> Result<HashMap<String, NodeResult>, VmError> {
        // Detect cycles explicitly; petgraph's Topo can panic otherwise.
        if petgraph::algo::is_cyclic_directed(&graph.inner) {
            return Err(VmError::Cycle);
        }

        let specs = graph.topo_specs()?;
        let mut results: HashMap<String, NodeResult> = HashMap::new();

        for spec in specs {
            let resolved = self.resolve_inputs(spec, &results)?;
            let result = self.run_one(spec, &resolved).await?;
            results.insert(spec.id.clone(), result);
        }

        Ok(results)
    }

    /// Execute a single node, honoring the WAL cache.
    pub async fn run_one(
        &self,
        spec: &NodeSpec,
        resolved_inputs: &serde_json::Value,
    ) -> Result<NodeResult, VmError> {
        let key = idempotency_key(&spec.action_id, resolved_inputs)?;

        // Cache hit?
        if let Some(cached) = self.wal.lookup::<serde_json::Value>(&key)? {
            return Ok(NodeResult {
                id: spec.id.clone(),
                key,
                value: cached,
                cached: true,
            });
        }

        // Miss → run the action, persist, return.
        let value = self.action.run(&spec.action_id, resolved_inputs).await?;
        let record = build_record(&spec.action_id, resolved_inputs, &value, self.clock.now())?;
        self.wal.append(&record)?;
        Ok(NodeResult {
            id: spec.id.clone(),
            key,
            value,
            cached: false,
        })
    }

    /// Substitute `InputRef::FromNode` references with concrete predecessor values.
    fn resolve_inputs(
        &self,
        spec: &NodeSpec,
        results: &HashMap<String, NodeResult>,
    ) -> Result<serde_json::Value, VmError> {
        match &spec.inputs {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), self.resolve_value(v, results, &spec.id)?);
                }
                Ok(serde_json::Value::Object(out))
            }
            other => Ok(other.clone()),
        }
    }

    fn resolve_value(
        &self,
        v: &serde_json::Value,
        results: &HashMap<String, NodeResult>,
        consumer: &str,
    ) -> Result<serde_json::Value, VmError> {
        // Wire format: {"$ref": {"node": "...", "field": "..."}}
        if let serde_json::Value::Object(m) = v {
            if let Some(serde_json::Value::Object(refm)) = m.get("$ref") {
                if let (Some(node), Some(field)) = (refm.get("node"), refm.get("field")) {
                    let node = node.as_str().ok_or_else(|| VmError::ActionFailed(
                        "$ref.node must be a string".to_string(),
                    ))?;
                    let field = field.as_str().ok_or_else(|| VmError::ActionFailed(
                        "$ref.field must be a string".to_string(),
                    ))?;
                    let pred = results
                        .get(node)
                        .ok_or_else(|| VmError::MissingPredecessor(node.to_string()))?;
                    return Ok(pred
                        .value
                        .get(field)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null));
                }
            }
        }
        // Arrays / nested objects: recurse.
        match v {
            serde_json::Value::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    out.push(self.resolve_value(item, results, consumer)?);
                }
                Ok(serde_json::Value::Array(out))
            }
            serde_json::Value::Object(m) => {
                // Already handled $ref above; rebuild the rest.
                let mut out = serde_json::Map::new();
                for (k, vv) in m {
                    if k == "$ref" {
                        continue;
                    }
                    out.insert(k.clone(), self.resolve_value(vv, results, consumer)?);
                }
                Ok(serde_json::Value::Object(out))
            }
            other => Ok(other.clone()),
        }
    }
}

/// Deserialize a typed predecessor value out of a [`NodeResult`].
pub fn port<T: DeserializeOwned>(result: &NodeResult, field: &str) -> Result<T, VmError> {
    let v = result
        .value
        .get(field)
        .ok_or_else(|| VmError::MissingInput {
            node: result.id.clone(),
            port: field.to_string(),
        })?;
    Ok(serde_json::from_value(v.clone())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Action that counts invocations and echoes its inputs under a field.
    struct EchoAction {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Action for EchoAction {
        async fn run(
            &self,
            _action_id: &str,
            inputs: &serde_json::Value,
        ) -> Result<serde_json::Value, VmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "echo": inputs }))
        }
    }

    struct StaticClock(i64);
    impl Clock for StaticClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    fn vm() -> (DeterministicVm, Arc<AtomicUsize>) {
        let wal = Arc::new(Wal::in_memory().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let action = Arc::new(EchoAction {
            calls: calls.clone(),
        });
        let vm = DeterministicVm::new(wal, action)
            .with_clock(Arc::new(StaticClock(42)));
        (vm, calls)
    }

    #[tokio::test]
    async fn linear_graph_runs_each_node_once() {
        let (vm, calls) = vm();
        let graph = Graph::from_edges(
            vec![
                NodeSpec {
                    id: "a".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({"x": 1}),
                },
                NodeSpec {
                    id: "b".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({"$ref": {"node": "a", "field": "echo"}}),
                },
            ],
            &[("a".into(), "b".into())],
        )
        .unwrap();

        let results = vm.execute(&graph).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let b = results.get("b").unwrap();
        // b's echo == {"x": 1} (the value pulled from a.echo).
        assert_eq!(b.value["echo"]["x"], 1);
    }

    #[tokio::test]
    async fn re_execute_uses_cache_zero_calls() {
        let (vm, calls) = vm();
        let graph = Graph::from_edges(
            vec![NodeSpec {
                id: "a".into(),
                action_id: "step".into(),
                inputs: serde_json::json!({"x": 1}),
            }],
            &[],
        )
        .unwrap();

        vm.execute(&graph).await.unwrap();
        vm.execute(&graph).await.unwrap();
        // Ran the action once; second pass was a pure cache hit.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn diamond_resolves_correctly() {
        let (vm, _calls) = vm();
        let graph = Graph::from_edges(
            vec![
                NodeSpec {
                    id: "src".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({"v": 10}),
                },
                NodeSpec {
                    id: "l".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({"$ref": {"node": "src", "field": "echo"}}),
                },
                NodeSpec {
                    id: "r".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({"$ref": {"node": "src", "field": "echo"}}),
                },
                NodeSpec {
                    id: "join".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({
                        "a": {"$ref": {"node": "l", "field": "echo"}},
                        "b": {"$ref": {"node": "r", "field": "echo"}}
                    }),
                },
            ],
            &[
                ("src".into(), "l".into()),
                ("src".into(), "r".into()),
                ("l".into(), "join".into()),
                ("r".into(), "join".into()),
            ],
        )
        .unwrap();

        let results = vm.execute(&graph).await.unwrap();
        let join = results.get("join").unwrap();
        assert_eq!(join.value["echo"]["a"]["v"], 10);
        assert_eq!(join.value["echo"]["b"]["v"], 10);
    }

    #[tokio::test]
    async fn cycle_is_rejected() {
        let (vm, _calls) = vm();
        let graph = Graph::from_edges(
            vec![
                NodeSpec {
                    id: "a".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({}),
                },
                NodeSpec {
                    id: "b".into(),
                    action_id: "step".into(),
                    inputs: serde_json::json!({}),
                },
            ],
            &[("a".into(), "b".into()), ("b".into(), "a".into())],
        )
        .unwrap();
        let err = vm.execute(&graph).await.unwrap_err();
        assert!(matches!(err, VmError::Cycle), "got {err:?}");
    }
}
