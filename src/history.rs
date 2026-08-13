use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::model::ActionResult;

const DEFAULT_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_RETAINED_FILES: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub timestamp: DateTime<Utc>,
    pub distribution: String,
    pub command: String,
    pub results: Vec<ActionResult>,
}

pub struct HistoryStore {
    path: PathBuf,
    max_file_bytes: u64,
    retained_files: usize,
}

impl HistoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self::with_retention(path, DEFAULT_MAX_FILE_BYTES, DEFAULT_RETAINED_FILES)
    }

    fn with_retention(path: PathBuf, max_file_bytes: u64, retained_files: usize) -> Self {
        Self {
            path,
            max_file_bytes,
            retained_files,
        }
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
        self.ensure_parent()?;
        let _lock = HistoryLock::exclusive(&self.lock_path())?;
        self.secure_existing_history()?;
        let mut encoded = serde_json::to_vec(record)?;
        encoded.push(b'\n');
        self.rotate_if_needed(encoded.len() as u64)?;

        let mut file = secure_open_options()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        secure_permissions(&file)?;
        file.write_all(&encoded)?;
        file.flush()?;
        Ok(())
    }

    pub fn read_recent(&self, limit: usize) -> Result<Vec<HistoryRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.ensure_parent()?;
        let _lock = HistoryLock::shared(&self.lock_path())?;
        let mut recent = VecDeque::new();
        for path in self.history_paths_oldest_first() {
            self.read_file(&path, limit, &mut recent)?;
        }
        let mut recent: Vec<_> = recent.into_iter().collect();
        recent.reverse();
        Ok(recent)
    }

    fn ensure_parent(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        Ok(())
    }

    fn rotate_if_needed(&self, additional_bytes: u64) -> Result<()> {
        let current_bytes = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if current_bytes == 0
            || current_bytes.saturating_add(additional_bytes) <= self.max_file_bytes
        {
            return Ok(());
        }
        if self.retained_files == 0 {
            secure_open_options()
                .write(true)
                .truncate(true)
                .open(&self.path)
                .with_context(|| format!("failed to truncate {}", self.path.display()))?;
            return Ok(());
        }
        for index in (2..=self.retained_files).rev() {
            let source = self.rotated_path(index - 1);
            let destination = self.rotated_path(index);
            rename_if_present(&source, &destination)?;
        }
        fs::rename(&self.path, self.rotated_path(1))
            .with_context(|| format!("failed to rotate {}", self.path.display()))?;
        Ok(())
    }

    fn secure_existing_history(&self) -> Result<()> {
        for path in self.history_paths_oldest_first() {
            let file = match secure_open_options().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            secure_permissions(&file)?;
        }
        Ok(())
    }

    fn read_file(
        &self,
        path: &Path,
        limit: usize,
        records: &mut VecDeque<HistoryRecord>,
    ) -> Result<()> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? != 0 {
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                line.clear();
                continue;
            }
            let Ok(record) = serde_json::from_slice(&line) else {
                line.clear();
                continue;
            };
            records.push_back(record);
            if records.len() > limit {
                records.pop_front();
            }
            line.clear();
        }
        Ok(())
    }

    fn history_paths_oldest_first(&self) -> Vec<PathBuf> {
        (1..=self.retained_files)
            .rev()
            .map(|index| self.rotated_path(index))
            .chain(std::iter::once(self.path.clone()))
            .collect()
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        suffixed_path(&self.path, &format!(".{index}"))
    }

    fn lock_path(&self) -> PathBuf {
        suffixed_path(&self.path, ".lock")
    }
}

struct HistoryLock {
    file: File,
}

impl HistoryLock {
    fn exclusive(path: &Path) -> Result<Self> {
        let file = open_lock_file(path)?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self { file })
    }

    fn shared(path: &Path) -> Result<Self> {
        let file = open_lock_file(path)?;
        FileExt::lock_shared(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    let file = secure_open_options()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open history lock {}", path.display()))?;
    secure_permissions(&file)?;
    Ok(file)
}

fn secure_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn secure_permissions(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = file.metadata()?.permissions();
        if permissions.mode() & 0o777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}

fn rename_if_present(source: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to rotate {} to {}",
                source.display(),
                destination.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use tempfile::tempdir;

    fn record(command: impl Into<String>) -> HistoryRecord {
        HistoryRecord {
            timestamp: Utc::now(),
            distribution: "Test".into(),
            command: command.into(),
            results: Vec::new(),
        }
    }

    #[test]
    fn returns_newest_history_first() {
        let root = tempdir().unwrap();
        let store = HistoryStore::new(root.path().join("history.jsonl"));
        for command in ["first", "second", "third"] {
            store.append(&record(command)).unwrap();
        }
        let records = store.read_recent(2).unwrap();
        assert_eq!(records[0].command, "third");
        assert_eq!(records[1].command, "second");
    }

    #[test]
    fn skips_corrupt_records_without_losing_valid_history() {
        let root = tempdir().unwrap();
        let store = HistoryStore::new(root.path().join("history.jsonl"));
        store.append(&record("before")).unwrap();
        OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap()
            .write_all(b"not valid json\n\xff\n")
            .unwrap();
        store.append(&record("after")).unwrap();

        let records = store.read_recent(10).unwrap();
        let commands: Vec<_> = records
            .iter()
            .map(|record| record.command.as_str())
            .collect();
        assert_eq!(commands, ["after", "before"]);
    }

    #[test]
    fn rotates_history_and_drops_files_beyond_retention() {
        let root = tempdir().unwrap();
        let store = HistoryStore::with_retention(root.path().join("history.jsonl"), 1, 2);
        for command in ["first", "second", "third", "fourth"] {
            store.append(&record(command)).unwrap();
        }

        let records = store.read_recent(10).unwrap();
        let commands: Vec<_> = records
            .iter()
            .map(|record| record.command.as_str())
            .collect();
        assert_eq!(commands, ["fourth", "third", "second"]);
        assert!(store.rotated_path(2).is_file());
        assert!(!store.rotated_path(3).exists());
    }

    #[test]
    fn serializes_concurrent_writers_without_corrupting_records() {
        let root = tempdir().unwrap();
        let store = Arc::new(HistoryStore::new(root.path().join("history.jsonl")));
        let writers = 6;
        let records_per_writer = 25;
        let handles: Vec<_> = (0..writers)
            .map(|writer| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    for index in 0..records_per_writer {
                        store
                            .append(&record(format!("writer-{writer}-{index}")))
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let records = store.read_recent(writers * records_per_writer).unwrap();
        let commands: HashSet<_> = records.into_iter().map(|record| record.command).collect();
        assert_eq!(commands.len(), writers * records_per_writer);
    }

    #[cfg(unix)]
    #[test]
    fn protects_history_and_lock_files_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let store = HistoryStore::new(root.path().join("history.jsonl"));
        fs::write(store.path(), b"").unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
        store.append(&record("private")).unwrap();

        let history_mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        let lock_mode = fs::metadata(store.lock_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(history_mode, 0o600);
        assert_eq!(lock_mode, 0o600);
    }
}
