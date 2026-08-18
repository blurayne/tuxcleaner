# AGENTS.md

Fork workflow rules for this repository. These rules are binding for every agent and every human working in this checkout. They complement `CLAUDE.md`, which stays the upstream project's file and describes how to write TuxCleaner code. This file describes where that code is allowed to land.

## Fork topology

| Remote | URL | Meaning |
| --- | --- | --- |
| `origin` | `git@github.com:blurayne/tuxcleaner.git` | our fork |
| `upstream` | `https://github.com/debba/tuxcleaner.git` | the original project we forked off |

## Branch roles

**`upstream`** is a read-only mirror of `upstream/main`. It tracks `upstream/main` and must never contain a single one of our commits. It exists so that every branch intended for an upstream pull request has a clean, unpolluted base to fork from and to diff against.

**`main`** is our fork's integration branch. It holds upstream history plus every change we have made to the fork. All finished work merges back here. `main` is the answer to "what does our fork look like right now".

**Feature branches** hold one topic each and are developed in their own git worktree. The base depends on where the change is meant to go:

- Intended for an upstream pull request: branch off `upstream`, so the pull request diff contains only that topic and no fork-only noise.
- Fork-only change (nothing upstream would want, or something that depends on other fork-only work): branch off `main`.

Both kinds merge into `main` when done. An upstream-targeted branch is pushed to `origin` and used to open the pull request against `debba/tuxcleaner`, and it also gets merged into `main` so our fork carries the change while the pull request is pending.

## Hard rules

1. Never commit to `upstream`. Never merge, cherry-pick, or rebase anything onto it. The only permitted update is a fast-forward from `upstream/main`.
2. Never open a pull request against upstream from `main`. `main` carries fork-only history and would produce an unreviewable diff.
3. A branch destined for an upstream pull request is based on `upstream` and touches nothing fork-only (see the list below). Verify with `git diff upstream...HEAD --stat` before pushing.
4. Every feature branch gets its own worktree. Do not develop directly in the primary checkout.
5. Merge feature branches into `main`. Do not rewrite published history on `main`.
6. Run the full validation suite from `CLAUDE.md` before merging anything into `main` or pushing an upstream pull request branch.

## Fork-only files

These exist in our fork and must never appear in an upstream pull request:

- `AGENTS.md` (this file)
- `docs/superpowers/`

Add to this list whenever new fork-only material is introduced.

## Intentional divergences from upstream policy

`CLAUDE.md` is upstream's file and must never be edited in this fork, because every edit becomes a permanent merge conflict on each sync. Where our fork deliberately departs from a rule stated there, record it here instead.

**Hidden application data is selectable in `analyze`.** `CLAUDE.md` says "Large personal files and hidden application data are reported only, never deleted by `analyze`." Our fork relaxes the hidden-data half of that rule: a large file under a dot-directory can be selected and removed through the normal explicit-selection and confirmation flow. Authorized by the repository owner on 2026-08-18. Every other protection still applies, and `.ssh`, `.gnupg`, `.config`, and `.git` remain hard-blocked at any size under the separate invariant that forbids deleting them. Any branch carrying this change is fork-only and must never be sent upstream.

## Recipes

Sync the mirror and bring upstream changes into our fork:

```bash
git fetch upstream
git switch upstream && git merge --ff-only upstream/main
git switch main && git merge upstream
```

If the fast-forward is refused, `upstream` has been polluted. Reset it instead of merging: `git switch upstream && git reset --hard upstream/main`.

Start a branch for an upstream pull request:

```bash
git fetch upstream
git worktree add -b feat-my-topic ../tuxcleaner.feat-my-topic upstream/main
```

Start a fork-only branch:

```bash
git worktree add -b fork-my-topic ../tuxcleaner.fork-my-topic main
```

Finish a branch:

```bash
# in the worktree, after the CLAUDE.md validation suite passes
git diff upstream...HEAD --stat    # upstream-targeted branches only: confirm the diff is clean
git push -u origin feat-my-topic   # upstream-targeted branches only
gh pr create --repo debba/tuxcleaner --base main --head blurayne:feat-my-topic

# back in the primary checkout
git switch main && git merge feat-my-topic
```

Remove the worktree once the branch is merged and the pull request is closed:

```bash
git worktree remove ../tuxcleaner.feat-my-topic
```

## Worktree layout

Worktrees live next to the primary checkout as `../tuxcleaner.<branch>`, matching the existing `../tuxcleaner.feat-parallel-analyse`.
