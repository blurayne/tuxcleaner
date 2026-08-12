use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::ActionResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub timestamp: DateTime<Utc>,
    pub distribution: String,
    pub command: String,
    pub results: Vec<ActionResult>,
}

pub struct HistoryStore {
    path: PathBuf,
}

impl HistoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn system_default() -> Result<Self> {
        let base = dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .ok_or_else(|| anyhow::anyhow!("could not determine state directory"))?;
        Ok(Self::new(base.join("tuxcleaner/history.jsonl")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &HistoryRecord) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    pub fn read_recent(&self, limit: usize) -> Result<Vec<HistoryRecord>> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            records.push(
                serde_json::from_str(&line).with_context(|| {
                    format!("invalid history record in {}", self.path.display())
                })?,
            );
        }
        let start = records.len().saturating_sub(limit);
        let mut recent = records.split_off(start);
        recent.reverse();
        Ok(recent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn returns_newest_history_first() {
        let root = tempdir().unwrap();
        let store = HistoryStore::new(root.path().join("history.jsonl"));
        for command in ["first", "second", "third"] {
            store
                .append(&HistoryRecord {
                    timestamp: Utc::now(),
                    distribution: "Test".into(),
                    command: command.into(),
                    results: Vec::new(),
                })
                .unwrap();
        }
        let records = store.read_recent(2).unwrap();
        assert_eq!(records[0].command, "third");
        assert_eq!(records[1].command, "second");
    }
}
