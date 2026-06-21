# LLM-VM Extended CLI Agent

A deterministic coding agent that stays stable under poor API conditions — 40 RPM limits, output truncation, tool-call failures.

## Problem

Open models (Nvidia NIM, vLLM, llama.cpp, etc.) offer strong reasoning but suffer from:
- Tool-call parse errors (10–30% failure rate)
- Output truncation / mid-stream disconnects
- Context forgetting on long sessions
- Error retries burning through tight RPM budgets

This project builds a **deterministic VM** with **preventive protocols** on top of an OpenAI-compatible LLM client, so that even under adversarial API conditions the agent completes tasks reliably.

## Architecture

```
┌─────────────────────────────────────┐
│  CLI (clap + ratatui TUI)          │  ← User interface
├─────────────────────────────────────┤
│  LLM Core (OpenAI/NIM client +     │  ← HTTP transport + tool exec
│              tool registry)          │
├─────────────────────────────────────┤
│  Deterministic VM + DAG + WAL       │  ← Core execution engine
├─────────────────────────────────────┤
│  Protocol Layer (5 protocols)       │  ← Robustness guarantees
├─────────────────────────────────────┤
│  Headroom / GraphRAG / gPTE         │  ← Optimization layer
└─────────────────────────────────────┘
```

## Crates

| Crate | Description |
|-------|-------------|
| `llm-vm-cli` | Binary entry-point: `run`, `dag`, `repl`, `inspect` subcommands |
| `llm-vm-core` | LLM client (OpenAI-compatible) + tool execution (read/write/shell) |
| `llm-vm-vm` | Deterministic VM: DAG execution engine with WAL-backed idempotency |
| `llm-vm-wal` | Write-Ahead Log with SHA-256 content-addressed keys (SQLite) |
| `llm-vm-protocol` | Guided Decoding, Sentinel+Overlap Diff, Hedge Requests |
| `llm-vm-headroom` | Pre-LLM compression: JSON / Code / Text strategies |
| `llm-vm-graphrag` | Codebase knowledge graph for multi-hop reasoning |
| `llm-vm-gpte` | Graph-task prompt tuning (LEAP-style template registry) |

## 5 Foundational Protocols

### ① Guided Decoding
Constrain LLM output via JSON Schema so parse errors are **physically impossible**.
The schema is sent as `response_format` to OpenAI-compatible endpoints; client-side validation catches any non-compliant response and triggers a pin-point VM retry.

### ② DAG + Deterministic VM
Tasks are decomposed into a directed acyclic graph of nodes. Each node is executed in topological order. On failure, only the failing node (and its dependents) are retried — never the entire graph.

### ③ WAL + Idempotency
Every node execution is recorded with a `SHA-256(action_id || inputs)` key. Re-executing the same node short-circuits to the cached result. Double execution is **impossible**.

### ④ Sentinel + Overlap Diff
A trailing marker (`<<LLMVM_EOF>>`) is appended to every expected output. If the marker is missing, the output was truncated. A continuation request is issued and the two halves are spliced at their maximum overlap.

### ⑤ Hedge Requests
If the primary request hasn't produced a first token within a configurable TTFT threshold, a hedge (duplicate) request is fired. Both race; the loser is cancelled. On healthy endpoints, the hedge never fires.

## 3 Extension Technologies

| Technology | Effect |
|-----------|--------|
| **Headroom** | Compresses tool output / logs by 60–95% before sending to LLM |
| **GraphRAG** | Builds a code knowledge graph for multi-hop dependency queries |
| **gPTE** | Prompt template registry with LEAP-style success-rate scoring |

## Quick Start

```bash
# Build
cargo build --release

# Run a one-shot prompt
export NIM_API_KEY="nvapi-..."
cargo run -- run "Explain this codebase in 3 bullet points"

# Execute a DAG spec
cargo run -- dag plan.json

# Interactive TUI session
cargo run -- repl

# Inspect WAL records
cargo run -- inspect
```

### DAG Spec Format

```json
{
  "nodes": [
    { "id": "a", "action": "tool.read_file", "inputs": { "path": "Cargo.toml" } },
    { "id": "b", "action": "llm.complete", "inputs": {
      "messages": [
        {"role": "system", "content": "Summarize."},
        {"role": "user", "content": "{{$ref: {\"node\": \"a\", \"field\": \"content\"}}}"}
      ]
    }}
  ],
  "edges": [["a", "b"]]
}
```

## Development

```bash
# Test all crates
cargo test --workspace

# Build release binary
cargo build --release --bin llmvm
```

## Requirements

- Rust 1.75+
- SQLite3 (bundled via `rusqlite`)
- An OpenAI-compatible API endpoint (Nvidia NIM, vLLM, Ollama, etc.)

## License

MIT
