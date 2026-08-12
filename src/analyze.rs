use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskEntry {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LargeFile {
    pub path: PathBuf,
    pub size: u64,
    pub modified_unix: Option<u64>,
    pub app_data: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub root: PathBuf,
    pub entries: Vec<DiskEntry>,
    pub large_files: Vec<LargeFile>,
    pub total_size: u64,
    pub total_files: u64,
    pub skipped_entries: u64,
}

pub fn analyze(root: &Path, minimum_size: u64, max_depth: usize) -> Result<AnalysisReport> {
    if !root.exists() {
        bail!("analysis path does not exist: {}", root.display());
    }
    let root =
        fs::canonicalize(root).with_context(|| format!("failed to resolve {}", root.display()))?;
    let mut buckets: BTreeMap<PathBuf, (u64, bool)> = BTreeMap::new();
    let mut large_files = Vec::new();
    let mut total_size = 0_u64;
    let mut total_files = 0_u64;
    let mut skipped_entries = 0_u64;

    let walker = WalkDir::new(&root)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_visit);

    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                skipped_entries += 1;
                continue;
            }
        };
        if entry.path() == root {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_entries += 1;
                continue;
            }
        };
        let relative = match entry.path().strip_prefix(&root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let Some(first) = relative.components().next() else {
            continue;
        };
        let top = root.join(first.as_os_str());
        let top_is_dir = fs::metadata(&top)
            .map(|value| value.is_dir())
            .unwrap_or(false);
        let bucket = buckets.entry(top).or_insert((0, top_is_dir));

        if metadata.is_file() {
            let size = metadata.len();
            bucket.0 = bucket.0.saturating_add(size);
            total_size = total_size.saturating_add(size);
            total_files += 1;
            if size >= minimum_size {
                let modified_unix = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs());
                large_files.push(LargeFile {
                    path: entry.path().to_path_buf(),
                    size,
                    modified_unix,
                    app_data: relative.components().any(is_hidden_component),
                });
            }
        }
    }

    let mut entries: Vec<_> = buckets
        .into_iter()
        .map(|(path, (size, is_dir))| DiskEntry {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path,
            size,
            is_dir,
        })
        .collect();
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    large_files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));

    Ok(AnalysisReport {
        root,
        entries,
        large_files,
        total_size,
        total_files,
        skipped_entries,
    })
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !matches!(name.as_ref(), ".git" | "node_modules")
}

fn is_hidden_component(component: Component<'_>) -> bool {
    match component {
        Component::Normal(value) => value.to_string_lossy().starts_with('.'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn analyzes_top_level_usage_and_marks_hidden_app_data() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("Documents")).unwrap();
        fs::create_dir_all(root.path().join(".models")).unwrap();
        fs::write(root.path().join("Documents/video.mkv"), vec![0; 100]).unwrap();
        fs::write(root.path().join(".models/model.bin"), vec![0; 200]).unwrap();

        let report = analyze(root.path(), 50, 10).unwrap();
        assert_eq!(report.total_size, 300);
        assert_eq!(report.large_files.len(), 2);
        assert!(report.large_files[0].app_data);
    }

    #[test]
    fn excludes_git_and_node_modules_trees() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("project/.git")).unwrap();
        fs::create_dir_all(root.path().join("project/node_modules")).unwrap();
        fs::write(root.path().join("project/.git/object"), vec![0; 100]).unwrap();
        fs::write(
            root.path().join("project/node_modules/module"),
            vec![0; 100],
        )
        .unwrap();
        fs::write(root.path().join("project/source.rs"), vec![0; 10]).unwrap();
        let report = analyze(root.path(), 1, 10).unwrap();
        assert_eq!(report.total_size, 10);
    }
}
