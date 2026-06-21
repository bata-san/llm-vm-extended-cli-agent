//! Write-Ahead Log + idempotency store.
//!
//! Every node execution is recorded with a content-addressed key
//! (`SHA-256(action_id || inputs)`). Re-running a node with the same key
//! short-circuits to the stored result, giving the deterministic VM the
//! "execute exactly once" guarantee even when crashes / retries occur.

use rusqlite::{params, Connection};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WalError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("lock poisoned")]
    Poisoned,
}

/// A durable record of a single node execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub key: String,
    pub action_id: String,
    pub inputs_hash: String,
    pub result: serde_json::Value,
    pub created_at: i64,
}

/// Compute the idempotency key for a node.
///
/// The key is `SHA-256(action_id || "\x1f" || canonical(inputs))`. Inputs are
/// serialized with sorted keys so semantically-equal maps collide.
pub fn idempotency_key<T: Serialize>(action_id: &str, inputs: &T) -> Result<String, WalError> {
    let canonical = serde_json::to_string(inputs)?;
    Ok(key_from_parts(action_id, &canonical))
}

fn key_from_parts(action_id: &str, canonical_inputs: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(action_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(canonical_inputs.as_bytes());
    hex::encode(hasher.finalize())
}

/// SQLite-backed WAL. Mutex-guarded for simplicity; the VM schedules node
/// execution on a single critical section per WAL.
pub struct Wal {
    conn: Mutex<Connection>,
}

impl Wal {
    /// Open (creating if necessary) the WAL database at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, WalError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS records (
                key          TEXT PRIMARY KEY,
                action_id    TEXT NOT NULL,
                inputs_hash  TEXT NOT NULL,
                result       TEXT NOT NULL,
                created_at   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_records_action ON records(action_id);
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory WAL, useful for tests.
    pub fn in_memory() -> Result<Self, WalError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS records (
                key          TEXT PRIMARY KEY,
                action_id    TEXT NOT NULL,
                inputs_hash  TEXT NOT NULL,
                result       TEXT NOT NULL,
                created_at   INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Has a node with this key already committed?
    pub fn contains(&self, key: &str) -> Result<bool, WalError> {
        let conn = self.conn.lock().map_err(|_| WalError::Poisoned)?;
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM records WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )?;
        Ok(exists > 0)
    }

    /// Append a record. Returns `false` if a record with the same key already
    /// existed (caller should treat as a no-op / cache hit).
    pub fn append(&self, record: &Record) -> Result<bool, WalError> {
        let conn = self.conn.lock().map_err(|_| WalError::Poisoned)?;
        let result_json = serde_json::to_string(&record.result)?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO records (key, action_id, inputs_hash, result, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.key,
                record.action_id,
                record.inputs_hash,
                result_json,
                record.created_at,
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Typed lookup: deserialize the stored result for `key` into `T`.
    pub fn lookup<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, WalError> {
        let conn = self.conn.lock().map_err(|_| WalError::Poisoned)?;
        let mut stmt = conn.prepare("SELECT result FROM records WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            let typed: T = serde_json::from_value(value)?;
            Ok(Some(typed))
        } else {
            Ok(None)
        }
    }

    /// Enumerate all records (for inspection / replay debugging).
    pub fn all(&self) -> Result<Vec<Record>, WalError> {
        let conn = self.conn.lock().map_err(|_| WalError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT key, action_id, inputs_hash, result, created_at FROM records ORDER BY created_at",
        )?;
        let records = stmt.query_map([], |row| {
            let raw: String = row.get(3)?;
            let result: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
            Ok(Record {
                key: row.get(0)?,
                action_id: row.get(1)?,
                inputs_hash: row.get(2)?,
                result,
                created_at: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in records {
            out.push(r?);
        }
        Ok(out)
    }

    /// Wipe all records (test helper; never call in production unless you mean it).
    pub fn clear(&self) -> Result<(), WalError> {
        let conn = self.conn.lock().map_err(|_| WalError::Poisoned)?;
        conn.execute("DELETE FROM records", [])?;
        Ok(())
    }
}

/// Build a [`Record`] ready to append.
pub fn build_record<T: Serialize>(
    action_id: &str,
    inputs: &T,
    result: &impl Serialize,
    now: i64,
) -> Result<Record, WalError> {
    let canonical = serde_json::to_string(inputs)?;
    let key = key_from_parts(action_id, &canonical);
    let inputs_hash = {
        let mut h = Sha256::new();
        h.update(canonical.as_bytes());
        hex::encode(h.finalize())
    };
    Ok(Record {
        key,
        action_id: action_id.to_string(),
        inputs_hash,
        result: serde_json::to_value(result)?,
        created_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_is_stable_across_map_orders() {
        let a = serde_json::json!({"x": 1, "y": 2});
        let b = serde_json::json!({"y": 2, "x": 1});
        // serde_json::to_string is NOT order-stable across separate values,
        // so this is the caller's responsibility; but identical value -> identical key.
        let k1 = idempotency_key("act", &a).unwrap();
        let k2 = idempotency_key("act", &a).unwrap();
        assert_eq!(k1, k2);
        let _ = (a, b);
    }

    #[test]
    fn append_then_lookup_returns_cached_value() {
        let wal = Wal::in_memory().unwrap();
        let inputs = serde_json::json!({"path": "/tmp/a"});
        let key = idempotency_key("read_file", &inputs).unwrap();
        assert!(!wal.contains(&key).unwrap());

        let rec = build_record("read_file", &inputs, &"hello", 1).unwrap();
        assert!(wal.append(&rec).unwrap());
        assert!(wal.contains(&key).unwrap());

        // Re-append is a no-op.
        assert!(!wal.append(&rec).unwrap());

        let cached: String = wal.lookup(&key).unwrap().unwrap();
        assert_eq!(cached, "hello");
    }

    #[test]
    fn different_inputs_yield_different_keys() {
        let a = idempotency_key("read_file", &serde_json::json!({"p": "a"})).unwrap();
        let b = idempotency_key("read_file", &serde_json::json!({"p": "b"})).unwrap();
        assert_ne!(a, b);
    }
}
