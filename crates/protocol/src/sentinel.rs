//! ④ Sentinel + Overlap Diff.
//!
//! Truncated streams are a fact of life on rate-limited or capacity-starved
//! endpoints: the server returns 200 with a partial body, or the SSE stream
//! simply stops emitting. The classic mitigation is a **sentinel marker**
//! appended at the very end of every complete generation. If we don't see it,
//! we know the output was cut.
//!
//! To recover we issue a continuation prompt carrying the tail of the partial
//! output; the model resumes. Because the resume point is fuzzy, we then run
//! an **overlap diff** to find the largest suffix of the partial text that is
//! also a prefix of the continuation, and splice exactly once — no duplicated
//! prose, no dropped tokens.

use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

/// Canonical end-of-output marker. Kept short and improbable in natural text.
pub const DEFAULT_SENTINEL: &str = "<<LLMVM_EOF>>";

#[derive(Debug, Clone)]
pub struct SentinelConfig {
    /// The literal marker to look for.
    pub marker: String,
    /// Strip the marker (and any trailing whitespace) from the assembled text.
    pub strip_marker: bool,
    /// Minimum overlap window to consider when diffing partial + continuation.
    pub min_overlap: usize,
    /// Maximum number of continuation attempts before giving up.
    pub max_continuations: usize,
}

impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            marker: DEFAULT_SENTINEL.into(),
            strip_marker: true,
            min_overlap: 8,
            max_continuations: 4,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StitchError {
    #[error("exhausted continuation budget ({0} attempts) without seeing sentinel")]
    Exhausted(usize),
    #[error("no overlap found between partial tail and continuation")]
    NoOverlap,
}

/// Was the given text terminated by the sentinel marker?
pub fn is_complete(text: &str, cfg: &SentinelConfig) -> bool {
    text.contains(&cfg.marker)
}

/// Strip the sentinel marker (and trailing whitespace after it) from `text`.
pub fn strip(text: &str, cfg: &SentinelConfig) -> String {
    if let Some(idx) = text.find(&cfg.marker) {
        text[..idx].trim_end().to_string()
    } else {
        text.trim_end().to_string()
    }
}

/// Find the largest `k` such that `partial[-k..] == continuation[..k]`.
///
/// Returns the splice point `k` (number of shared bytes) or `None` if no
/// overlap of at least `cfg.min_overlap` exists.
pub fn overlap_len(partial: &str, continuation: &str, cfg: &SentinelConfig) -> Option<usize> {
    let p = partial.as_bytes();
    let c = continuation.as_bytes();
    let max = p.len().min(c.len());
    if max < cfg.min_overlap {
        return None;
    }
    let mut best = None;
    for k in (cfg.min_overlap..=max).rev() {
        if &p[p.len() - k..] == &c[..k] {
            best = Some(k);
            break;
        }
    }
    best
}

/// Splice `partial` and `continuation` at their largest overlap. The result
/// has the marker stripped if `cfg.strip_marker` is set.
pub fn stitch(partial: &str, continuation: &str, cfg: &SentinelConfig) -> Result<String, StitchError> {
    let partial_clean = strip(partial, cfg);
    let continuation_clean = strip(continuation, cfg);

    let k = overlap_len(&partial_clean, &continuation_clean, cfg)
        .ok_or(StitchError::NoOverlap)?;
    let mut out = String::with_capacity(partial_clean.len() + continuation_clean.len() - k);
    out.push_str(&partial_clean);
    out.push_str(&continuation_clean[k..]);
    Ok(out)
}

/// A streaming accumulator that knows how to detect truncation and feed a
/// resume prompt. The caller is responsible for actually issuing the
/// continuation request — this type just tracks state and offers the splice.
pub struct SentinelStream {
    cfg: SentinelConfig,
    buf: Arc<Mutex<String>>,
}

impl SentinelStream {
    pub fn new(cfg: SentinelConfig) -> Self {
        Self {
            cfg,
            buf: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Append a chunk received from the model.
    pub async fn push(&self, chunk: &str) {
        self.buf.lock().await.push_str(chunk);
    }

    /// Snapshot the accumulated text so far.
    pub async fn snapshot(&self) -> String {
        self.buf.lock().await.clone()
    }

    /// True if the accumulated text has seen the sentinel.
    pub async fn complete(&self) -> bool {
        let snap = self.snapshot().await;
        is_complete(&snap, &self.cfg)
    }

    /// Produce a clean final string (sentinel stripped). Only meaningful once
    /// [`complete`] is true.
    pub async fn finalize(&self) -> String {
        let snap = self.snapshot().await;
        strip(&snap, &self.cfg)
    }

    /// Splice a continuation into the buffer. Returns the new buffer state.
    ///
    /// If the continuation itself contains the sentinel marker, the stitched
    /// buffer is marked complete (the marker is re-appended after splicing,
    /// since [`stitch`] strips it from both halves).
    pub async fn splice_continuation(&self, continuation: &str) -> Result<String, StitchError> {
        let mut buf = self.buf.lock().await;
        let partial = buf.clone();
        let continuation_complete = is_complete(continuation, &self.cfg);
        let mut stitched = stitch(&partial, continuation, &self.cfg)?;
        if continuation_complete {
            stitched.push_str(&self.cfg.marker);
        }
        *buf = stitched.clone();
        Ok(stitched)
    }

    /// Build a continuation prompt that asks the model to resume from `tail`.
    /// The tail is the last `n` chars of the current buffer.
    pub async fn resume_prompt(&self, tail_chars: usize) -> String {
        let snap = self.snapshot().await;
        let tail = if snap.len() > tail_chars {
            &snap[snap.len() - tail_chars..]
        } else {
            &snap
        };
        format!(
            "The previous output was truncated. Continue exactly from this point, \
             matching style and indentation. Do not repeat the text below — resume \
             from where it leaves off:\n\n…{tail}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SentinelConfig {
        SentinelConfig {
            marker: "<<LLMVM_EOF>>".into(),
            strip_marker: true,
            min_overlap: 4,
            max_continuations: 4,
        }
    }

    #[test]
    fn complete_payload_detected() {
        let t = "hello world <<LLMVM_EOF>>";
        assert!(is_complete(t, &cfg()));
        assert_eq!(strip(t, &cfg()), "hello world");
    }

    #[test]
    fn truncated_payload_detected() {
        let t = "hello wor"; // cut mid-word
        assert!(!is_complete(t, &cfg()));
    }

    #[test]
    fn overlap_finds_largest_window() {
        let partial = "the quick brown fox";
        let cont = "brown fox jumps over";
        assert_eq!(overlap_len(partial, cont, &cfg()), Some("brown fox".len()));
    }

    #[test]
    fn stitch_removes_duplicate_region() {
        let partial = "fn main() { println!";
        let cont = "println!(\"hi\"); } <<LLMVM_EOF>>";
        let stitched = stitch(partial, cont, &cfg()).unwrap();
        assert_eq!(stitched, "fn main() { println!(\"hi\"); }");
    }

    #[test]
    fn stitch_no_overlap_errors() {
        let partial = "completely different";
        let cont = "no shared text at all <<LLMVM_EOF>>";
        assert_eq!(stitch(partial, cont, &cfg()), Err(StitchError::NoOverlap));
    }

    #[tokio::test]
    async fn stream_detects_truncation_and_resumes() {
        let s = SentinelStream::new(cfg());
        s.push("The first sentence. The second sen").await;
        assert!(!s.complete().await);

        // Model resumes mid-word; overlap diff splices cleanly.
        let resume = "second sentence continues here. <<LLMVM_EOF>>";
        let stitched = s.splice_continuation(resume).await.unwrap();
        assert!(s.complete().await);
        assert_eq!(stitched, "The first sentence. The second sentence continues here.");
    }
}
