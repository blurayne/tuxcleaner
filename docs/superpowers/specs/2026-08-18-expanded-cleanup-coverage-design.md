# Expanded cleanup coverage: containers, model caches, and project artifacts

Status: draft for discussion. No source files were changed to produce this document; it is planning input only.

Docs convention note: this repository has no pre-existing `docs/specs/` or `docs/plans/` folder (only `docs/architecture.md` and `docs/demo/`). Since no conflicting convention exists, this document was placed at the path requested by the task, `docs/superpowers/specs/2026-08-18-expanded-cleanup-coverage-design.md`. If the project later adopts a different docs layout, this file should move with it.

## Context

The user asked whether TuxCleaner already detects/cleans up six categories of developer and AI-tooling disk usage, and, where it does not, wants a safe design for adding them:

1. Cache/artifact directories: `.flatpak-builder`, `.pnpm-store`, `.terragrunt-cache`, `.terraform`, `.venv`, `node_modules`
2. Flatpak (unused runtimes/apps)
3. Ollama models
4. Podman (rootful and rootless)
5. Docker (rootful, rootless, multiple contexts)
6. `uv` cache

For containers and model caches, the user also asked for a considered CLI-vs-API decision per tool, and a context/connection iteration design for Docker and Podman so each context is an individually selectable `CleanupItem`.

This document is scoped to design only. All proposed actions are typed `CleanupAction` variants executed through the existing `src/executor.rs` allowlist; nothing here proposes shell-string execution, and every destructive item remains subject to explicit selection or `--yes` plus `--dry-run`, per `CLAUDE.md`.

## Current coverage audit

Audit performed by reading `src/scanner.rs`, `src/purge.rs`, `src/model.rs`, `src/distro.rs`, `src/analyze.rs`, `src/executor.rs`, `src/uninstall.rs`, and `docs/architecture.md`, and grepping `src/` for `flatpak`, `docker`, `podman`, `ollama`, `huggingface`, `uv`, and each requested path fragment.

### 1. Cache/artifact directories

| Path | Status | Evidence |
| --- | --- | --- |
| `node_modules` | **Covered** | `src/purge.rs:11` `ARTIFACTS` list; matching allowlist entry `src/executor.rs:274`; also excluded from `analyze` traversal at `src/analyze.rs:167` (different purpose: skip during size analysis, not a deletion rule) |
| `.venv` | **Covered** | Same `ARTIFACTS` list (`src/purge.rs:11`) and executor allowlist (`src/executor.rs:274`) |
| `.pnpm-store` | **Partially covered** | `src/scanner.rs:27` covers `.cache/pnpm` (pnpm's legacy/global cache location on some setups) as a `DEV_CACHE_PATHS` entry, but the pnpm *content-addressable store* directory name `.pnpm-store` (pre-v7 default, still used when `pnpm config get store-dir` is customized to a project-relative path) is not matched anywhere. Since pnpm's store location is configurable (`pnpm store path`), a fixed relative path is inherently approximate for this one. |
| `.terragrunt-cache` | **Not covered** | No match anywhere in `src/` |
| `.terraform` | **Not covered** | No match anywhere in `src/` |
| `.flatpak-builder` | **Not covered** | No match anywhere in `src/`. Existing Flatpak support (`src/scanner.rs:187-200`, `src/uninstall.rs:448-467`) only addresses installed runtimes/apps, not the builder's local build cache (manifests, downloaded sources, build directories under a project's `.flatpak-builder/`) |

### 2. Flatpak (unused runtimes/apps)

**Covered for "unused runtimes"**: `src/scanner.rs:187-200` adds a `CleanupGroup::Containers` item that runs `flatpak uninstall --unused -y` (`Risk::Elevated`), gated by `command_exists("flatpak")` (`src/scanner.rs:171,187`), with the exact command allowlisted at `src/executor.rs:428`.

**Covered separately for "specific installed apps"**: `src/uninstall.rs:448-467` (`discover_flatpaks`) catalogs installed Flatpak apps per install scope (`FlatpakUser`/`FlatpakSystem`, `src/uninstall.rs:22-48`) for the explicit, per-application `uninstall` command — a deliberately different, individually-selected workflow (not "unused" cleanup), with its own allowlist entries at `src/executor.rs:438-441`.

No gap here beyond what is asked below for `.flatpak-builder`.

### 3. Ollama models

**Not covered.** No mention of `ollama` anywhere in `src/`.

### 4. Podman

**Not covered.** No mention of `podman` anywhere in `src/`. No rootful/rootless handling, no connection discovery.

### 5. Docker

**Partially covered.** `src/scanner.rs:171-186` adds exactly one `CleanupGroup::Containers` item running `docker system prune -f` (no `--volumes`, correctly respecting the "Docker volumes are never part of group cleanup" invariant) gated by `command_exists("docker")`. Gaps:
- No context discovery/iteration — always targets whatever `docker` resolves to by default (typically the `default` context, or `DOCKER_HOST` if set).
- No explicit rootless-context support. Docker rootless mode is commonly configured as a *named context* (e.g. `rootless`, created by `dockerd-rootless-setuptool.sh`), so context iteration (proposed below) organically covers rootless Docker as long as the user has set up a rootless context — no separate rootful/rootless flag needed for Docker specifically.

### 6. `uv` cache

**Not covered.** `DEV_CACHE_PATHS` (`src/scanner.rs:24-35`) has no `.cache/uv` entry, and no other reference to `uv` exists in `src/`.

## Proposed additions

All new scanner-discovered items reuse the existing gate pattern: `command_exists()` (`src/scanner.rs:213-223`) for CLI-driven tools, or `dir_size(path) > 0` (`src/scanner.rs:225-240`) for pure filesystem checks, mirroring `scan_known_paths` (`src/scanner.rs:126-150`). No new item is force-added to a machine where the underlying tool/directory is absent — this is the "check if the CLI/tool exists first" gate the user asked for, generalized.

### A. Simple additions to existing lists (lowest risk, smallest diff)

**`.cache/uv` (uv cache)**
- File: `src/scanner.rs`, add to `DEV_CACHE_PATHS`: `(".cache/uv", "uv package manager cache")`.
- `CleanupGroup::Dev`, `Risk::Low` (matches sibling entries `.cache/pip`, `.cache/pnpm`).
- `CleanupAction::RemovePath { path, contents_only: false }` (existing `scan_known_paths` machinery produces this automatically).
- Executor: add `.cache/uv` to the `known` array in `src/executor.rs` (~line 246), no allowlist change needed (it's a path check, not a command).
- Tests: extend `scan_only_includes_existing_known_cache_paths` in `src/scanner.rs` (or add a sibling test) asserting the new id appears when the directory is populated; add a refusal-style check in `src/executor.rs` tests confirming a path *outside* `.cache/uv` (e.g. a symlink or a sibling `.cache/uv-evil`) is still rejected by `validate_path`.

**`.terraform` and `.terragrunt-cache` (per-project artifacts)**
- File: `src/purge.rs`, add both names to the `ARTIFACTS` const (`src/purge.rs:11`).
- These are project-local, potentially large, and safe to delete only with the same age-gated, explicitly-selected `purge` workflow already used for `node_modules`/`.venv`/`target` — not a home-directory cache, so `CleanupGroup::Dev` and `Risk::Explicit` via `PurgeCandidate::cleanup_item()` (`src/purge.rs:22-36`) is correct and requires no new enum work.
- Executor: add `"terraform" | "terragrunt-cache"`-style match arms to the `artifact` filename check in `src/executor.rs:270-276` (note: directory name is literally `.terraform` and `.terragrunt-cache`, so the match values are `".terraform"` and `".terragrunt-cache"`).
- Caution: `.terraform` also contains a `.terraform.lock.hcl` *file* one level up (not inside the directory) and, more importantly, `.terraform/terraform.tfstate` is sometimes present for local backends — deleting `.terraform` is safe for the *provider plugin cache and module cache* but a user relying on local state (no remote backend configured) would lose state. Recommend a `purge`-only (never `clean`) placement specifically because `purge` already requires per-candidate explicit selection or `--yes` plus an age threshold (`older_than_days`), and add a one-line note to the CLI help / architecture doc calling out that `.terraform` may contain local state and recommending remote backends. This is a conflict worth flagging explicitly (see Safety invariant notes).
- Tests: fixture test mirroring `finds_build_artifacts_but_never_git_contents` (`src/purge.rs:97-110`) with a `project/.terraform` and `project/.terragrunt-cache` directory; a refusal test in `src/executor.rs` confirming a path named `.terraform` outside a real project root (e.g. directly under `$HOME`) is still only accepted because the filename check is name-based, not path-based — so add an explicit test that `$HOME/.terraform` is treated the same as a nested one (documenting current behavior) since the filename-only check does not distinguish depth today (pre-existing property, worth a regression test rather than a behavior change).

**`.pnpm-store` (dev cache, complements existing `.cache/pnpm`)**
- File: `src/scanner.rs`, add to `DEV_CACHE_PATHS`: `(".pnpm-store", "pnpm content-addressable store")`. This only covers the common case where the store lives directly under `$HOME`; pnpm also allows a fully custom `store-dir`, which cannot be discovered without invoking `pnpm store path` (a CLI call). Given pnpm is a lower-priority tool than Docker/Ollama here, recommend the static path first and leave dynamic discovery as an open question (see below) rather than adding a `pnpm` subprocess dependency now.
- `CleanupGroup::Dev`, `Risk::Low`, `CleanupAction::RemovePath` (same machinery as above).
- Executor + tests: same pattern as `.cache/uv` above.

**`.flatpak-builder` (Flatpak build cache)**
- This is a *per-project* directory (created wherever `flatpak-builder` is invoked, not under `$HOME` predictably), so it belongs in `src/purge.rs`'s `ARTIFACTS` list next to `node_modules`/`target`, not in `scanner.rs`'s home-relative known paths.
- `CleanupGroup::Dev`, `Risk::Explicit`, discovered via the existing `scan_artifacts` walk with no new logic beyond the name match.
- Executor: add `".flatpak-builder"` to the artifact filename match in `src/executor.rs:270-276`.
- Tests: fixture test alongside the `.terraform`/`.terragrunt-cache` ones above.

### B. Ollama models (new dedicated module)

Not a simple path or a simple prune command — Ollama's on-disk layout is content-addressed: manifests (small JSON files identifying a model+tag) reference shared blobs by SHA-256 digest under `~/.ollama/models/blobs/` (or `$OLLAMA_MODELS/blobs/` if that env var is set). Multiple models/tags can reference the same blob. Deleting a model safely means removing its manifest and only the blobs that are no longer referenced by any remaining manifest — that is exactly the "reference-counted garbage collection" logic `ollama rm <model>` already implements internally. Reimplementing that logic as raw filesystem deletion would risk corrupting or under/over-deleting other models' blobs, which is unacceptable for a safety-first tool.

Design:
- New module `src/models.rs` (or a `models` sub-scope inside a broader module if the maintainers prefer fewer files — see Open questions) with a `discover_ollama_models()` function.
- Existence gate: `command_exists("ollama")` (reusing `crate::scanner::command_exists`), consistent with the existing pattern. Do not attempt discovery if the binary is absent.
- Discovery mechanism: prefer the local REST API, `GET http://localhost:11434/api/tags`, over `ollama list`, because `ollama list`/`ollama ls` has no `--format json` flag as of current Ollama releases and only emits a human-aligned text table (name/id/size/modified) that would require fragile whitespace-column parsing. The API returns a stable JSON array of `{name, size, digest, modified_at, ...}` and needs no new heavyweight dependency if a minimal synchronous HTTP client (e.g. `ureq`, no async runtime) is added. If the request fails (daemon not running, port not listening), skip discovery and add a warning to `ScanReport.warnings`, mirroring the existing "not installed" warning pattern in `scan_packages` (`src/scanner.rs:97-101`) — do not treat "unreachable API" as an error that aborts the whole scan.
- Each discovered model becomes its own `CleanupItem`:
  - `id`: `format!("models.ollama.{name}")` (name sanitized the same way `uninstall.rs` sanitizes package identifiers).
  - `group`: proposed new `CleanupGroup::Models` (see enum change below).
  - `risk`: `Risk::Elevated` (re-downloadable, but potentially large and sometimes actively used, so not `Risk::Low`).
  - `action`: `CleanupAction::Command { program: "ollama".into(), args: vec!["rm".into(), name.clone()], requires_root: false }` — deletion goes through the CLI specifically so Ollama's own manifest/blob reference counting stays correct. This is a **hybrid CLI/API design**: API for read-only discovery, CLI for the actual mutating action, each used where it is strictly better.
- Executor allowlist: add a validated arm `("ollama", ["rm", model], false) if is_valid_identifier(model)` to `is_allowed_command` in `src/executor.rs`, reusing `crate::uninstall::is_valid_identifier`.
- Tests: a fixture test with a mocked/fake `CommandRunner` (the executor already supports substituting `CommandRunner` — see `Executor::with_runner`, `src/executor.rs:66-68`) verifying `ollama rm <name>` is invoked with the expected exact args and rejected for a non-identifier name (e.g. containing `;` or leading `-`); a discovery test using a fake HTTP layer (or a trait-abstracted "OllamaClient" so it can be swapped in tests) confirming a well-formed JSON response is turned into the expected `CleanupItem`s, and that a connection failure produces a warning rather than a panic/error.

### C. Hugging Face model cache (filesystem-only, new dedicated module or scanner extension)

The user asked about Hugging Face too, alongside Ollama. Layout: `~/.cache/huggingface/hub/{models,datasets,spaces}--<org>--<name>/{blobs,snapshots,refs}`, where `snapshots/<rev>/*` are symlinks into `blobs/` (content-addressed, but via filesystem symlinks rather than manifest references as in Ollama). Because `dir_size()` (`src/scanner.rs:225-240`) already refuses to follow symlinks and blobs are walked directly as regular files in the same subtree, computing the size of a whole `models--org--name` directory is already accurate and safe with existing helpers, and `fs::remove_dir_all` (used by `remove_entry`, `src/executor.rs:402-416`) removes symlink entries without following them — so deleting an entire repo directory as a unit is safe with the tool's existing filesystem primitives, no subprocess required.

Design:
- Add a new scan step, e.g. `scan_model_caches` in `src/scanner.rs` or the new `src/models.rs`, that lists immediate children of `~/.cache/huggingface/hub` (if the directory exists) and produces one `CleanupItem` per child directory matching the `models--*`/`datasets--*`/`spaces--*` naming convention.
- `group`: `CleanupGroup::Models` (same new group as Ollama). `risk`: `Risk::Elevated`. `action`: `CleanupAction::RemovePath { path, contents_only: false }`.
- Executor validation gap: today's `validate_path` (`src/executor.rs:230-276`) only accepts an exact, fixed set of relative paths (the `known` array) or a small fixed set of artifact *filenames* found anywhere. Hugging Face repo directory names are dynamic (`models--bert-base-cased`, etc.) and live under one fixed parent (`.cache/huggingface/hub`), which is a new validation shape: "parent path must be exactly `X`, and the child name must match a fixed prefix pattern." This needs a small, explicit new branch in `validate_path` (not a loosened general rule) that checks `path.parent() == home.join(".cache/huggingface/hub")` and the file name starts with `"models--"`, `"datasets--"`, or `"spaces--"`. This keeps the allowlist exact-and-narrow per the project's validation philosophy while accommodating dynamic names, the same way `purge.rs`/`executor.rs` already accommodate dynamic *project* artifact paths by filename match.
- CLI/API decision: **do not** shell out to `huggingface-cli`/`hf`, and **do not** embed or call into Python (`huggingface_hub`) at all. The user's own framing — "TuxCleaner is Rust with no Python dependency" — is the deciding factor: `huggingface-cli` is a Python entry point that is frequently absent even on machines with a populated HF cache (e.g. cache populated by a Rust or Ollama-adjacent tool, or by `transformers` inside a virtualenv that isn't on `PATH`), whereas the cache directory layout itself is a long-stable, documented, filesystem-only contract with no daemon to talk to (unlike Docker/Podman/Ollama, HF has no local API at all). Optionally, *if* `huggingface-cli`/`hf` is present, its `scan-cache` output could be used as a corroborating cross-check surfaced only in warnings — but it must never be required, and never used as the deletion mechanism.
- Tests: fixture test creating a fake `~/.cache/huggingface/hub/models--fake--demo/{blobs/...,snapshots/<rev>/file->../../blobs/...}` and asserting the discovered item's size matches the blob size (not double-counted through the symlink) and that deletion via the executor succeeds; a refusal test asserting `validate_path` rejects a sibling path that doesn't match the `models--`/`datasets--`/`spaces--` prefix (e.g. `~/.cache/huggingface/hub/version.txt` or an unrelated directory placed there) and a path with the right name but wrong parent (e.g. `~/models--fake--demo` directly under home).

### D. Podman (new dedicated module, `src/containers.rs`)

Podman has no daemon by default; "rootful" and "rootless" are two independent storage/runtime states reached by *who* invokes the `podman` binary (root vs. the invoking user), not by a `--context`-style flag as in Docker. Podman's own `system connection list` concept is for *remote* or *named* endpoints (e.g. Podman machine, remote SSH hosts), which is a second, orthogonal axis.

Design:
- New module `src/containers.rs` housing both Docker and Podman context-aware discovery (natural home given `docs/architecture.md`'s existing rule "Keep discovery separate from execution" and the module table listing `scanner` as owning "known system, user, developer, Docker, and Flatpak candidates" — this is a good point to split container-specific logic out of `scanner.rs` into its own module now that it is growing beyond one command per tool).
- Gate: `command_exists("podman")`.
- Two always-offered base items when the binary exists:
  - `containers.podman.rootless` — `CleanupAction::Command { program: "podman", args: ["system", "prune", "-f"], requires_root: false }`.
  - `containers.podman.rootful` — same command but `requires_root: true`, which the existing `ProcessCommandRunner` (`src/executor.rs:31-49`) already wraps in `sudo --`. This is consistent with "System operations may use narrow sudo commands" and keeps the *user* cleanup path (rootless) sudo-free, matching "User cleanup must not use sudo" — rootful Podman cleanup is explicitly a system operation, not user cleanup, and should be labeled/grouped accordingly (see Safety invariant notes on group placement below).
  - Neither includes `--volumes` — consistent with the Docker volume exclusion rule (see Safety invariant notes; this is deliberately generalized to Podman even though CLAUDE.md's literal text names Docker).
- Connection iteration: run `podman system connection list --format json` (only from the rootless, non-root invocation — connections are a per-user config concept); for each named connection, add `containers.podman.connection.<name>` running `podman --connection <name> system prune -f` (`requires_root: false`, since remote connections are inherently user-invoked, not root-invoked).
- Executor allowlist additions: `("podman", ["system", "prune", "-f"], false)`, `("podman", ["system", "prune", "-f"], true)`, and a validated arm `("podman", ["--connection", name, "system", "prune", "-f"], false) if is_valid_identifier(name)`.
- Tests: fixture tests with a fake `CommandRunner` verifying the exact three command shapes (rootless, rootful via `sudo --`, and per-connection) and a refusal test for a connection name containing shell metacharacters or a leading dash.

### E. Docker context iteration (extends existing `scan_containers`)

Design:
- Move the existing Docker block (`src/scanner.rs:171-186`) into the new `src/containers.rs`, alongside Podman, and extend it: after confirming `command_exists("docker")`, run `docker context ls --format json` (each line is one JSON object with at least `Name` and `Current`). Parse leniently (skip malformed lines, following the same "warn and continue" pattern flatpak parsing already uses at `src/uninstall.rs:454-467`) and produce one `CleanupItem` per context:
  - `id`: `format!("containers.docker.context.{name}")`.
  - `action`: `CleanupAction::Command { program: "docker", args: ["--context", name, "system", "prune", "-f"], requires_root: false }` for every context except when `name == "default"`, where the existing bare `docker system prune -f` (no `--context` flag) can be kept for backward-compatible behavior/id stability, or simplified to always pass `--context default` explicitly — recommend always passing `--context <name>` explicitly for uniformity and to avoid relying on ambient default resolution, at the minor cost of changing the existing item's exact command args (a behavior change, flagged below).
  - If `docker context ls` fails or returns nothing (e.g. very old Docker without context support), fall back to today's single unparameterized item so the feature degrades gracefully rather than disappearing.
  - This item list already gives the user "iterate through contexts when provided" and lets them opt into cleaning context A but not B, satisfying the explicit-selection invariant per context.
- Executor allowlist: add a validated arm `("docker", ["--context", name, "system", "prune", "-f"], false) if is_valid_identifier(name)`.
- Tests: fixture test with fake context-list JSON producing N items with distinct ids/args; refusal test for a context name that is not a valid identifier; a test confirming the graceful single-item fallback when context listing fails.

### F. New `CleanupGroup::Models` enum variant

Recommend adding a new `CleanupGroup::Models` variant (`src/model.rs:9-14`) rather than folding Ollama/Hugging Face into `Containers`, because these are re-downloadable AI-model artifacts with different semantics (no daemon prune verbs, no volumes concept, generally larger and more likely to represent "I might want this again next week" than a container build-cache blob). This is additive to `CleanupGroup::ALL` (`src/model.rs:17`) and to the JSON `type` tag space — new enum variants are additive per the "Preserve JSON output compatibility by adding fields rather than renaming or removing them" rule, since existing variants keep their exact serialized name. `CleanupGroup::Containers`'s `title()` (`src/model.rs:24`, currently `"Docker & Flatpak"`) should be updated to `"Docker, Podman & Flatpak"` to stay accurate — this is a display-string change only, not a schema change, since `title()` is not itself serialized (the enum discriminant is).

## Container & model-cache CLI-vs-API decision summary

| Tool | Recommendation | One-line justification |
| --- | --- | --- |
| Docker | CLI (`docker` subprocess) | Contexts are a CLI/config-file concept (`~/.docker/contexts`) more than an Engine-API concept reachable uniformly across arbitrary sockets/TLS setups; CLI keeps the existing typed-`Command`/allowlist model with zero new dependencies and matches the already-established `command_exists("docker")` gate. |
| Podman | CLI (`podman` subprocess) | Same reasoning as Docker, plus Podman's REST API is opt-in (`podman system service` must be manually enabled), so it cannot be assumed present, while the `podman` binary is the one thing `command_exists("podman")` can reliably detect. |
| Ollama | Hybrid: local REST API (`GET /api/tags`) for discovery, `ollama` CLI (`ollama rm <name>`) for deletion | `ollama list` has no JSON output (would require fragile table parsing) so the JSON API is strictly better for read-only discovery; but deletion must go through the CLI because Ollama's blob storage is reference-counted across manifests and only `ollama rm` performs that garbage collection safely — reimplementing it via raw filesystem deletes risks damaging other models' shared blobs. |
| Hugging Face | Filesystem-only (no CLI, no API, no Python) | The cache layout is a stable, fully-documented, symlink-based content-addressed directory contract with no daemon or API to call at all; `huggingface-cli`/`hf` are Python entry points that are frequently absent on a Rust-only system (the exact "no Python dependency" concern the user raised), so treating the directory itself as the source of truth (as TuxCleaner already does for `.cache/pip`, `.cache/pnpm`, etc.) needs no new runtime dependency and no new failure mode. |

### Context/connection iteration design (shared shape)

For both Docker and Podman, context/connection discovery follows one shape, implementable once and reused:

1. Gate on `command_exists(program)`.
2. Run the listing subcommand with `--format json` (`docker context ls --format json`, `podman system connection list --format json`).
3. Parse each output line/array element defensively; a line/element that fails to parse is skipped with a warning, never treated as fatal (matches the tolerant-parsing precedent in `discover_flatpaks`, `src/uninstall.rs:448-467`, and in `history.rs`'s "skip individual malformed JSONL records" behavior per `docs/architecture.md`).
4. For every valid entry, synthesize one `CleanupItem` whose `id` embeds the sanitized context/connection name and whose `action` embeds that name as a *validated, allowlisted* extra argument (`is_valid_identifier`), never interpolated into a shell string.
5. If listing itself fails (old CLI without context support, no config file yet, tool present but never configured), fall back to a single generic item (today's existing behavior) so the feature never regresses when there is nothing to iterate.

This gives the "opt into cleaning context A but not B" behavior for free, because each context is already a fully independent `CleanupItem` id that flows through the same `MultiSelect`/`--yes`-gated selection paths every other item already uses (`src/cli/clean.rs`, `src/tui/execution.rs`) with no new UI concept required.

## Safety invariant notes and conflicts

- **Docker/Podman volumes stay out of group cleanup.** Every proposed Docker/Podman action here is `system prune -f` without `--volumes`. No design in this document proposes a volume-pruning `CleanupItem` inside `CleanupGroup::Containers`/batch cleanup, per the explicit invariant. If the maintainers still want *some* way to reclaim unused volume space, it would have to be a separate, individually-selected, `--yes`-gated, non-batchable command outside the normal group-scan flow (e.g. a dedicated `tuxcleaner clean --docker-volumes` flag requiring its own explicit confirmation, never included in `scan()`'s default item list or in "select all"). This document flags that tension but does not recommend building it, since the user only asked to flag it, not implement it — leaving it as an open question below.
- **Podman rootful is a system operation, not user cleanup.** "User cleanup must not use sudo" — the rootful Podman item must not be placed alongside the rootless item under a casual "select all containers" gesture without the user understanding it will prompt for `sudo`. Recommend `Risk::Elevated` at minimum (matching Docker's existing risk level) and possibly keeping it visually/grouping-wise distinguishable (e.g. label explicitly says "(root)"), even though it technically still sits in `CleanupGroup::Containers`/`Models` alongside non-root items — this mirrows how `system.journal` (`src/scanner.rs:106-123`) already coexists with non-root items inside `CleanupGroup::System`, so it is a precedented pattern, not a new one.
- **`.terraform` may contain local state.** Flagged above under artifact additions — recommend `purge`-only placement (never an unconditional `clean` group item) plus a doc/help note. Deleting `.terraform` when a remote backend is *not* configured could delete state indirectly by removing `.terraform/terraform.tfstate` if a local backend is used without a separate root-level `terraform.tfstate` file (Terraform's default local backend actually writes `terraform.tfstate` at the project root, not inside `.terraform/`, so the common case is safe — but a project using `-backend-config` pointed at a path *inside* `.terraform/` would not be). This is a narrow but real edge case worth a one-line CLI warning rather than blocking the feature.
- **Hidden-app-data-is-report-only applies to `analyze`, not to designed cleanup rules generally.** CLAUDE.md's "Large personal files and hidden application data are reported only, never deleted by `analyze`" is scoped to the `analyze` command's large-file/app-data reporting feature (`src/analyze.rs`'s `LargeFile.app_data` flag), which already coexists with plenty of hidden, dot-prefixed directories that a *different* command (`clean`) safely deletes today (`.cache/pip`, `.cache/yay`, etc.). Ollama's `~/.ollama` and Hugging Face's `~/.cache/huggingface` are hidden but are the same *kind* of thing as those existing entries — reproducible, tool-owned cache data — not personal files. This document treats them as `clean`-eligible cache items, consistent with existing precedent, and explicitly not as an exception to the `analyze` invariant (which is untouched).
- **No shell-string execution anywhere in this design.** Every proposed action is `CleanupAction::Command`/`CommandSequence` with a fixed program and a `Vec<String>` of arguments (context/connection/model names pass through `is_valid_identifier` before being allowlisted), or `CleanupAction::RemovePath` on a `PathBuf`. No `sh -c`, no string formatting into a single command string.
- **Docker existing item's exact command args would change** if the "always pass `--context <name>` explicitly" recommendation (section E) is adopted, since today's item runs bare `docker system prune -f` with no `--context` flag. This is a behavior change to an existing, already-shipped `CleanupItem`/allowlist entry, not a purely additive one — flagged for explicit maintainer sign-off (see Open questions).

## Open questions for the user

1. Should the new Ollama/Hugging Face items live in one new `src/models.rs` module, or should Ollama go in the same new `src/containers.rs` module as Docker/Podman (since it also involves a local daemon/API) while Hugging Face stays as a pure scanner extension? This document assumes two separate concerns (daemon-backed vs. filesystem-only) but the maintainers may prefer fewer new files.
2. Is adding a synchronous HTTP client dependency (e.g. `ureq`) acceptable for the Ollama discovery hybrid, or would the maintainers rather accept the fragile `ollama list` table-parsing to avoid a new dependency entirely (falling back to pure-CLI for both discovery and deletion)?
3. For Docker context iteration: is it acceptable to change the *existing* default item's command from bare `docker system prune -f` to explicit `docker --context default system prune -f` (a behavior change to a shipped allowlist entry), or should the default context keep its current bare-command form for stability and only *additional* non-default contexts get the `--context` flag?
4. Should Podman's rootful item be offered at all by default-on scans (`clean` with no flags), or should anything that necessarily invokes `sudo` require an explicit opt-in flag (e.g. `--include-root`) before it even appears in the item list — separate from the existing per-item `Risk::Elevated`/selection gating? (The existing `system.journal` precedent suggests "appear in the list, gated by selection/`--yes`" is the established norm, but Podman rootful is a heavier, more surprising action than journal vacuuming.)
5. Does the project want a deliberately separate, non-batchable Docker/Podman volume-pruning command despite (or because of) the group-cleanup exclusion, or should volumes remain entirely out of scope for TuxCleaner, full stop?
6. For `.pnpm-store`, is a static `$HOME/.pnpm-store` path good enough, or is it worth invoking `pnpm store path` (a new small CLI dependency) to find a custom store directory? This document recommends starting static and revisiting if users report misses.
7. Multiple `OLLAMA_MODELS` / non-default install locations: should discovery also read the `OLLAMA_MODELS` environment variable (if set) to find models stored outside `~/.ollama/models`, mirroring how `Distribution::detect()` already supports a `TUXCLEANER_OS_RELEASE` override for testability (`src/distro.rs:27-33`)? Recommend yes, but flagging since it wasn't explicit in the request.
