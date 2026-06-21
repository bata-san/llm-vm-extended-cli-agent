//! GraphRAG: codebase knowledge graph for multi-hop reasoning.
//!
//! Retrieval-Augmented Generation over flat embeddings loses signal on
//! questions like "which callers of `parse_config` would break if I change
//! its return type?" — that's a multi-hop traversal, not a similarity search.
//!
//! GraphRAG keeps a codebase as a property graph:
//!
//! - **Nodes**: files, modules, symbols (functions/types/traits), with spans.
//! - **Edges**: `defines`, `calls`, `imports`, `references`, `contains`.
//!
//! At query time we seed from symbol/file nodes and expand `k` hops, ranking
//! by edge type weights. The result is a compact subgraph the LLM can reason
//! about directly — no "guess the relevant chunk" step.

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
}

/// What kind of codebase entity a node represents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NodeKind {
    File,
    Module,
    Function,
    Type,
    Trait,
    Variable,
}

/// Edge semantics. Weights for ranking live in [`EdgeWeight`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Defines,
    Contains,
    Calls,
    References,
    Imports,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeNode {
    pub kind: NodeKind,
    pub name: String,
    /// Path for files/modules; fully-qualified name for symbols.
    pub qualified: String,
    pub span: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdge {
    pub kind: EdgeKind,
}

/// The knowledge graph plus name → NodeIndex indices for fast seeding.
pub struct KnowledgeGraph {
    graph: DiGraph<CodeNode, CodeEdge>,
    by_name: HashMap<String, NodeIndex>,
}

impl KnowledgeGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            by_name: HashMap::new(),
        }
    }

    /// Add (or look up) a node by qualified name.
    pub fn add_node(&mut self, node: CodeNode) -> NodeIndex {
        if let Some(&idx) = self.by_name.get(&node.qualified) {
            return idx;
        }
        let idx = self.graph.add_node(node.clone());
        self.by_name.insert(node.qualified, idx);
        idx
    }

    pub fn add_edge(&mut self, from: NodeIndex, to: NodeIndex, kind: EdgeKind) {
        self.graph.add_edge(from, to, CodeEdge { kind });
    }

    pub fn get(&self, qualified: &str) -> Option<NodeIndex> {
        self.by_name.get(qualified).copied()
    }

    pub fn node(&self, idx: NodeIndex) -> Option<&CodeNode> {
        self.graph.node_weight(idx)
    }

    /// k-hop subgraph rooted at `seed`. Returns nodes in BFS order, ranked by
    /// edge-type weight (calls/references score higher than contains).
    pub fn subgraph(&self, seed: NodeIndex, k: u8) -> Vec<NodeIndex> {
        let mut visited = HashSet::new();
        let mut out = Vec::new();
        // DFS to depth k, both directions.
        self.dfs_dir(seed, k, petgraph::Direction::Outgoing, &mut visited, &mut out);
        self.dfs_dir(seed, k, petgraph::Direction::Incoming, &mut visited, &mut out);
        out
    }

    fn dfs_dir(
        &self,
        start: NodeIndex,
        k: u8,
        dir: petgraph::Direction,
        visited: &mut HashSet<NodeIndex>,
        out: &mut Vec<NodeIndex>,
    ) {
        let mut stack: Vec<(NodeIndex, u8)> = vec![(start, 0)];
        while let Some((node, depth)) = stack.pop() {
            if !visited.insert(node) {
                continue;
            }
            if node != start {
                out.push(node);
            }
            if depth >= k {
                continue;
            }
            // Iterate neighbors, prioritizing high-signal edges.
            let mut neighbors: Vec<(NodeIndex, EdgeKind)> = self
                .graph
                .edges_directed(node, dir)
                .filter_map(|e| {
                    let target = if dir == petgraph::Direction::Outgoing {
                        e.target()
                    } else {
                        e.source()
                    };
                    Some((target, e.weight().kind.clone()))
                })
                .collect();
            neighbors.sort_by_key(|(_, k)| rank(k));
            for (n, _) in neighbors {
                stack.push((n, depth + 1));
            }
        }
        let _ = visited; // silence on unused warnings in some paths
    }

    /// Convenience: seed by name, expand k hops, return the CodeNodes.
    pub fn query(&self, seed_name: &str, k: u8) -> Result<Vec<&CodeNode>, GraphError> {
        let seed = self
            .get(seed_name)
            .ok_or_else(|| GraphError::NodeNotFound(seed_name.to_string()))?;
        let mut nodes = vec![self.node(seed)];
        for idx in self.subgraph(seed, k) {
            nodes.push(self.node(idx));
        }
        Ok(nodes.into_iter().flatten().collect())
    }

    /// Render a subgraph as a compact, model-friendly adjacency summary.
    pub fn summarize(&self, seed_name: &str, k: u8) -> Result<String, GraphError> {
        let seed = self
            .get(seed_name)
            .ok_or_else(|| GraphError::NodeNotFound(seed_name.to_string()))?;
        let mut lines = Vec::new();
        if let Some(n) = self.node(seed) {
            lines.push(format!("# {}", n.qualified));
        }
        for idx in self.subgraph(seed, k) {
            if let Some(n) = self.node(idx) {
                lines.push(format!("- {} {} ({})", kind_label(&n.kind), n.name, n.qualified));
            }
        }
        Ok(lines.join("\n"))
    }
}

impl Default for KnowledgeGraph {
    fn default() -> Self {
        Self::new()
    }
}

fn rank(k: &EdgeKind) -> u8 {
    match k {
        EdgeKind::Calls => 0,
        EdgeKind::References => 1,
        EdgeKind::Imports => 2,
        EdgeKind::Defines => 3,
        EdgeKind::Contains => 4,
    }
}

fn kind_label(k: &NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Module => "module",
        NodeKind::Function => "fn",
        NodeKind::Type => "type",
        NodeKind::Trait => "trait",
        NodeKind::Variable => "var",
    }
}

/// A very lightweight Rust symbol extractor: pulls `fn name`, `struct Name`,
/// `trait Name`, `enum Name` declarations and call sites `name(`.
///
/// This is intentionally regex-based — it powers a *baseline* knowledge graph
/// suitable for demos and small codebases. A production deployment would swap
/// in a tree-sitter-based extractor behind the same interface.
pub struct RustExtractor {
    decl_re: Regex,
    call_re: Regex,
    use_re: Regex,
}

impl Default for RustExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl RustExtractor {
    pub fn new() -> Self {
        Self {
            decl_re: Regex::new(
                r"(?:pub\s+)?(?:async\s+)?(?:fn|struct|enum|trait|type)\s+([A-Za-z_][A-Za-z0-9_]*)",
            )
            .unwrap(),
            call_re: Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap(),
            use_re: Regex::new(r"use\s+([A-Za-z0-9_:{} ,]+);").unwrap(),
        }
    }

    /// Extract declarations and call edges from one file into `graph`.
    pub fn ingest(&self, graph: &mut KnowledgeGraph, path: &str, src: &str) -> NodeIndex {
        let file_idx = graph.add_node(CodeNode {
            kind: NodeKind::File,
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            qualified: path.to_string(),
            span: None,
        });

        let mut declared: Vec<(NodeIndex, String)> = Vec::new();
        for caps in self.decl_re.captures_iter(src) {
            let name = caps.get(1).unwrap().as_str().to_string();
            let kind = classify_keyword(&caps[0]);
            let qualified = format!("{path}::{name}");
            let sym = CodeNode {
                kind,
                name: name.clone(),
                qualified,
                span: None,
            };
            let idx = graph.add_node(sym);
            graph.add_edge(file_idx, idx, EdgeKind::Defines);
            graph.add_edge(file_idx, idx, EdgeKind::Contains);
            declared.push((idx, name));
        }

        // Intra-file call edges.
        for caps in self.call_re.captures_iter(src) {
            let callee = caps.get(1).unwrap().as_str();
            if BUILTINS.contains(&callee) {
                continue;
            }
            for (decl_idx, _) in &declared {
                let edges: Vec<_> = graph
                    .graph
                    .edges_directed(*decl_idx, petgraph::Direction::Outgoing)
                    .filter(|e| e.weight().kind == EdgeKind::Calls)
                    .map(|e| e.target())
                    .collect();
                if edges.iter().any(|t| {
                    graph
                        .node(*t)
                        .map_or(false, |n| n.name == callee)
                }) {
                    continue;
                }
                // Lazy: add the call edge if the callee is declared elsewhere.
                // In a real extractor this requires a global symbol table; here
                // we approximate by linking to the first symbol with this name.
                if let Some(&target) = graph
                    .by_name
                    .iter()
                    .find(|(qn, _)| qn.ends_with(&format!("::{callee}")))
                    .map(|(_, idx)| idx)
                {
                    graph.add_edge(*decl_idx, target, EdgeKind::Calls);
                }
            }
        }

        // Imports.
        for caps in self.use_re.captures_iter(src) {
            let path_str = caps.get(1).unwrap().as_str().to_string();
            let module = path_str.split("::").next().unwrap_or(&path_str).to_string();
            let mod_idx = graph.add_node(CodeNode {
                kind: NodeKind::Module,
                name: module.clone(),
                qualified: module,
                span: None,
            });
            graph.add_edge(file_idx, mod_idx, EdgeKind::Imports);
        }

        file_idx
    }
}

fn classify_keyword(decl_text: &str) -> NodeKind {
    if decl_text.contains("fn ") {
        NodeKind::Function
    } else if decl_text.contains("trait ") {
        NodeKind::Trait
    } else if decl_text.contains("struct ") || decl_text.contains("enum ") {
        NodeKind::Type
    } else {
        NodeKind::Type
    }
}

const BUILTINS: &[&str] = &[
    "if", "for", "while", "match", "let", "fn", "pub", "use", "mod", "struct", "enum", "trait",
    "impl", "self", "Self", "return", "Some", "None", "Ok", "Err", "println", "print", "format",
    "vec", "Box", "Arc", "Mutex", "Vec", "HashMap", "String", "str",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_and_query_callers() {
        let mut g = KnowledgeGraph::new();
        let ext = RustExtractor::new();
        ext.ingest(
            &mut g,
            "a.rs",
            r#"
            pub fn alpha() { beta(); }
            pub fn beta() {}
            "#,
        );

        // beta is defined in a.rs; alpha calls it.
        let beta_qn = g
            .by_name
            .keys()
            .find(|k| k.ends_with("::beta"))
            .unwrap()
            .clone();
        let sub = g.summarize(&beta_qn, 1).unwrap();
        assert!(sub.contains("alpha"), "summarize: {sub}");
    }

    #[test]
    fn k_hop_returns_related_files() {
        let mut g = KnowledgeGraph::new();
        let ext = RustExtractor::new();
        ext.ingest(
            &mut g,
            "lib.rs",
            r#"
            pub fn parse_config() {}
            pub fn load() { parse_config(); }
            "#,
        );
        let qn = g
            .by_name
            .keys()
            .find(|k| k.ends_with("::parse_config"))
            .unwrap()
            .clone();
        let nodes = g.query(&qn, 1).unwrap();
        assert!(nodes.iter().any(|n| n.name == "load"));
    }
}
