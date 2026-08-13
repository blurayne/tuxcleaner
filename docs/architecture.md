# Architecture

## Design goals

TuxCleaner keeps scanning, policy, and execution separate. A scanner may discover a candidate, but it cannot delete it. An executor may delete an item, but only after the item passes an independent allowlist.

The split supports three goals:

- read-only scans can run without privilege
- distribution behavior stays isolated from shared cache rules
- tests can replace command execution without changing production code

## Modules

| Module | Responsibility |
| --- | --- |
| `cli` | Clap interface, confirmations, rendering, and command orchestration |
| `tui` | Ratatui interactive entry menu |
| `distro` | `os-release` parsing and package-manager actions |
| `uninstall` | Visible desktop application discovery, package ownership, and protected-package policy |
| `scanner` | Known system, user, developer, Docker, and Flatpak candidates |
| `analyze` | Read-only disk aggregation and explicit large-file candidates |
| `purge` | Discovery of old reproducible project artifacts |
| `executor` | Exact path and command validation, removal, and process execution |
| `history` | Append-only JSONL operation records |
| `status` | Read-only Linux health information from `/proc` and `df` |
| `update` | GitHub release discovery, SHA-256 verification, and atomic self-replacement |

## Adding a distribution

1. Add a `DistroFamily` variant and `os-release` identifiers.
2. Define package cache paths used only for the estimate.
3. Return a typed cleanup action with fixed arguments.
4. Add the exact command to the executor allowlist.
5. Add an `os-release` fixture and test the selected adapter.
6. Add a container smoke-test entry.

Application uninstall is separate from cleanup adapters. The catalog includes explicitly installed native packages only when they own a visible desktop entry. Flatpak applications are cataloged by installation scope. Each selection uses its exact source-qualified ID, receives a package-manager transaction preview, and passes the protected-package policy before execution. TuxCleaner never turns orphan detection into an automatic removal action.

## Application uninstall invariants

- Catalog discovery is read-only and runs without privilege.
- Native applications must be explicitly installed and own a visible desktop entry.
- Selection is empty by default and non-interactive use requires an exact `--app` ID with `--yes`.
- Native removals receive a transaction preview before confirmation.
- The executor validates the source, identifier, protected-package policy, and fixed argument shape independently.
- Flatpak installation scope is preserved in both preview and execution.
- Application configuration and user data are measured where available but never removed.

## Filesystem invariants

All traversals disable symlink following. Before deletion, the executor walks every target component with `symlink_metadata` and refuses symlinks again. This second check limits both scanner mistakes and time-of-check/time-of-use surprises involving known cache paths.

Deletion uses `remove_file` and `remove_dir_all` on exact `Path` values. No user path is interpolated into a shell command.

Large personal-file removal has a separate executor action and validator. It accepts only exact regular files in non-hidden paths under the canonical home directory. Candidates must come from the current size-threshold analysis, and hidden application data remains report-only.

## Machine-readable interfaces

The JSON structures are regular Serde models. Fields are additive within the `0.x` series. Automation should ignore unknown fields and must pass `--yes` to request destructive cleanup. Without `--yes`, `clean --json` and `uninstall --json` return inventory only. Large-file automation additionally requires one or more exact `--file` paths from the current analysis.

The updater selects an exact target archive from GitHub release metadata, downloads its adjacent SHA-256 asset, verifies the complete archive, extracts only the `tuxcleaner` entry, and atomically replaces the current executable. `update --check` and `update --dry-run` stop before downloading or replacing the binary.
