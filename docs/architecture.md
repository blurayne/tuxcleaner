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
| `scanner` | Known system, user, developer, Docker, and Flatpak candidates |
| `analyze` | Read-only disk aggregation and large-file reporting |
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

Package removal is intentionally outside the initial cleanup adapters. Orphan detection differs across package managers and can remove load-bearing packages. A future orphan feature must report exact package names and use a separate explicit confirmation.

## Filesystem invariants

All traversals disable symlink following. Before deletion, the executor walks every target component with `symlink_metadata` and refuses symlinks again. This second check limits both scanner mistakes and time-of-check/time-of-use surprises involving known cache paths.

Deletion uses `remove_file` and `remove_dir_all` on exact `Path` values. No user path is interpolated into a shell command.

## Machine-readable interfaces

The JSON structures are regular Serde models. Fields are additive within the `0.x` series. Automation should ignore unknown fields and must pass `--yes` to request destructive cleanup. Without `--yes`, `clean --json` returns inventory only.

The updater selects an exact target archive from GitHub release metadata, downloads its adjacent SHA-256 asset, verifies the complete archive, extracts only the `tuxcleaner` entry, and atomically replaces the current executable. `update --check` and `update --dry-run` stop before downloading or replacing the binary.
