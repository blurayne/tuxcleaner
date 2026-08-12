use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use crate::model::{ActionResult, CleanupAction, CleanupItem};

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
                .output()
        } else {
            Command::new(program).args(args).output()
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
        ];
        let artifact = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                matches!(
                    name,
                    "node_modules" | "target" | ".build" | "build" | "dist" | ".venv"
                )
            });
        if !known.contains(&normalized.as_ref()) && !artifact {
            return Err(ExecutionError::UnsafePath(path.to_path_buf()));
        }
        self.ensure_no_symlink_ancestors(path)?;
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
    matches!(
        (program, values.as_slice(), requires_root),
        ("paccache", ["-rk1"], true)
            | ("paccache", ["-ruk0"], true)
            | ("apt-get", ["clean"], true)
            | ("dnf", ["clean", "all"], true)
            | ("journalctl", ["--vacuum-size=200M"], true)
            | ("docker", ["system", "prune", "-f"], false)
            | ("flatpak", ["uninstall", "--unused", "-y"], false)
    )
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
}
