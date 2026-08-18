//! Container tooling discovery (Docker, Podman, Flatpak "unused" cleanup).
//!
//! Discovery here mirrors the pattern already established for application
//! discovery in `uninstall.rs`: read-only listing commands are invoked
//! directly through the `CommandRunner` seam so tests can substitute a fake
//! runner, while the resulting `CleanupItem`s only ever embed validated,
//! allowlisted arguments (see `executor::is_allowed_command`).

use serde_json::Value;

use crate::executor::{CommandRunner, ProcessCommandRunner};
use crate::model::{CleanupAction, CleanupGroup, CleanupItem, Risk};
use crate::scanner::command_exists;
use crate::uninstall::is_valid_identifier;

/// Adds Docker, Podman, and Flatpak "unused" cleanup items to `items`.
///
/// Each tool is only offered when its binary is present on `PATH`. Podman
/// connection discovery failures never abort the scan; they are reported as
/// warnings in `warnings` and the two base Podman items are still emitted.
pub fn scan(items: &mut Vec<CleanupItem>, warnings: &mut Vec<String>) {
    scan_with_runner(&ProcessCommandRunner, items, warnings);
}

fn scan_with_runner<R: CommandRunner>(
    runner: &R,
    items: &mut Vec<CleanupItem>,
    warnings: &mut Vec<String>,
) {
    if command_exists("docker") {
        items.push(docker_item());
    }
    if command_exists("podman") {
        items.push(podman_rootless_item());
        items.push(podman_rootful_item());
        for name in discover_podman_connections(runner, warnings) {
            items.push(podman_connection_item(&name));
        }
    }
    if command_exists("flatpak") {
        items.push(flatpak_item());
    }
}

fn docker_item() -> CleanupItem {
    CleanupItem {
        id: "containers.docker".into(),
        group: CleanupGroup::Containers,
        label: "Stopped containers, dangling images, networks, and build cache (never volumes)"
            .into(),
        estimated_bytes: 0,
        risk: Risk::Elevated,
        action: CleanupAction::Command {
            program: "docker".into(),
            args: vec!["system".into(), "prune".into(), "-f".into()],
            requires_root: false,
        },
    }
}

fn flatpak_item() -> CleanupItem {
    CleanupItem {
        id: "containers.flatpak".into(),
        group: CleanupGroup::Containers,
        label: "Unused Flatpak runtimes".into(),
        estimated_bytes: 0,
        risk: Risk::Elevated,
        action: CleanupAction::Command {
            program: "flatpak".into(),
            args: vec!["uninstall".into(), "--unused".into(), "-y".into()],
            requires_root: false,
        },
    }
}

fn podman_rootless_item() -> CleanupItem {
    CleanupItem {
        id: "containers.podman.rootless".into(),
        group: CleanupGroup::Containers,
        label: "Podman (rootless): stopped containers, dangling images, networks, and build cache (never volumes)".into(),
        estimated_bytes: 0,
        risk: Risk::Elevated,
        action: CleanupAction::Command {
            program: "podman".into(),
            args: vec!["system".into(), "prune".into(), "-f".into()],
            requires_root: false,
        },
    }
}

fn podman_rootful_item() -> CleanupItem {
    CleanupItem {
        id: "containers.podman.rootful".into(),
        group: CleanupGroup::Containers,
        label: "Podman (root): stopped containers, dangling images, networks, and build cache (never volumes)".into(),
        estimated_bytes: 0,
        risk: Risk::Elevated,
        action: CleanupAction::Command {
            program: "podman".into(),
            args: vec!["system".into(), "prune".into(), "-f".into()],
            requires_root: true,
        },
    }
}

fn podman_connection_item(name: &str) -> CleanupItem {
    CleanupItem {
        id: format!("containers.podman.connection.{name}"),
        group: CleanupGroup::Containers,
        label: format!(
            "Podman connection \"{name}\": stopped containers, dangling images, networks, and build cache (never volumes)"
        ),
        estimated_bytes: 0,
        risk: Risk::Elevated,
        action: CleanupAction::Command {
            program: "podman".into(),
            args: vec![
                "--connection".into(),
                name.into(),
                "system".into(),
                "prune".into(),
                "-f".into(),
            ],
            requires_root: false,
        },
    }
}

/// Lists Podman connection names via `podman system connection list --format
/// json`, always as the rootless (non-root) invocation. Parsing is
/// deliberately tolerant: a malformed individual entry is skipped with a
/// warning, and if the listing command fails entirely (missing config,
/// unsupported Podman version, non-zero exit, spawn failure) this returns an
/// empty list so the caller can still emit the two base Podman items.
fn discover_podman_connections<R: CommandRunner>(
    runner: &R,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let args = vec![
        "system".into(),
        "connection".into(),
        "list".into(),
        "--format".into(),
        "json".into(),
    ];
    let output = match runner.run("podman", &args, false) {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries: Vec<Value> = match serde_json::from_str(&stdout) {
        Ok(entries) => entries,
        Err(_) => {
            warnings.push(
                "failed to parse Podman connection list output; skipping connection-scoped cleanup items"
                    .into(),
            );
            return Vec::new();
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        match entry.get("Name").and_then(Value::as_str) {
            Some(name) if is_valid_identifier(name) => names.push(name.to_owned()),
            _ => warnings.push(format!(
                "skipped a malformed Podman connection entry while scanning: {entry}"
            )),
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::Output;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::executor::Executor;

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
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// A runner whose `podman system connection list` reply is scripted;
    /// every other invocation records the call and succeeds trivially.
    struct ScriptedConnectionListRunner {
        calls: RecordedCalls,
        response: Result<(bool, String), ()>,
    }

    impl CommandRunner for ScriptedConnectionListRunner {
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
            if program == "podman"
                && args.first().map(String::as_str) == Some("system")
                && args.get(1).map(String::as_str) == Some("connection")
            {
                return match &self.response {
                    Ok((success, stdout)) => Ok(Output {
                        status: std::process::ExitStatus::from_raw(if *success { 0 } else { 1 }),
                        stdout: stdout.clone().into_bytes(),
                        stderr: Vec::new(),
                    }),
                    Err(()) => Err(std::io::Error::other("podman not found")),
                };
            }
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn podman_rootless_and_rootful_produce_the_exact_prune_command_shape() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );

        let rootless = executor.execute(&podman_rootless_item(), false);
        assert!(rootless.success, "{}", rootless.message);
        let rootful = executor.execute(&podman_rootful_item(), false);
        assert!(rootful.success, "{}", rootful.message);

        // `requires_root: true` is exactly what ProcessCommandRunner uses to
        // decide whether to wrap the invocation in `sudo --` (see
        // executor::ProcessCommandRunner::run), so asserting it is passed
        // through unchanged for the rootful item verifies the escalation
        // path without needing a real sudo binary in tests.
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                (
                    "podman".to_string(),
                    vec!["system".into(), "prune".into(), "-f".into()],
                    false,
                ),
                (
                    "podman".to_string(),
                    vec!["system".into(), "prune".into(), "-f".into()],
                    true,
                ),
            ]
        );
    }

    #[test]
    fn podman_connection_item_produces_the_exact_connection_scoped_command_shape() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );

        let result = executor.execute(&podman_connection_item("staging"), false);
        assert!(result.success, "{}", result.message);

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [(
                "podman".to_string(),
                vec![
                    "--connection".into(),
                    "staging".into(),
                    "system".into(),
                    "prune".into(),
                    "-f".into(),
                ],
                false,
            )]
        );
    }

    #[test]
    fn refuses_connection_names_containing_shell_metacharacters() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );

        let result = executor.execute(&podman_connection_item("staging; rm -rf ~"), false);
        assert!(!result.success);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn refuses_connection_names_with_a_leading_dash() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_runner(
            PathBuf::from("/home/tester"),
            RecordingRunner {
                calls: calls.clone(),
            },
        );

        let result = executor.execute(&podman_connection_item("-x"), false);
        assert!(!result.success);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn discovers_connections_and_skips_malformed_entries_with_a_warning() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = ScriptedConnectionListRunner {
            calls: calls.clone(),
            response: Ok((
                true,
                r#"[{"Name":"staging"},{"NoName":"oops"},{"Name":"-bad"}]"#.to_string(),
            )),
        };
        let mut warnings = Vec::new();
        let names = discover_podman_connections(&runner, &mut warnings);

        assert_eq!(names, ["staging".to_string()]);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn gracefully_degrades_when_connection_listing_fails() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = ScriptedConnectionListRunner {
            calls: calls.clone(),
            response: Ok((false, String::new())),
        };
        let mut items = Vec::new();
        let mut warnings = Vec::new();

        // Simulate the caller's gate: exercise the same code path
        // `scan_with_runner` takes when `podman` is present.
        items.push(podman_rootless_item());
        items.push(podman_rootful_item());
        for name in discover_podman_connections(&runner, &mut warnings) {
            items.push(podman_connection_item(&name));
        }

        assert_eq!(items.len(), 2);
        assert!(warnings.is_empty());
        assert!(
            items
                .iter()
                .all(|item| item.id != "containers.podman.connection")
        );
    }

    #[test]
    fn gracefully_degrades_when_the_podman_binary_cannot_be_invoked() {
        let runner = ScriptedConnectionListRunner {
            calls: Arc::new(Mutex::new(Vec::new())),
            response: Err(()),
        };
        let mut warnings = Vec::new();
        let names = discover_podman_connections(&runner, &mut warnings);
        assert!(names.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn scan_with_runner_never_emits_a_volume_pruning_item() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = ScriptedConnectionListRunner {
            calls: calls.clone(),
            response: Ok((true, r#"[{"Name":"staging"}]"#.to_string())),
        };
        let mut items = Vec::new();
        let mut warnings = Vec::new();
        scan_with_runner(&runner, &mut items, &mut warnings);

        for item in &items {
            if let CleanupAction::Command { args, .. } = &item.action {
                assert!(!args.iter().any(|arg| arg == "--volumes"));
            }
        }
    }
}
