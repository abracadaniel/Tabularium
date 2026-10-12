//! Query history (DataGrip "Query History"), persisted as JSON lines.
//!
//! Every executed statement is appended to the file as one JSON object per line. Re-running
//! the same SQL on the same connection replaces the previous entry instead of adding a new
//! one (the file still receives the new line; loading applies the same de-duplication). The
//! history keeps at most [`DEFAULT_LIMIT`] entries; the file is rewritten (compacted) when it
//! grows more than 10% beyond the limit.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Maximum number of entries kept by default.
pub const DEFAULT_LIMIT: usize = 10_000;

/// One executed statement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    /// When the statement was executed.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Connection name (or id).
    pub connection: String,
    /// Current database/catalog, if any.
    pub database: Option<String>,
    /// Statement text.
    pub sql: String,
    /// Execution time.
    pub duration_ms: u64,
    /// Rows returned or affected.
    pub rows: Option<u64>,
    /// Error message for failed statements.
    pub error: Option<String>,
}

impl HistoryEntry {
    /// Entry stamped with the current time.
    pub fn now(connection: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            timestamp: chrono::Utc::now(),
            connection: connection.into(),
            database: None,
            sql: sql.into(),
            duration_ms: 0,
            rows: None,
            error: None,
        }
    }
}

/// Query history store.
#[derive(Debug)]
pub struct QueryHistory {
    path: Option<PathBuf>,
    entries: Vec<HistoryEntry>,
    limit: usize,
    /// Number of lines currently in the file (including superseded duplicates).
    file_lines: usize,
}

fn same_run(a: &HistoryEntry, b: &HistoryEntry) -> bool {
    a.connection == b.connection && a.sql.trim() == b.sql.trim()
}

impl QueryHistory {
    /// Open (or lazily create) a history file. Existing entries are loaded; corrupt lines are
    /// skipped.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_limit(path, DEFAULT_LIMIT)
    }

    /// Like [`open`](Self::open) with a custom entry limit.
    pub fn open_with_limit(path: impl Into<PathBuf>, limit: usize) -> Result<Self> {
        let path = path.into();
        let mut h = Self { path: Some(path.clone()), entries: Vec::new(), limit: limit.max(1), file_lines: 0 };
        match File::open(&path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let Ok(line) = line else { continue };
                    if line.trim().is_empty() {
                        continue;
                    }
                    h.file_lines += 1;
                    if let Ok(e) = serde_json::from_str::<HistoryEntry>(&line) {
                        h.push_memory(e);
                    }
                }
                h.trim_memory();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        }
        Ok(h)
    }

    /// History that is not persisted.
    pub fn in_memory() -> Self {
        Self { path: None, entries: Vec::new(), limit: DEFAULT_LIMIT, file_lines: 0 }
    }

    /// Change the entry limit (takes effect on the next [`add`](Self::add)).
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
    }

    fn push_memory(&mut self, e: HistoryEntry) {
        match self.entries.last_mut() {
            Some(last) if same_run(last, &e) => *last = e,
            _ => self.entries.push(e),
        }
    }

    fn trim_memory(&mut self) -> bool {
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.entries.drain(..excess);
            true
        } else {
            false
        }
    }

    fn rewrite(&mut self) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?);
            for e in &self.entries {
                serde_json::to_writer(&mut w, e)?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        self.file_lines = self.entries.len();
        Ok(())
    }

    /// Record an entry (appended to the file).
    pub fn add(&mut self, entry: HistoryEntry) -> Result<()> {
        self.push_memory(entry.clone());
        self.trim_memory();
        let Some(path) = self.path.clone() else { return Ok(()) };
        if self.file_lines + 1 > self.limit + self.limit / 10 {
            return self.rewrite();
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        self.file_lines += 1;
        Ok(())
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Entries whose SQL contains `query` (case-insensitive; empty matches all), optionally
    /// restricted to one connection, newest first.
    pub fn search(&self, query: &str, connection: Option<&str>) -> Vec<&HistoryEntry> {
        let needle = query.to_lowercase();
        self.entries
            .iter()
            .rev()
            .filter(|e| connection.is_none_or(|c| e.connection == c))
            .filter(|e| needle.is_empty() || e.sql.to_lowercase().contains(&needle))
            .collect()
    }

    /// Remove every entry (and truncate the file).
    pub fn clear(&mut self) -> Result<()> {
        self.entries.clear();
        if let Some(path) = &self.path {
            if path.exists() {
                File::create(path).with_context(|| format!("truncating {}", path.display()))?;
            }
        }
        self.file_lines = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry(conn: &str, sql: &str, secs: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: chrono::Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap(),
            connection: conn.into(),
            database: Some("db".into()),
            sql: sql.into(),
            duration_ms: 5,
            rows: Some(1),
            error: None,
        }
    }

    #[test]
    fn persistence_and_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("history.jsonl");
        {
            let mut h = QueryHistory::open(&path).unwrap();
            h.add(entry("pg", "select 1", 0)).unwrap();
            h.add(entry("pg", "select 1", 1)).unwrap(); // deduped
            h.add(entry("my", "select 1", 2)).unwrap();
            h.add(entry("pg", "SELECT * FROM users", 3)).unwrap();
            assert_eq!(h.entries().len(), 3);
            assert_eq!(h.entries()[0].timestamp, entry("pg", "", 1).timestamp);
        }
        // corrupt line is skipped
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{not json\n").unwrap();
        drop(f);
        let h = QueryHistory::open(&path).unwrap();
        assert_eq!(h.entries().len(), 3);
        assert_eq!(h.entries()[2].sql, "SELECT * FROM users");

        let found = h.search("SELECT", None);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].sql, "SELECT * FROM users"); // newest first
        assert_eq!(h.search("users", Some("my")).len(), 0);
        assert_eq!(h.search("select 1", Some("my")).len(), 1);
    }

    #[test]
    fn trimming_rewrites_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.jsonl");
        let mut h = QueryHistory::open_with_limit(&path, 10).unwrap();
        for i in 0..25 {
            h.add(entry("c", &format!("select {i}"), i)).unwrap();
        }
        assert_eq!(h.entries().len(), 10);
        assert_eq!(h.entries()[0].sql, "select 15");
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines <= 11, "file has {lines} lines");
        let h2 = QueryHistory::open_with_limit(&path, 10).unwrap();
        assert_eq!(h2.entries(), h.entries());
    }

    #[test]
    fn clear_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.jsonl");
        let mut h = QueryHistory::open(&path).unwrap();
        h.add(entry("c", "x", 0)).unwrap();
        h.clear().unwrap();
        assert!(h.entries().is_empty());
        assert!(QueryHistory::open(&path).unwrap().entries().is_empty());

        let mut m = QueryHistory::in_memory();
        m.add(entry("c", "x", 0)).unwrap();
        m.add(HistoryEntry::now("c", "y")).unwrap();
        assert_eq!(m.entries().len(), 2);
    }
}
