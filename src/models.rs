//! Discovery of local LLM/model-weight caches (Ollama, Hugging Face, and a
//! handful of other widely used developer AI tools).
//!
//! Discovery here is deliberately read-only and filesystem based wherever
//! possible, mirroring `src/scanner.rs`. Ollama is the one exception: its
//! on-disk blobs are reference-counted across manifests, so *deletion* must
//! go through the `ollama` CLI (`ollama rm <name>`) to avoid corrupting
//! blobs still referenced by other models. Discovery of Ollama models is
//! still filesystem based (no HTTP client, no `ollama list` table parsing).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use walkdir::WalkDir;

use crate::model::{CleanupAction, CleanupGroup, CleanupItem, Risk};
use crate::scanner::{command_exists, dir_size};

/// Static, home-relative model/weight caches for developer AI tooling other
/// than Ollama and Hugging Face. Each entry is included only when it is a
/// documented, tool-owned cache directory that holds reproducible,
/// re-downloadable model weights and cannot reasonably hold user-authored
/// content (chat history, notes, etc.).
const STATIC_MODEL_CACHES: &[(&str, &str)] = &[
    (".cache/torch/hub", "PyTorch Hub cached models"),
    (".cache/whisper", "OpenAI Whisper model cache"),
    (".lmstudio/models", "LM Studio downloaded models"),
    (".cache/gpt4all", "GPT4All (Python bindings) model cache"),
    (
        ".cache/torch/sentence_transformers",
        "sentence-transformers model cache",
    ),
    (
        ".cache/modelscope/hub",
        "ModelScope model and dataset cache",
    ),
];

#[derive(Debug, Deserialize)]
struct OllamaManifest {
    #[serde(default)]
    config: Option<OllamaManifestBlob>,
    #[serde(default)]
    layers: Vec<OllamaManifestBlob>,
}

#[derive(Debug, Deserialize)]
struct OllamaManifestBlob {
    digest: String,
}

/// Scan every supported model-cache location and append discovered items.
/// Never aborts: missing tools/directories are skipped, and unreadable
/// Ollama roots produce a warning instead of failing the whole scan.
pub fn scan(home: &Path, items: &mut Vec<CleanupItem>, warnings: &mut Vec<String>) {
    scan_static_caches(home, items);
    scan_ollama(home, items, warnings);
    scan_huggingface(home, items);
}

fn scan_static_caches(home: &Path, items: &mut Vec<CleanupItem>) {
    for (relative, label) in STATIC_MODEL_CACHES {
        let path = home.join(relative);
        let estimated_bytes = dir_size(&path);
        if estimated_bytes == 0 {
            continue;
        }
        items.push(CleanupItem {
            id: format!("models.{}", relative.replace('/', ".")),
            group: CleanupGroup::Models,
            label: (*label).into(),
            estimated_bytes,
            risk: Risk::Elevated,
            action: CleanupAction::RemovePath {
                path,
                contents_only: false,
            },
        });
    }
}

/// Resolve the Ollama models root: `$OLLAMA_MODELS` if set, else
/// `~/.ollama/models`.
pub fn ollama_models_root(home: &Path) -> PathBuf {
    env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".ollama/models"))
}

fn scan_ollama(home: &Path, items: &mut Vec<CleanupItem>, warnings: &mut Vec<String>) {
    if !command_exists("ollama") {
        return;
    }
    let root = ollama_models_root(home);
    let manifests_dir = root.join("manifests");
    let blobs_dir = root.join("blobs");

    if !manifests_dir.is_dir() {
        warnings.push(format!(
            "Ollama is installed but the models directory {} was not found; skipping Ollama model discovery",
            manifests_dir.display()
        ));
        return;
    }

    let mut had_errors = false;
    for entry in WalkDir::new(&manifests_dir).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                had_errors = true;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = match entry.path().strip_prefix(&manifests_dir) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let components: Vec<&str> = relative
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect();
        // Expected shape: manifests/<registry>/<namespace>/<model>/<tag>
        if components.len() != 4 {
            had_errors = true;
            continue;
        }
        let [registry, namespace, model, tag] =
            [components[0], components[1], components[2], components[3]];

        let Ok(content) = fs::read_to_string(entry.path()) else {
            had_errors = true;
            continue;
        };
        let manifest: OllamaManifest = match serde_json::from_str(&content) {
            Ok(manifest) => manifest,
            Err(_) => {
                had_errors = true;
                continue;
            }
        };

        let mut estimated_bytes = 0u64;
        if let Some(config) = &manifest.config {
            estimated_bytes += blob_size(&blobs_dir, &config.digest);
        }
        for layer in &manifest.layers {
            estimated_bytes += blob_size(&blobs_dir, &layer.digest);
        }

        let name = ollama_display_name(registry, namespace, model, tag);
        let id_suffix = name.replace(['/', ':'], ".");
        items.push(CleanupItem {
            id: format!("models.ollama.{id_suffix}"),
            group: CleanupGroup::Models,
            label: format!("Ollama model {name}"),
            estimated_bytes,
            risk: Risk::Elevated,
            action: CleanupAction::Command {
                program: "ollama".into(),
                args: vec!["rm".into(), name],
                requires_root: false,
            },
        });
    }

    if had_errors {
        warnings.push(format!(
            "some entries under {} could not be read or did not match the expected Ollama manifest layout",
            manifests_dir.display()
        ));
    }
}

/// Build the display/CLI name Ollama itself uses for a model, e.g.
/// `qwen3-coder:latest` (default registry, default `library` namespace),
/// `myuser/mymodel:latest` (default registry, custom namespace), or
/// `example.com/myuser/mymodel:latest` (custom registry).
fn ollama_display_name(registry: &str, namespace: &str, model: &str, tag: &str) -> String {
    const DEFAULT_REGISTRY: &str = "registry.ollama.ai";
    const DEFAULT_NAMESPACE: &str = "library";
    if registry == DEFAULT_REGISTRY {
        if namespace == DEFAULT_NAMESPACE {
            format!("{model}:{tag}")
        } else {
            format!("{namespace}/{model}:{tag}")
        }
    } else {
        format!("{registry}/{namespace}/{model}:{tag}")
    }
}

/// Look up a blob's actual on-disk size by digest (`sha256:<hex>` ->
/// `blobs/sha256-<hex>`). Never follows symlinks; missing or symlinked blobs
/// contribute zero bytes rather than failing the whole scan.
fn blob_size(blobs_dir: &Path, digest: &str) -> u64 {
    let Some(hash) = digest.strip_prefix("sha256:") else {
        return 0;
    };
    let path = blobs_dir.join(format!("sha256-{hash}"));
    fs::symlink_metadata(&path)
        .ok()
        .filter(|metadata| !metadata.file_type().is_symlink())
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

/// A dedicated, equally strict validator for Ollama model names. These
/// names look like `library/model:tag` or `registry.example.com/ns/model:tag`
/// and therefore contain `/`, which `crate::uninstall::is_valid_identifier`
/// deliberately does not allow. Rather than loosen that shared validator for
/// every other caller, this validator applies the same allowlist philosophy
/// (fixed character set, no leading dash, bounded length) plus a `/`-aware
/// path-traversal guard.
pub fn is_valid_ollama_model_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("..")
        && !value.contains("//")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+:@/-".contains(&byte))
}

/// Resolve the Hugging Face hub cache directory: `$HF_HOME/hub` if
/// `$HF_HOME` is set, else `~/.cache/huggingface/hub`.
pub fn huggingface_hub_dir(home: &Path) -> PathBuf {
    env::var_os("HF_HOME")
        .map(PathBuf::from)
        .map(|dir| dir.join("hub"))
        .unwrap_or_else(|| home.join(".cache/huggingface/hub"))
}

fn scan_huggingface(home: &Path, items: &mut Vec<CleanupItem>) {
    let hub_dir = huggingface_hub_dir(home);
    let Ok(entries) = fs::read_dir(&hub_dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // `DirEntry::file_type` does not follow symlinks, so a symlinked
        // "models--..." entry is skipped rather than treated as a real repo.
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("models--")
            || name.starts_with("datasets--")
            || name.starts_with("spaces--"))
        {
            continue;
        }
        let path = entry.path();
        let estimated_bytes = dir_size(&path);
        if estimated_bytes == 0 {
            continue;
        }
        items.push(CleanupItem {
            id: format!("models.huggingface.{name}"),
            group: CleanupGroup::Models,
            label: format!("Hugging Face cache: {name}"),
            estimated_bytes,
            risk: Risk::Elevated,
            action: CleanupAction::RemovePath {
                path,
                contents_only: false,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::*;

    // Serializes tests that mutate process-wide environment variables
    // (OLLAMA_MODELS / HF_HOME) so they cannot race each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_manifest(path: &Path, layer_digests: &[&str], config_digest: Option<&str>) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let layers: Vec<_> = layer_digests
            .iter()
            .map(|digest| format!(r#"{{"mediaType":"application/vnd.ollama.image.model","digest":"{digest}","size":0}}"#))
            .collect();
        let config = config_digest
            .map(|digest| format!(r#""config":{{"mediaType":"application/vnd.ollama.image.config","digest":"{digest}","size":0}},"#))
            .unwrap_or_default();
        let content = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json",{config}"layers":[{}]}}"#,
            layers.join(",")
        );
        fs::write(path, content).unwrap();
    }

    fn write_blob(blobs_dir: &Path, digest: &str, bytes: usize) {
        fs::create_dir_all(blobs_dir).unwrap();
        let hash = digest.strip_prefix("sha256:").unwrap();
        fs::write(blobs_dir.join(format!("sha256-{hash}")), vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn ollama_display_name_matches_ollama_conventions() {
        assert_eq!(
            ollama_display_name("registry.ollama.ai", "library", "qwen3-coder", "latest"),
            "qwen3-coder:latest"
        );
        assert_eq!(
            ollama_display_name("registry.ollama.ai", "myuser", "mymodel", "latest"),
            "myuser/mymodel:latest"
        );
        assert_eq!(
            ollama_display_name("example.com", "myuser", "mymodel", "v1"),
            "example.com/myuser/mymodel:v1"
        );
    }

    #[test]
    fn ollama_model_name_validator_is_strict() {
        assert!(is_valid_ollama_model_name("qwen3-coder:latest"));
        assert!(is_valid_ollama_model_name("myuser/mymodel:latest"));
        assert!(is_valid_ollama_model_name("example.com/myuser/mymodel:v1"));
        assert!(!is_valid_ollama_model_name(""));
        assert!(!is_valid_ollama_model_name("-rf"));
        assert!(!is_valid_ollama_model_name("model; rm -rf /"));
        assert!(!is_valid_ollama_model_name("model && evil"));
        assert!(!is_valid_ollama_model_name("../etc/passwd"));
        assert!(!is_valid_ollama_model_name("/absolute"));
        assert!(!is_valid_ollama_model_name("trailing/"));
    }

    #[test]
    fn scan_ollama_discovers_models_and_sums_blob_sizes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = tempdir().unwrap();
        unsafe {
            env::set_var("OLLAMA_MODELS", root.path());
        }

        let manifest_path = root
            .path()
            .join("manifests/registry.ollama.ai/library/qwen3-coder/latest");
        write_manifest(
            &manifest_path,
            &["sha256:model", "sha256:license"],
            Some("sha256:config"),
        );
        let blobs_dir = root.path().join("blobs");
        write_blob(&blobs_dir, "sha256:model", 1000);
        write_blob(&blobs_dir, "sha256:license", 20);
        write_blob(&blobs_dir, "sha256:config", 5);

        let mut items = Vec::new();
        let mut warnings = Vec::new();
        scan_ollama(root.path(), &mut items, &mut warnings);

        unsafe {
            env::remove_var("OLLAMA_MODELS");
        }

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, "models.ollama.qwen3-coder.latest");
        assert_eq!(item.group, CleanupGroup::Models);
        assert_eq!(item.risk, Risk::Elevated);
        assert_eq!(item.estimated_bytes, 1025);
        match &item.action {
            CleanupAction::Command {
                program,
                args,
                requires_root,
            } => {
                assert_eq!(program, "ollama");
                assert_eq!(args, &["rm".to_string(), "qwen3-coder:latest".to_string()]);
                assert!(!requires_root);
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn scan_ollama_warns_but_does_not_abort_when_root_is_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = tempdir().unwrap();
        unsafe {
            env::set_var("OLLAMA_MODELS", root.path().join("does-not-exist"));
        }

        let mut items = Vec::new();
        let mut warnings = Vec::new();
        scan_ollama(root.path(), &mut items, &mut warnings);

        unsafe {
            env::remove_var("OLLAMA_MODELS");
        }

        assert!(items.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn scan_huggingface_size_is_not_double_counted_through_symlinks() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = tempdir().unwrap();
        unsafe {
            env::remove_var("HF_HOME");
        }
        let repo = root
            .path()
            .join(".cache/huggingface/hub/models--fake--demo");
        let blobs = repo.join("blobs");
        let snapshot = repo.join("snapshots/abc123");
        fs::create_dir_all(&blobs).unwrap();
        fs::create_dir_all(&snapshot).unwrap();
        fs::write(blobs.join("blobhash"), vec![0u8; 4096]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(blobs.join("blobhash"), snapshot.join("link")).unwrap();

        let mut items = Vec::new();
        scan_huggingface(root.path(), &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].estimated_bytes, 4096);
        assert_eq!(items[0].group, CleanupGroup::Models);
        assert_eq!(items[0].risk, Risk::Elevated);
        assert!(matches!(
            &items[0].action,
            CleanupAction::RemovePath {
                contents_only: false,
                ..
            }
        ));
    }

    #[test]
    fn scan_huggingface_ignores_non_matching_children() {
        let root = tempdir().unwrap();
        let hub = root.path().join(".cache/huggingface/hub");
        fs::create_dir_all(&hub).unwrap();
        fs::write(hub.join("version.txt"), b"1").unwrap();
        let mut items = Vec::new();
        scan_huggingface(root.path(), &mut items);
        assert!(items.is_empty());
    }
}
