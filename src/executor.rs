use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use crate::model::{ActionResult, CleanupAction, CleanupItem};
use crate::models::{huggingface_hub_dir, is_valid_ollama_model_name};
use crate::uninstall::{
    Application, ApplicationSource, UninstallPreview, is_protected_package, is_valid_identifier,
    is_valid_snap_revision,
};

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("unsafe cleanup target rejected: {0}")]
    UnsafePath(PathBuf),
    #[error("symbolic-link cleanup target rejected: {0}")]
    Symlink(PathBuf),
    #[error("cleanup command is not allowed: {program} {args}")]
    UnsafeCommand { program: String, args: String },
    #[error("I/O error while processing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Locations that must never be selectable or removable via `analyze`, regardless of size and
/// regardless of the general hidden-path policy applied to personal files. Shared by the TUI's
/// pre-selection check (`is_selectable_personal_file` in `src/tui/view.rs`) and
/// `validate_personal_file`'s execution-time re-check below, so the two enforce exactly the same
/// rule and can never independently drift apart.
///
/// Checked against every component of the path relative to `home`, not just the first. A
/// first-component-only check would miss a large file inside a project's own `.git` (for
/// example `~/code/project/.git/objects/pack-....pack`) or a `.config`/`.ssh`/`.gnupg` directory
/// that happens to live somewhere other than directly under `home`; those are exactly the kind
/// of location this denylist exists to protect, so the any-component form is the safer choice.
/// This mirrors how `validate_path`'s own `.git` check already works, a few lines below.
pub fn is_denylisted_personal_file_path(relative: &Path) -> bool {
    const PROTECTED_LOCATIONS: [&str; 4] = [".ssh", ".gnupg", ".config", ".git"];
    relative.starts_with("go/pkg")
        || relative.components().any(|component| {
            matches!(component, Component::Normal(name) if PROTECTED_LOCATIONS.contains(&name.to_string_lossy().as_ref()))
        })
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String], requires_root: bool) -> std::io::Result<Output>;
}

pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, program: &str, args: &[String], requires_root: bool) -> std::io::Result<Output> {
        if requires_root && !is_effective_root() {
            Command::new("sudo")
                .arg("--")
                .arg(program)
                .args(args)
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .output()
        } else {
            Command::new(program)
                .args(args)
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .output()
        }
    }
}

pub struct Executor<R = ProcessCommandRunner> {
    home: PathBuf,
    runner: R,
}

impl Executor<ProcessCommandRunner> {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            runner: ProcessCommandRunner,
        }
    }
}

impl<R: CommandRunner> Executor<R> {
    pub fn with_runner(home: PathBuf, runner: R) -> Self {
        Self { home, runner }
    }

    pub fn execute(&self, item: &CleanupItem, dry_run: bool) -> ActionResult {
        let outcome = match &item.action {
            CleanupAction::RemovePath {
                path,
                contents_only,
            } => self
                .validate_path(path)
                .and_then(|()| self.ensure_not_symlink(path))
                .and_then(|()| {
                    if dry_run || !path.exists() {
                        Ok(())
                    } else if *contents_only {
                        remove_contents(path)
                    } else {
                        remove_entry(path)
                    }
                })
                .map_err(|error| error.to_string()),
            CleanupAction::RemovePersonalFile { path } => self
                .validate_personal_file(path)
                .and_then(|()| {
                    if dry_run || !path.exists() {
                        Ok(())
                    } else {
                        remove_entry(path)
                    }
                })
                .map_err(|error| error.to_string()),
            CleanupAction::Command {
                program,
                args,
                requires_root,
            } => self.execute_command(program, args, *requires_root, dry_run),
            CleanupAction::CommandSequence { commands } => {
                commands.iter().try_for_each(|command| {
                    self.execute_command(
                        &command.program,
                        &command.args,
                        command.requires_root,
                        dry_run,
                    )
                })
            }
        };

        ActionResult {
            item_id: item.id.clone(),
            label: item.label.clone(),
            success: outcome.is_ok(),
            dry_run,
            estimated_bytes: item.estimated_bytes,
            message: outcome
                .map(|()| "completed".into())
                .unwrap_or_else(|message| message),
        }
    }

    pub fn preview_uninstall(&self, application: &Application) -> Result<UninstallPreview, String> {
        validate_application(application)?;
        let package = application.package.clone();
        let (program, args, expected_success) = match application.source {
            ApplicationSource::Pacman => (
                "pacman",
                vec![
                    "-Rs".into(),
                    "--print".into(),
                    "--print-format".into(),
                    "%n\t%v\t%s".into(),
                    "--".into(),
                    package.clone(),
                ],
                true,
            ),
            ApplicationSource::Apt => (
                "apt-get",
                vec!["--simulate".into(), "remove".into(), package.clone()],
                true,
            ),
            ApplicationSource::Dnf => (
                "dnf",
                vec!["--assumeno".into(), "remove".into(), package.clone()],
                false,
            ),
            ApplicationSource::FlatpakUser | ApplicationSource::FlatpakSystem => {
                return Ok(UninstallPreview {
                    application_id: application.id.clone(),
                    command: uninstall_command_display(application),
                    removals: vec![format!("{} ({})", application.package, application.version)],
                    raw: "Flatpak application data is preserved by default.".into(),
                    preserves_user_data: true,
                });
            }
        };
        let output = self
            .runner
            .run(program, &args, false)
            .map_err(|error| format!("failed to preview {}: {error}", application.id))?;
        let raw = combined_output(&output);
        if expected_success && !output.status.success() {
            return Err(format!(
                "preview command exited with {}: {}",
                output.status,
                raw.trim()
            ));
        }
        if !expected_success && !output.status.success() && !raw.contains(&package) {
            return Err(format!(
                "DNF could not produce a removal plan for {}: {}",
                application.id,
                raw.trim()
            ));
        }
        let removals = parse_uninstall_removals(application.source, &raw);
        if removals.is_empty() {
            return Err(format!(
                "the package manager returned an empty removal plan for {}",
                application.id
            ));
        }
        Ok(UninstallPreview {
            application_id: application.id.clone(),
            command: uninstall_command_display(application),
            removals,
            raw,
            preserves_user_data: true,
        })
    }

    pub fn execute_uninstall(&self, application: &Application, dry_run: bool) -> ActionResult {
        let outcome = validate_application(application).and_then(|()| {
            let (program, args, requires_root) = uninstall_command(application);
            self.execute_command(program, &args, requires_root, dry_run)
        });
        ActionResult {
            item_id: application.id.clone(),
            label: format!("Uninstall {} ({})", application.name, application.id),
            success: outcome.is_ok(),
            dry_run,
            estimated_bytes: application.installed_bytes,
            message: outcome
                .map(|()| "completed; user data preserved".into())
                .unwrap_or_else(|message| message),
        }
    }

    pub fn validate_path(&self, path: &Path) -> Result<(), ExecutionError> {
        if !path.is_absolute()
            || path == Path::new("/")
            || path == self.home
            || !path.starts_with(&self.home)
            || path
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        let relative = path
            .strip_prefix(&self.home)
            .map_err(|_| ExecutionError::UnsafePath(path.to_path_buf()))?;
        if relative.components().any(
            |component| matches!(component, Component::Normal(name) if name == OsStr::new(".git")),
        ) {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        let protected = [".config", ".ssh", ".gnupg"];
        if relative
            .components()
            .next()
            .is_some_and(|component| matches!(component, Component::Normal(name) if protected.contains(&name.to_string_lossy().as_ref())))
        {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }

        let normalized = relative.to_string_lossy();
        let known = [
            ".cache/yay",
            ".cache/paru",
            ".cache/thumbnails",
            ".cache/mozilla",
            ".cache/chromium",
            ".cache/google-chrome",
            ".cache/microsoft-edge",
            ".cache/BraveSoftware",
            ".cache/vivaldi",
            ".cache/opera",
            ".local/share/Trash",
            ".cache/pip",
            ".npm/_cacache",
            ".cache/pnpm",
            ".cache/yarn",
            ".cargo/registry/cache",
            ".cargo/registry/src",
            ".cargo/git",
            "go/pkg/mod",
            ".gradle/caches",
            ".cache/composer",
            ".cache/uv",
            ".pnpm-store",
            ".cache/terragrunt",
            ".terraform.d/plugin-cache",
            ".cache/ms-playwright",
            ".cache/.bun",
            ".cache/zig",
            ".cache/pre-commit",
            ".cache/go-build",
            ".cache/ms-playwright-go",
        ];
        let artifact = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                matches!(
                    name,
                    "node_modules"
                        | "target"
                        | ".build"
                        | "build"
                        | "dist"
                        | ".venv"
                        | ".terraform"
                        | ".terragrunt-cache"
                        | ".flatpak-builder"
                        | ".pytest_cache"
                        | ".ruff_cache"
                        | ".mypy_cache"
                        | ".tox"
                        | ".next"
                        | ".turbo"
                        | ".parcel-cache"
                )
            });
        // Hugging Face cache repositories live directly under the hub
        // directory with dynamic, but predictably prefixed, names. The
        // parent must match exactly (already constrained to be inside
        // `self.home` by the checks above) and the child name must start
        // with one of the three documented repo-type prefixes.
        let huggingface_repo = path.parent() == Some(huggingface_hub_dir(&self.home).as_path())
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name.starts_with("models--")
                        || name.starts_with("datasets--")
                        || name.starts_with("spaces--")
                });
        if !known.contains(&normalized.as_ref()) && !artifact && !huggingface_repo {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        self.ensure_no_symlink_ancestors(path)?;
        Ok(())
    }

    pub fn validate_personal_file(&self, path: &Path) -> Result<(), ExecutionError> {
        if !path.is_absolute()
            || path == Path::new("/")
            || path == self.home
            || !path.starts_with(&self.home)
            || path
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        let relative = path
            .strip_prefix(&self.home)
            .map_err(|_| ExecutionError::UnsafePath(path.to_path_buf()))?;
        if is_denylisted_personal_file_path(relative) {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        self.ensure_no_symlink_ancestors(path)?;
        let metadata = fs::symlink_metadata(path).map_err(|source| ExecutionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        Ok(())
    }

    fn execute_command(
        &self,
        program: &str,
        args: &[String],
        requires_root: bool,
        dry_run: bool,
    ) -> Result<(), String> {
        if !is_allowed_command(program, args, requires_root) {
            return Err(ExecutionError::UnsafeCommand {
                program: program.into(),
                args: args.join(" "),
            }
            .to_string());
        }
        if dry_run {
            return Ok(());
        }
        let output = self
            .runner
            .run(program, args, requires_root)
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        Err(if detail.is_empty() {
            format!("command exited with {}", output.status)
        } else {
            detail.to_owned()
        })
    }

    fn ensure_no_symlink_ancestors(&self, path: &Path) -> Result<(), ExecutionError> {
        let relative = path
            .strip_prefix(&self.home)
            .map_err(|_| ExecutionError::UnsafePath(path.to_path_buf()))?;
        let mut current = self.home.clone();
        for component in relative.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(ExecutionError::Symlink(current));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(source) => {
                    return Err(ExecutionError::Io {
                        path: current,
                        source,
                    });
                }
            }
        }
        Ok(())
    }

    fn ensure_not_symlink(&self, path: &Path) -> Result<(), ExecutionError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(ExecutionError::Symlink(path.to_path_buf()))
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ExecutionError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

fn remove_contents(path: &Path) -> Result<(), ExecutionError> {
    let entries = fs::read_dir(path).map_err(|source| ExecutionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ExecutionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        remove_entry(&entry.path())?;
    }
    Ok(())
}

fn remove_entry(path: &Path) -> Result<(), ExecutionError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ExecutionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|source| ExecutionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn is_allowed_command(program: &str, args: &[String], requires_root: bool) -> bool {
    let values: Vec<&str> = args.iter().map(String::as_str).collect();
    if matches!(
        (program, values.as_slice(), requires_root),
        ("paccache", ["-rk1"], true)
            | ("paccache", ["-ruk0"], true)
            | ("apt-get", ["clean"], true)
            | ("dnf", ["clean", "all"], true)
            | ("journalctl", ["--vacuum-size=200M"], true)
            | ("docker", ["system", "prune", "-f"], false)
            | ("flatpak", ["uninstall", "--unused", "-y"], false)
            | ("podman", ["system", "prune", "-f"], false)
            | ("podman", ["system", "prune", "-f"], true)
    ) {
        return true;
    }
    match (program, values.as_slice(), requires_root) {
        ("pacman", ["-Rns", "--noconfirm", "--", package], true)
        | ("apt-get", ["--yes", "remove", package], true)
        | ("dnf", ["--assumeyes", "remove", package], true) => {
            is_valid_identifier(package) && !is_protected_package(package)
        }
        ("flatpak", ["uninstall", "--user", "-y", application], false)
        | ("flatpak", ["uninstall", "--system", "-y", application], false) => {
            is_valid_identifier(application)
        }
        ("podman", ["--connection", name, "system", "prune", "-f"], false) => {
            is_valid_identifier(name)
        }
        ("ollama", ["rm", name], false) => is_valid_ollama_model_name(name),
        ("snap", ["remove", revision_flag, name], true) => {
            revision_flag
                .strip_prefix("--revision=")
                .is_some_and(is_valid_snap_revision)
                && is_valid_identifier(name)
        }
        _ => false,
    }
}

fn validate_application(application: &Application) -> Result<(), String> {
    if !is_valid_identifier(&application.package)
        || application.id != Application::new_id(application.source, &application.package)
        || (!application.source.is_flatpak() && is_protected_package(&application.package))
    {
        return Err(format!(
            "unsafe or protected application identifier rejected: {}",
            application.id
        ));
    }
    Ok(())
}

fn uninstall_command(application: &Application) -> (&'static str, Vec<String>, bool) {
    let package = application.package.clone();
    match application.source {
        ApplicationSource::Pacman => (
            "pacman",
            vec!["-Rns".into(), "--noconfirm".into(), "--".into(), package],
            true,
        ),
        ApplicationSource::Apt => (
            "apt-get",
            vec!["--yes".into(), "remove".into(), package],
            true,
        ),
        ApplicationSource::Dnf => (
            "dnf",
            vec!["--assumeyes".into(), "remove".into(), package],
            true,
        ),
        ApplicationSource::FlatpakUser => (
            "flatpak",
            vec!["uninstall".into(), "--user".into(), "-y".into(), package],
            false,
        ),
        ApplicationSource::FlatpakSystem => (
            "flatpak",
            vec!["uninstall".into(), "--system".into(), "-y".into(), package],
            false,
        ),
    }
}

fn uninstall_command_display(application: &Application) -> String {
    let (program, args, requires_root) = uninstall_command(application);
    let prefix = if requires_root { "sudo -- " } else { "" };
    format!("{prefix}{program} {}", args.join(" "))
}

fn combined_output(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    text
}

fn parse_uninstall_removals(source: ApplicationSource, raw: &str) -> Vec<String> {
    match source {
        ApplicationSource::Pacman => raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                if fields.len() >= 3 {
                    let size = fields[2]
                        .parse::<u64>()
                        .map(crate::size::format_bytes)
                        .unwrap_or_else(|_| fields[2].into());
                    format!("{} {} ({size})", fields[0], fields[1])
                } else {
                    line.trim().to_owned()
                }
            })
            .collect(),
        ApplicationSource::Apt => raw
            .lines()
            .filter_map(|line| line.strip_prefix("Remv "))
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_owned)
            .collect(),
        ApplicationSource::Dnf => raw
            .lines()
            .map(str::trim)
            .filter(|line| {
                !line.is_empty()
                    && !line.starts_with("Dependencies resolved")
                    && !line.starts_with("Transaction Summary")
                    && !line.starts_with("Operation aborted")
            })
            .take(80)
            .map(str::to_owned)
            .collect(),
        ApplicationSource::FlatpakUser | ApplicationSource::FlatpakSystem => Vec::new(),
    }
}

fn is_effective_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .and_then(|line| line.split_whitespace().nth(2))
                .and_then(|value| value.parse::<u32>().ok())
        })
        == Some(0)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::model::{CleanupGroup, Risk};
    use tempfile::tempdir;

    struct SuccessfulRunner;

    impl CommandRunner for SuccessfulRunner {
        fn run(&self, _: &str, _: &[String], _: bool) -> std::io::Result<Output> {
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn item(path: PathBuf) -> CleanupItem {
        CleanupItem {
            id: "test".into(),
            group: CleanupGroup::Dev,
            label: "test".into(),
            estimated_bytes: 1,
            risk: Risk::Explicit,
            action: CleanupAction::RemovePath {
                path,
                contents_only: false,
            },
        }
    }

    #[test]
    fn rejects_home_root_and_git_paths() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home.clone(), SuccessfulRunner);
        assert!(executor.validate_path(&home).is_err());
        assert!(executor.validate_path(Path::new("/")).is_err());
        assert!(
            executor
                .validate_path(Path::new("/home/tester/project/.git/target"))
                .is_err()
        );
    }

    #[test]
    fn refuses_unknown_paths_even_inside_home() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        assert!(
            executor
                .validate_path(Path::new("/home/tester/Documents"))
                .is_err()
        );
    }

    #[test]
    fn accepts_a_correctly_named_huggingface_repo_under_the_hub_directory() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        assert!(
            executor
                .validate_path(Path::new(
                    "/home/tester/.cache/huggingface/hub/models--fake--demo"
                ))
                .is_ok()
        );
        assert!(
            executor
                .validate_path(Path::new(
                    "/home/tester/.cache/huggingface/hub/datasets--fake--demo"
                ))
                .is_ok()
        );
    }

    #[test]
    fn rejects_a_non_matching_sibling_under_the_huggingface_hub_directory() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.cache/huggingface/hub/version.txt"))
                .is_err()
        );
    }

    #[test]
    fn rejects_a_correctly_named_huggingface_repo_under_the_wrong_parent() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        assert!(
            executor
                .validate_path(Path::new("/home/tester/models--fake--demo"))
                .is_err()
        );
    }

    #[test]
    fn refuses_a_near_miss_sibling_of_a_known_cache_path() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.cache/uv-evil"))
                .is_err()
        );
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.cache/uv"))
                .is_ok()
        );
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.pnpm-store"))
                .is_ok()
        );
    }

    #[test]
    fn refuses_a_symlinked_known_cache_path() {
        let root = tempdir().unwrap();
        let real_target = root.path().join("elsewhere");
        fs::create_dir_all(&real_target).unwrap();
        let cache_dir = root.path().join(".cache");
        fs::create_dir_all(&cache_dir).unwrap();
        let uv_link = cache_dir.join("uv");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_target, &uv_link).unwrap();

        let executor = Executor::with_runner(root.path().to_path_buf(), SuccessfulRunner);
        let result = executor.execute(&item(uv_link.clone()), false);
        assert!(!result.success);
        assert!(real_target.exists());
    }

    #[test]
    fn accepts_the_new_dev_and_browser_cache_paths() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        for accepted in [
            "/home/tester/.cache/terragrunt",
            "/home/tester/.terraform.d/plugin-cache",
            "/home/tester/.cache/ms-playwright",
            "/home/tester/.cache/.bun",
            "/home/tester/.cache/zig",
            "/home/tester/.cache/pre-commit",
            "/home/tester/.cache/go-build",
            "/home/tester/.cache/ms-playwright-go",
            "/home/tester/.cache/microsoft-edge",
        ] {
            assert!(
                executor.validate_path(Path::new(accepted)).is_ok(),
                "expected {accepted} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_terraform_d_itself_and_near_miss_siblings() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        // The whole .terraform.d directory also holds credentials and CLI
        // configuration, so only the plugin-cache subdirectory is allowed.
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.terraform.d"))
                .is_err()
        );
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.terraform.d/credentials.tfrc.json"))
                .is_err()
        );
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.cache/zig-evil"))
                .is_err()
        );
        assert!(
            executor
                .validate_path(Path::new("/home/tester/.cache/pre-commit-evil"))
                .is_err()
        );
    }

    #[test]
    fn accepts_the_new_project_artifact_names() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        for accepted in [
            "/home/tester/project/.pytest_cache",
            "/home/tester/project/.ruff_cache",
            "/home/tester/project/.mypy_cache",
            "/home/tester/project/.tox",
            "/home/tester/project/.next",
            "/home/tester/project/.turbo",
            "/home/tester/project/.parcel-cache",
        ] {
            assert!(
                executor.validate_path(Path::new(accepted)).is_ok(),
                "expected {accepted} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_explicitly_omitted_artifact_names() {
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home, SuccessfulRunner);
        // These were deliberately excluded from ARTIFACTS: __pycache__ nests
        // at every package level, vendor/ is frequently version-controlled,
        // and .direnv can hold expensive-to-rebuild or Nix profile state.
        for rejected in [
            "/home/tester/project/pkg/__pycache__",
            "/home/tester/project/vendor",
            "/home/tester/project/.direnv",
        ] {
            assert!(
                executor.validate_path(Path::new(rejected)).is_err(),
                "expected {rejected} to be rejected"
            );
        }
    }

    #[test]
    fn artifact_filename_match_is_not_depth_aware() {
        // Documents current behavior: the artifact filename check in
        // validate_path only matches on the final path component, so a
        // directory named .terraform is accepted whether it sits directly
        // under $HOME or nested inside a project. This is a pre-existing
        // property of the name-based check (shared with node_modules,
        // target, etc.), not something introduced by this change.
        let home = PathBuf::from("/home/tester");
        let executor = Executor::with_runner(home.clone(), SuccessfulRunner);
        assert!(executor.validate_path(&home.join(".terraform")).is_ok());
        assert!(
            executor
                .validate_path(&home.join("project/nested/.terraform"))
                .is_ok()
        );
    }

    #[test]
    fn removes_exact_build_artifact_after_validation() {
        let root = tempdir().unwrap();
        let target = root.path().join("project/target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("binary"), b"data").unwrap();
        let executor = Executor::with_runner(root.path().to_path_buf(), SuccessfulRunner);
        let result = executor.execute(&item(target.clone()), false);
        assert!(result.success, "{}", result.message);
        assert!(!target.exists());
    }

    #[test]
    fn dry_run_preserves_target() {
        let root = tempdir().unwrap();
        let target = root.path().join("project/target");
        fs::create_dir_all(&target).unwrap();
        let executor = Executor::with_runner(root.path().to_path_buf(), SuccessfulRunner);
        assert!(executor.execute(&item(target.clone()), true).success);
        assert!(target.exists());
    }

    #[test]
    fn removes_an_exact_personal_file() {
        let root = tempdir().unwrap();
        let file = root.path().join("Downloads/archive.iso");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"data").unwrap();
        let executor = Executor::with_runner(root.path().to_path_buf(), SuccessfulRunner);
        let item = CleanupItem {
            id: "large-file:test".into(),
            group: CleanupGroup::User,
            label: "archive.iso".into(),
            estimated_bytes: 4,
            risk: Risk::Explicit,
            action: CleanupAction::RemovePersonalFile { path: file.clone() },
        };

        let result = executor.execute(&item, false);
        assert!(result.success, "{}", result.message);
        assert!(!file.exists());
    }

    #[test]
    fn accepts_hidden_application_data_as_a_personal_file() {
        // The owner of this fork has explicitly relaxed the hidden-data half of the "large
        // personal files and hidden application data are reported only" invariant: hidden
        // directories such as `.ollama` or `.local/share/containers` are now selectable and
        // removable, as long as they are not one of the specifically protected locations below.
        let root = tempdir().unwrap();
        let file = root.path().join(".models/model.bin");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"data").unwrap();
        let executor = Executor::with_runner(root.path().to_path_buf(), SuccessfulRunner);

        assert!(executor.validate_personal_file(&file).is_ok());
        assert!(file.exists());
    }

    #[test]
    fn rejects_protected_personal_file_locations_at_any_size() {
        // `.ssh`, `.gnupg`, `.config`, and `.git` are NOT covered by the hidden-data relaxation:
        // a separate CLAUDE.md invariant ("Never delete ... .git, .config, .ssh, .gnupg")
        // protects them unconditionally, so they must stay rejected even at a single byte.
        let root = tempdir().unwrap();
        let executor = Executor::with_runner(root.path().to_path_buf(), SuccessfulRunner);
        for relative in [
            ".ssh/id_rsa_backup",
            ".gnupg/secring.gpg",
            ".config/app/state.bin",
            ".git/objects/pack/big.pack",
            // Nested occurrences must also be rejected, not just directly under home, which is
            // exactly why the denylist checks every path component instead of only the first.
            "code/project/.git/objects/pack/big.pack",
            "backups/.ssh/id_rsa",
        ] {
            let file = root.path().join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, b"x").unwrap();
            assert!(
                executor.validate_personal_file(&file).is_err(),
                "expected {relative} to be rejected"
            );
            assert!(file.exists(), "{relative} should not have been removed");
        }
    }

    fn application(package: &str) -> Application {
        Application {
            id: Application::new_id(ApplicationSource::Pacman, package),
            name: package.into(),
            package: package.into(),
            version: "1.0".into(),
            source: ApplicationSource::Pacman,
            installed_bytes: 100,
            user_data_bytes: 0,
            desktop_file: None,
            user_data_paths: Vec::new(),
        }
    }

    type RecordedCalls = Arc<Mutex<Vec<(String, Vec<String>, bool)>>>;

    struct RecordingRunner {
        calls: RecordedCalls,
    }

    impl CommandRunner for RecordingRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            requires_root: bool,
        ) -> std::io::Result<Output> {
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec(), requires_root));
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"firefox\t1.0\t100\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn uninstall_uses_an_exact_source_specific_command() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );
        let result = executor.execute_uninstall(&application("firefox"), false);
        assert!(result.success, "{}", result.message);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [(
                "pacman".into(),
                vec![
                    "-Rns".into(),
                    "--noconfirm".into(),
                    "--".into(),
                    "firefox".into()
                ],
                true,
            )]
        );
    }

    fn ollama_rm_item(name: &str) -> CleanupItem {
        CleanupItem {
            id: format!("models.ollama.{name}"),
            group: CleanupGroup::Models,
            label: format!("Ollama model {name}"),
            estimated_bytes: 1,
            risk: Risk::Elevated,
            action: CleanupAction::Command {
                program: "ollama".into(),
                args: vec!["rm".into(), name.into()],
                requires_root: false,
            },
        }
    }

    #[test]
    fn ollama_rm_is_invoked_with_exact_args() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );
        let result = executor.execute(&ollama_rm_item("qwen3-coder:latest"), false);
        assert!(result.success, "{}", result.message);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [(
                "ollama".into(),
                vec!["rm".into(), "qwen3-coder:latest".into()],
                false,
            )]
        );
    }

    #[test]
    fn ollama_rm_refuses_a_malicious_model_name() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );
        let semicolon = executor.execute(&ollama_rm_item("model; rm -rf /"), false);
        assert!(!semicolon.success);
        let leading_dash = executor.execute(&ollama_rm_item("-rf"), false);
        assert!(!leading_dash.success);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn uninstall_preview_reports_package_manager_impact() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor =
            Executor::with_runner(PathBuf::from("/home/tester"), RecordingRunner { calls });
        let preview = executor.preview_uninstall(&application("firefox")).unwrap();
        assert_eq!(preview.removals, ["firefox 1.0 (100 B)"]);
        assert!(preview.preserves_user_data);
    }

    #[test]
    fn protected_packages_never_reach_the_command_runner() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );
        let result = executor.execute_uninstall(&application("systemd"), false);
        assert!(!result.success);
        assert!(calls.lock().unwrap().is_empty());
    }

    fn snap_item(name: &str, revision: &str) -> CleanupItem {
        CleanupItem {
            id: format!("packages.snap.{name}.{revision}"),
            group: CleanupGroup::System,
            label: format!("Disabled snap revision {name} r{revision}"),
            estimated_bytes: 1_000,
            risk: Risk::Elevated,
            action: CleanupAction::Command {
                program: "snap".into(),
                args: vec![
                    "remove".into(),
                    format!("--revision={revision}"),
                    name.into(),
                ],
                requires_root: true,
            },
        }
    }

    #[test]
    fn snap_revision_removal_uses_the_exact_command_shape_and_requires_root() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );
        let result = executor.execute(&snap_item("firefox", "8664"), false);
        assert!(result.success, "{}", result.message);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [(
                "snap".into(),
                vec!["remove".into(), "--revision=8664".into(), "firefox".into()],
                true,
            )]
        );
    }

    #[test]
    fn snap_removal_rejects_names_with_shell_metacharacters() {
        assert!(!is_allowed_command(
            "snap",
            &[
                "remove".into(),
                "--revision=8664".into(),
                "firefox; rm -rf ~".into()
            ],
            true
        ));
    }

    #[test]
    fn snap_removal_rejects_names_with_a_leading_dash() {
        assert!(!is_allowed_command(
            "snap",
            &["remove".into(), "--revision=8664".into(), "-firefox".into()],
            true
        ));
    }

    #[test]
    fn snap_removal_rejects_non_numeric_revisions() {
        assert!(!is_allowed_command(
            "snap",
            &["remove".into(), "--revision=x1".into(), "firefox".into()],
            true
        ));
        assert!(!is_allowed_command(
            "snap",
            &["remove".into(), "8664".into(), "firefox".into()],
            true
        ));
    }

    #[test]
    fn snap_removal_without_root_is_rejected() {
        assert!(!is_allowed_command(
            "snap",
            &["remove".into(), "--revision=8664".into(), "firefox".into()],
            false
        ));
    }
}
