# Security policy

TuxCleaner performs local maintenance operations, some of which permanently remove files or invoke privileged package-manager commands. Safety failures should be treated as security issues.

## Supported version

Only the latest release is supported while the project is in its initial development phase.

## Cleanup trust boundaries

TuxCleaner does not accept arbitrary cleanup commands. System actions are built by distribution adapters and must match an exact executable, argument list, and privilege requirement in the executor.

Filesystem removal is limited to known cache paths under the current user's canonical home directory and exact project artifact directory names discovered by the purge scanner. The executor rejects:

- root and the home directory itself
- relative paths and parent traversal
- anything outside the home directory
- `.git` at any path depth
- `.config`, `.ssh`, and `.gnupg`
- unknown directory names
- cleanup targets or ancestors that are symbolic links

Docker cleanup uses `docker system prune -f` without `--volumes`. TuxCleaner does not enumerate or remove Docker volumes.

Large personal files found by `analyze` are read-only results. The command offers no deletion mode.

## Application uninstall trust boundary

TuxCleaner lists only explicitly installed native packages that own visible desktop entries, plus Flatpak applications. Automated uninstall requires a source-qualified ID returned by the current catalog. The executor rejects malformed identifiers, mismatched IDs, protected system packages, and unexpected command shapes.

Native package managers must return a transaction preview before confirmation. A preview may include dependencies selected by the package manager, so users should review the full plan. TuxCleaner preserves application configuration and user data, including Flatpak data under `~/.var/app`.

## Privilege model

Package cache, native application uninstall, and journal cleanup use `sudo -- <program> <arguments>` when the process is not already running as root. User caches, developer caches, Docker, Flatpak, and project artifacts do not request privilege escalation.

Do not run the complete TuxCleaner process as root. Use the normal user account and allow the narrow `sudo` commands after reviewing the selected system group.

## Reporting a vulnerability

Please open a private security advisory in the project repository. Include the affected version, exact command, distribution, expected safety boundary, and a minimal reproduction. Do not include personal file paths, credentials, or command history containing sensitive data.
