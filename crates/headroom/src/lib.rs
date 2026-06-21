//! Headroom: pre-LLM compression of tool output / logs.
//!
//! Sending raw `cargo build` output, full file dumps, or stack traces into the
//! model context burns tokens at an alarming rate. Headroom sits *between* the
//! tool execution layer and the LLM client and compresses payloads before they
//! ever hit the wire, reclaiming 60–95% of the tokens.
//!
//! Three strategies, matched to payload shape:
//!
//! - [`SmartCrusher`]  — structured JSON: drops nulls/empties, compacts arrays
//!   of identical-shape objects into row tables, abbreviates long strings.
//! - [`CodeCompressor`] — source code: line-based dedup, comment stripping,
//!   blank-run compaction, import grouping. Preserves structure for the model.
//! - [`KompressBase`]   — unstructured text / logs: collapse repeated lines,
//!   trim ANSI, summarize runs of identical log lines.

pub mod code;
pub mod json;
pub mod text;

pub use code::{compress_code, CodeCompressor};
pub use json::{compress_json, SmartCrusher};
pub use text::{compress_text, KompressBase};

/// Knobs shared by every compressor.
#[derive(Debug, Clone)]
pub struct CompressConfig {
    /// Maximum chars of any single string value before it gets truncated with
    /// an ellipsis marker.
    pub max_string_chars: usize,
    /// Maximum number of repeated identical lines to keep before collapsing
    /// into `… (N more)`.
    pub max_repeated_lines: usize,
    /// Strip ANSI escape sequences.
    pub strip_ansi: bool,
    /// Aggressiveness 0..=100. Higher = more aggressive truncation.
    pub level: u8,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            max_string_chars: 512,
            max_repeated_lines: 3,
            strip_ansi: true,
            level: 60,
        }
    }
}

/// Pick a compressor based on a content hint and apply it.
pub fn compress(input: &str, kind: PayloadKind, cfg: &CompressConfig) -> String {
    match kind {
        PayloadKind::Json => compress_json(input, cfg),
        PayloadKind::Code(_) => compress_code(input, cfg),
        PayloadKind::Text => compress_text(input, cfg),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    Json,
    Code(code::Language),
    Text,
}

#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    fn json_round_trip_keeps_meaningful_data() {
        // Note: real input has the repeated string; we approximate with a long literal.
        let input = r#"{"name":"a","empty_arr":[],"empty_obj":{},"note":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        let cfg = CompressConfig::default();
        let out = compress_json(input, &cfg);
        // empties dropped
        assert!(!out.contains("empty_arr"));
        assert!(!out.contains("empty_obj"));
        assert!(out.contains("\"name\":\"a\""));
    }

    #[test]
    fn text_collapses_repeated_lines() {
        let input = "line\nline\nline\nline\nline\nunique";
        let cfg = CompressConfig::default();
        let out = compress_text(input, &cfg);
        assert!(out.contains("unique"));
        assert!(out.contains("more") || out.matches("line").count() < input.matches("line").count());
    }
}
