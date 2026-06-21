//! SmartCrusher: structured-JSON compression.
//!
//! The model rarely needs every null, empty container, or 10 KB string. We
//! parse the JSON, walk it, and rebuild a leaner version. When an array's
//! elements all share the same key set we collapse it into a compact
//! "header + rows" representation that's still valid JSON but far smaller.

use super::CompressConfig;

/// Compress a JSON document string. On parse failure we fall back to the
/// text compressor so callers don't have to branch.
pub fn compress_json(input: &str, cfg: &CompressConfig) -> String {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => {
            let crushed = crush_value(&v, cfg);
            // Pretty-serialize but compact (no whitespace) to save tokens.
            serde_json::to_string(&crushed).unwrap_or_else(|_| input.to_string())
        }
        Err(_) => super::compress_text(input, cfg),
    }
}

pub struct SmartCrusher;
impl SmartCrusher {
    pub fn compress(input: &str, cfg: &CompressConfig) -> String {
        compress_json(input, cfg)
    }
}

fn crush_value(v: &serde_json::Value, cfg: &CompressConfig) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => {
            serde_json::Value::String(truncate_str(s, cfg.max_string_chars))
        }
        serde_json::Value::Array(arr) => {
            let crushed: Vec<serde_json::Value> = arr
                .iter()
                .map(|e| crush_value(e, cfg))
                .collect();
            try_tabulate(&crushed).unwrap_or(serde_json::Value::Array(crushed))
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                // Drop nulls and empty containers.
                if is_empty(val) {
                    continue;
                }
                out.insert(k.clone(), crush_value(val, cfg));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

fn is_empty(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Null)
        || matches!(v, serde_json::Value::Array(a) if a.is_empty())
        || matches!(v, serde_json::Value::Object(o) if o.is_empty())
        || matches!(v, serde_json::Value::String(s) if s.is_empty())
}

/// If every element of `arr` is an object with the same key set, render as
/// `{"_header": [...keys], "_rows": [[v11,v12,...], ...]}`. Otherwise return None.
fn try_tabulate(arr: &[serde_json::Value]) -> Option<serde_json::Value> {
    if arr.len() < 2 {
        return None;
    }
    let mut keys: Option<Vec<String>> = None;
    for el in arr {
        let obj = el.as_object()?;
        let mut k: Vec<String> = obj.keys().cloned().collect();
        k.sort();
        match &keys {
            None => keys = Some(k),
            Some(existing) if existing == &k => {}
            _ => return None,
        }
    }
    let keys = keys?;
    let mut rows = Vec::with_capacity(arr.len());
    for el in arr {
        let obj = el.as_object()?;
        let row: Vec<serde_json::Value> =
            keys.iter().map(|k| obj.get(k).cloned().unwrap_or(serde_json::Value::Null)).collect();
        rows.push(serde_json::Value::Array(row));
    }
    Some(serde_json::json!({
        "_header": keys,
        "_rows": rows,
    }))
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("…");
        out
    }
}
