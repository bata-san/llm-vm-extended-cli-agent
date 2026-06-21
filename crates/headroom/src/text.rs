//! KompressBase: unstructured text / log compression.
//!
//! Logs are extremely repetitive: the same `[INFO] request handled` line might
//! appear 5000 times. Tokenizing that is pure waste. We collapse runs of
//! identical or near-identical lines into a single representative + count,
//! strip ANSI noise, and trim per-line length.

use super::CompressConfig;

pub struct KompressBase;
impl KompressBase {
    pub fn compress(input: &str, cfg: &CompressConfig) -> String {
        compress_text(input, cfg)
    }
}

pub fn compress_text(input: &str, cfg: &CompressConfig) -> String {
    let cleaned = if cfg.strip_ansi {
        strip_ansi(input)
    } else {
        input.to_string()
    };

    let max_lines = cfg.max_repeated_lines.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    let mut run: usize = 0;
    let mut suppressed: usize = 0;

    // Flush the current run into `out`. Emits up to `max_lines` copies and
    // records any surplus in `suppressed` for a trailing marker.
    let flush = |out: &mut Vec<String>,
                 current: &mut Option<String>,
                 run: &mut usize,
                 suppressed: &mut usize,
                 max: usize| {
        if let Some(line) = current.take() {
            let kept = (*run).min(max);
            for _ in 0..kept {
                out.push(line.clone());
            }
            if *run > max {
                *suppressed += *run - max;
            }
            *run = 0;
        }
    };

    for raw in cleaned.lines() {
        let line = raw.trim_end();
        match &current {
            Some(c) if c == line => {
                run += 1;
            }
            _ => {
                flush(&mut out, &mut current, &mut run, &mut suppressed, max_lines);
                current = Some(line.to_string());
                run = 1;
            }
        }
    }
    flush(&mut out, &mut current, &mut run, &mut suppressed, max_lines);

    let mut joined = out.join("\n");
    if suppressed > 0 {
        joined.push_str(&format!("\n  … ({} similar suppressed)", suppressed));
    }
    joined
}

/// Strip ANSI escape sequences (colors, cursor moves, etc.).
pub fn strip_ansi(input: &str) -> String {
    let re = regex::Regex::new(r"\x1B\[[0-9;?]*[ -/]*[@-~]").unwrap();
    re.replace_all(input, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_runs() {
        let input = "INFO start\nINFO tick\nINFO tick\nINFO tick\nINFO tick\nINFO end";
        let cfg = CompressConfig {
            max_repeated_lines: 1,
            ..Default::default()
        };
        let out = compress_text(input, &cfg);
        // Only one "INFO tick" survives, plus a suppression marker.
        assert_eq!(out.matches("INFO tick").count(), 1);
        assert!(out.contains("suppressed"));
        assert!(out.contains("INFO start"));
        assert!(out.contains("INFO end"));
    }

    #[test]
    fn ansi_stripped() {
        let input = "\x1B[32mgreen\x1B[0m";
        assert_eq!(strip_ansi(input), "green");
    }
}
