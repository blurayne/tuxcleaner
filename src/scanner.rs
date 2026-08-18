use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use rayon::prelude::*;

use crate::analyze::parallel_dir_size;
use crate::distro::{Distribution, DistroFamily};
use crate::model::{CleanupAction, CleanupGroup, CleanupItem, Risk, ScanReport};
use crate::uninstall::{is_valid_identifier, is_valid_snap_revision};

/// Directory where snapd stores every revision payload it has ever installed,
/// as `<name>_<revision>.snap`. World-readable even though the files
/// themselves are root-owned mode 0600, so `fs::metadata` can size them
/// without elevated privileges.
const SNAPD_SNAPS_DIR: &str = "/var/lib/snapd/snaps";

/// Directory containing one subdirectory per installed snap, each with a
/// `current` symlink whose target names the active revision.
const SNAP_DIR: &str = "/snap";

const USER_CACHE_PATHS: &[(&str, &str)] = &[
    (".cache/yay", "yay build cache"),
    (".cache/paru", "paru build cache"),
    (".cache/thumbnails", "Desktop thumbnails"),
    (".cache/mozilla", "Firefox cache"),
    (".cache/chromium", "Chromium cache"),
    (".cache/google-chrome", "Google Chrome cache"),
    (".cache/microsoft-edge", "Microsoft Edge cache"),
    (".cache/BraveSoftware", "Brave cache"),
    (".cache/vivaldi", "Vivaldi cache"),
    (".cache/opera", "Opera cache"),
];

const DEV_CACHE_PATHS: &[(&str, &str)] = &[
    (".cache/pip", "pip cache"),
    (".npm/_cacache", "npm content cache"),
    (".cache/pnpm", "pnpm cache"),
    (".cache/yarn", "Yarn cache"),
    (".cargo/registry/cache", "Cargo package cache"),
    (".cargo/registry/src", "Cargo unpacked registry sources"),
    (".cargo/git", "Cargo Git checkout cache"),
    ("go/pkg/mod", "Go module cache"),
    (".gradle/caches", "Gradle cache"),
    (".cache/composer", "Composer cache"),
    (".cache/uv", "uv package manager cache"),
    (".pnpm-store", "pnpm content-addressable store"),
    (".cache/terragrunt", "Terragrunt provider cache"),
    (".terraform.d/plugin-cache", "Terraform plugin cache"),
    (".cache/ms-playwright", "Playwright browser downloads"),
    (".cache/.bun", "Bun install cache"),
    (".cache/zig", "Zig compiler cache"),
    (".cache/pre-commit", "pre-commit hook environments"),
    (".cache/go-build", "Go build cache"),
    (".cache/ms-playwright-go", "Playwright Go driver cache"),
];

pub struct Scanner {
    home: PathBuf,
    distro: Distribution,
}

impl Scanner {
    pub fn new(home: PathBuf, distro: Distribution) -> Self {
        Self { home, distro }
    }

    pub fn system_default() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
        Ok(Self::new(home, Distribution::detect()?))
    }

    pub fn scan(&self) -> ScanReport {
        let mut items = Vec::new();
        let mut warnings = Vec::new();

        self.scan_packages(&mut items, &mut warnings);
        self.scan_system(&mut items);
        self.scan_snap_revisions(&mut items, &mut warnings);
        self.scan_known_paths(USER_CACHE_PATHS, CleanupGroup::User, &mut items);
        self.scan_trash(&mut items);
        self.scan_known_paths(DEV_CACHE_PATHS, CleanupGroup::Dev, &mut items);
        self.scan_containers(&mut items, &mut warnings);
        crate::models::scan(&self.home, &mut items, &mut warnings);

        if self.distro.family == DistroFamily::Unsupported {
            warnings.push(format!(
                "{} is not yet supported for package cleanup; user and developer caches are still available",
                self.distro.name
            ));
        }

        sort_items_by_group_size(&mut items);
        ScanReport::from_items(self.distro.name.clone(), items, warnings)
    }

    fn scan_packages(&self, items: &mut Vec<CleanupItem>, warnings: &mut Vec<String>) {
        let estimated = self
            .distro
            .package_cache_paths()
            .iter()
            .map(Path::new)
            .map(dir_size)
            .sum();

        if let Some(item) = self.distro.package_cleanup_item(estimated) {
            let program = match &item.action {
                CleanupAction::Command { program, .. } => program,
                CleanupAction::CommandSequence { commands } => &commands[0].program,
                _ => unreachable!(),
            };
            if command_exists(program) {
                items.push(item);
            } else if estimated > 0 {
                warnings.push(format!(
                    "{program} is not installed; the package cache was measured but cannot be cleaned safely"
                ));
            }
        }
    }

    fn scan_system(&self, items: &mut Vec<CleanupItem>) {
        if command_exists("journalctl") {
            let journal_bytes = dir_size(Path::new("/var/log/journal"));
            let reclaimable = journal_bytes.saturating_sub(200 * 1024 * 1024);
            if reclaimable > 0 {
                items.push(CleanupItem {
                    id: "system.journal".into(),
                    group: CleanupGroup::System,
                    label: "systemd journal above 200 MiB".into(),
                    estimated_bytes: reclaimable,
                    risk: Risk::Elevated,
                    action: CleanupAction::Command {
                        program: "journalctl".into(),
                        args: vec!["--vacuum-size=200M".into()],
                        requires_root: true,
                    },
                });
            }
        }
    }

    fn scan_snap_revisions(&self, items: &mut Vec<CleanupItem>, warnings: &mut Vec<String>) {
        if !command_exists("snap") {
            return;
        }
        let (mut discovered, mut snap_warnings) =
            discover_disabled_snap_revisions(Path::new(SNAPD_SNAPS_DIR), Path::new(SNAP_DIR));
        let found_any = !discovered.is_empty();
        items.append(&mut discovered);
        warnings.append(&mut snap_warnings);
        if found_any {
            warnings.push(
                "disabled snap revisions were found; to keep fewer of them after future \
                 refreshes, run `sudo snap set system refresh.retain=2` manually (tuxcleaner \
                 does not change this setting)"
                    .into(),
            );
        }
    }

    fn scan_known_paths(
        &self,
        definitions: &[(&str, &str)],
        group: CleanupGroup,
        items: &mut Vec<CleanupItem>,
    ) {
        // Size every known path in parallel; `par_iter().map(..).collect()` preserves the
        // original `definitions` order, so the resulting items keep the exact same order as
        // the previous sequential loop.
        let sized: Vec<(PathBuf, &str, u64)> = definitions
            .par_iter()
            .map(|(relative, label)| {
                let path = self.home.join(relative);
                let estimated_bytes = dir_size(&path);
                (path, *label, estimated_bytes)
            })
            .collect();

        for ((relative, _), (path, label, estimated_bytes)) in definitions.iter().zip(sized) {
            if estimated_bytes == 0 {
                continue;
            }
            items.push(CleanupItem {
                id: format!("{}.{}", group_slug(group), relative.replace('/', ".")),
                group,
                label: label.into(),
                estimated_bytes,
                risk: Risk::Low,
                action: CleanupAction::RemovePath {
                    path,
                    contents_only: false,
                },
            });
        }
    }

    fn scan_trash(&self, items: &mut Vec<CleanupItem>) {
        let path = self.home.join(".local/share/Trash");
        let estimated_bytes = dir_size(&path);
        if estimated_bytes > 0 {
            items.push(CleanupItem {
                id: "user.trash".into(),
                group: CleanupGroup::User,
                label: "Trash contents".into(),
                estimated_bytes,
                risk: Risk::Explicit,
                action: CleanupAction::RemovePath {
                    path,
                    contents_only: true,
                },
            });
        }
    }

    fn scan_containers(&self, items: &mut Vec<CleanupItem>, warnings: &mut Vec<String>) {
        crate::containers::scan(items, warnings);
    }
}

fn group_slug(group: CleanupGroup) -> &'static str {
    match group {
        CleanupGroup::System => "system",
        CleanupGroup::User => "user",
        CleanupGroup::Dev => "dev",
        CleanupGroup::Containers => "containers",
        CleanupGroup::Models => "models",
    }
}

/// Enumerates disabled snap revisions by comparing every `<name>_<revision>.snap`
/// payload in `snaps_dir` against the active revision recorded by the
/// `current` symlink under `snap_dir/<name>`.
///
/// This filesystem-based approach was chosen over parsing `snap list --all`
/// text output because it does not depend on a human-oriented table format,
/// and because the `current` symlink is the same source of truth snapd
/// itself uses to know which revision is active. A snap is only ever treated
/// as disabled when its active revision was positively identified and
/// differs from the file being inspected; if the active revision cannot be
/// determined for a snap, all of that snap's revisions are skipped rather
/// than guessed at, so a missing or broken `current` symlink can never cause
/// an active revision to be reported as removable.
///
/// Unparseable or malformed entries (unexpected file names, non-numeric
/// revisions) are skipped with a warning; they never abort the scan.
pub fn discover_disabled_snap_revisions(
    snaps_dir: &Path,
    snap_dir: &Path,
) -> (Vec<CleanupItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut warnings = Vec::new();

    let entries = match fs::read_dir(snaps_dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!("failed to read {}: {error}", snaps_dir.display()));
            return (items, warnings);
        }
    };

    let mut active_revisions: HashMap<String, Option<String>> = HashMap::new();
    let mut warned_names: HashSet<String> = HashSet::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "failed to read an entry in {}: {error}",
                    snaps_dir.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("snap") {
            continue;
        }
        let Some((name, revision)) = parse_snap_revision_filename(&path) else {
            warnings.push(format!(
                "skipping unparseable snap revision file: {}",
                path.display()
            ));
            continue;
        };

        let active = active_revisions
            .entry(name.clone())
            .or_insert_with(|| read_active_snap_revision(snap_dir, &name));

        match active.as_deref() {
            Some(active_revision) if active_revision == revision => continue,
            Some(_) => {}
            None => {
                if warned_names.insert(name.clone()) {
                    warnings.push(format!(
                        "cannot determine the active revision for snap {name}; skipping its \
                         revisions to avoid removing the active one"
                    ));
                }
                continue;
            }
        }

        let estimated_bytes = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        items.push(CleanupItem {
            id: format!("packages.snap.{name}.{revision}"),
            group: CleanupGroup::System,
            label: format!("Disabled snap revision {name} r{revision}"),
            estimated_bytes,
            risk: Risk::Elevated,
            action: CleanupAction::Command {
                program: "snap".into(),
                args: vec![
                    "remove".into(),
                    format!("--revision={revision}"),
                    name.clone(),
                ],
                requires_root: true,
            },
        });
    }

    (items, warnings)
}

fn parse_snap_revision_filename(path: &Path) -> Option<(String, String)> {
    let stem = path.file_stem()?.to_str()?;
    let (name, revision) = stem.rsplit_once('_')?;
    if !is_valid_identifier(name) || !is_valid_snap_revision(revision) {
        return None;
    }
    Some((name.to_owned(), revision.to_owned()))
}

fn read_active_snap_revision(snap_dir: &Path, name: &str) -> Option<String> {
    let current = snap_dir.join(name).join("current");
    let target = fs::read_link(&current).ok()?;
    target.file_name()?.to_str().map(str::to_owned)
}

pub fn command_exists(program: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|directory| {
        let candidate = directory.join(program);
        fs::metadata(candidate)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    })
}

pub fn dir_size(path: &Path) -> u64 {
    parallel_dir_size(path, &AtomicBool::new(false))
}

pub fn summarize_by_group(items: &[CleanupItem]) -> HashMap<CleanupGroup, u64> {
    let mut sizes = HashMap::new();
    for item in items {
        *sizes.entry(item.group).or_default() += item.estimated_bytes;
    }
    sizes
}

/// Orders cleanup items so the group with the largest total reclaimable
/// size sorts first, and within each group the largest item sorts first.
/// Ties are broken deterministically (group title, then item size, then
/// item id) so JSON output does not churn between otherwise-equal runs.
pub fn sort_items_by_group_size(items: &mut [CleanupItem]) {
    let group_totals = summarize_by_group(items);
    let group_total = |group: CleanupGroup| group_totals.get(&group).copied().unwrap_or(0);
    items.sort_by(|a, b| {
        group_total(b.group)
            .cmp(&group_total(a.group))
            .then_with(|| a.group.title().cmp(b.group.title()))
            .then_with(|| b.estimated_bytes.cmp(&a.estimated_bytes))
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn dir_size_does_not_follow_symlinks() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("large"), vec![0; 4096]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        assert_eq!(dir_size(root.path()), 0);
    }

    #[test]
    fn scan_only_includes_existing_known_cache_paths() {
        let root = tempdir().unwrap();
        let cache = root.path().join(".cache/pip");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("wheel"), vec![0; 123]).unwrap();
        let scanner = Scanner::new(
            root.path().to_path_buf(),
            Distribution::parse_os_release("ID=custom\nNAME=Custom\n"),
        );
        let report = scanner.scan();
        assert!(report.items.iter().any(|item| item.id == "dev..cache.pip"));
        assert!(!report.items.iter().any(|item| item.id.contains("mozilla")));
    }

    #[test]
    fn scan_includes_uv_and_pnpm_store_only_when_populated() {
        let root = tempdir().unwrap();
        let scanner = Scanner::new(
            root.path().to_path_buf(),
            Distribution::parse_os_release("ID=custom\nNAME=Custom\n"),
        );

        // Neither directory exists yet: neither id should appear.
        let report = scanner.scan();
        assert!(!report.items.iter().any(|item| item.id == "dev..cache.uv"));
        assert!(!report.items.iter().any(|item| item.id == "dev..pnpm-store"));

        let uv_cache = root.path().join(".cache/uv");
        fs::create_dir_all(&uv_cache).unwrap();
        fs::write(uv_cache.join("wheel"), vec![0; 64]).unwrap();
        let pnpm_store = root.path().join(".pnpm-store");
        fs::create_dir_all(&pnpm_store).unwrap();
        fs::write(pnpm_store.join("blob"), vec![0; 64]).unwrap();

        let report = scanner.scan();
        assert!(report.items.iter().any(|item| item.id == "dev..cache.uv"));
        assert!(report.items.iter().any(|item| item.id == "dev..pnpm-store"));
    }

    #[test]
    fn scan_includes_additional_dev_and_browser_caches_only_when_populated() {
        let root = tempdir().unwrap();
        let scanner = Scanner::new(
            root.path().to_path_buf(),
            Distribution::parse_os_release("ID=custom\nNAME=Custom\n"),
        );

        let expected_absent = [
            "dev..cache.terragrunt",
            "dev..terraform.d.plugin-cache",
            "dev..cache.ms-playwright",
            "dev..cache..bun",
            "dev..cache.zig",
            "dev..cache.pre-commit",
            "dev..cache.go-build",
            "dev..cache.ms-playwright-go",
            "user..cache.microsoft-edge",
        ];
        let report = scanner.scan();
        for id in expected_absent {
            assert!(
                !report.items.iter().any(|item| item.id == id),
                "expected {id} to be absent before the directory exists"
            );
        }

        let populated = [
            (".cache/terragrunt", "dev..cache.terragrunt"),
            (".terraform.d/plugin-cache", "dev..terraform.d.plugin-cache"),
            (".cache/ms-playwright", "dev..cache.ms-playwright"),
            (".cache/.bun", "dev..cache..bun"),
            (".cache/zig", "dev..cache.zig"),
            (".cache/pre-commit", "dev..cache.pre-commit"),
            (".cache/go-build", "dev..cache.go-build"),
            (".cache/ms-playwright-go", "dev..cache.ms-playwright-go"),
            (".cache/microsoft-edge", "user..cache.microsoft-edge"),
        ];
        for (relative, _) in populated {
            let dir = root.path().join(relative);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("data"), vec![0; 32]).unwrap();
        }

        let report = scanner.scan();
        for (_, id) in populated {
            assert!(
                report.items.iter().any(|item| item.id == id),
                "expected {id} to be present once the directory is populated"
            );
        }
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn discovers_disabled_snap_revisions_and_never_returns_the_active_one() {
        let snaps_dir = tempdir().unwrap();
        let snap_dir = tempdir().unwrap();

        fs::write(snaps_dir.path().join("firefox_8664.snap"), vec![0; 500]).unwrap();
        fs::write(snaps_dir.path().join("firefox_8754.snap"), vec![0; 700]).unwrap();
        fs::write(snaps_dir.path().join("core20_2769.snap"), vec![0; 300]).unwrap();
        fs::write(snaps_dir.path().join("core20_2866.snap"), vec![0; 400]).unwrap();

        fs::create_dir_all(snap_dir.path().join("firefox")).unwrap();
        fs::create_dir_all(snap_dir.path().join("core20")).unwrap();
        symlink(Path::new("8754"), &snap_dir.path().join("firefox/current"));
        symlink(Path::new("2866"), &snap_dir.path().join("core20/current"));

        let (items, warnings) = discover_disabled_snap_revisions(snaps_dir.path(), snap_dir.path());

        assert!(warnings.is_empty(), "{warnings:?}");
        let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"packages.snap.firefox.8664".to_string()));
        assert!(ids.contains(&"packages.snap.core20.2769".to_string()));
        // The active revisions must never be reported as disabled.
        assert!(!ids.iter().any(|id| id.ends_with(".8754")));
        assert!(!ids.iter().any(|id| id.ends_with(".2866")));

        let firefox_item = items
            .iter()
            .find(|item| item.id == "packages.snap.firefox.8664")
            .unwrap();
        assert_eq!(firefox_item.estimated_bytes, 500);
        assert_eq!(firefox_item.group, CleanupGroup::System);
        assert_eq!(firefox_item.risk, Risk::Elevated);
        assert_eq!(
            firefox_item.action,
            CleanupAction::Command {
                program: "snap".into(),
                args: vec!["remove".into(), "--revision=8664".into(), "firefox".into()],
                requires_root: true,
            }
        );
    }

    #[test]
    fn skips_a_snap_when_its_active_revision_cannot_be_determined() {
        let snaps_dir = tempdir().unwrap();
        // No "current" symlink is created for this snap in snap_dir at all.
        let snap_dir = tempdir().unwrap();

        fs::write(snaps_dir.path().join("hwctl_72.snap"), vec![0; 10]).unwrap();
        fs::write(snaps_dir.path().join("hwctl_123.snap"), vec![0; 20]).unwrap();

        let (items, warnings) = discover_disabled_snap_revisions(snaps_dir.path(), snap_dir.path());
        assert!(items.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("hwctl"));
    }

    #[test]
    #[cfg(unix)]
    fn skips_unparseable_snap_filenames_with_a_warning_but_keeps_scanning() {
        let snaps_dir = tempdir().unwrap();
        let snap_dir = tempdir().unwrap();

        fs::write(snaps_dir.path().join("noRevisionMarker.snap"), vec![0; 10]).unwrap();
        fs::write(snaps_dir.path().join("mysnap_x1.snap"), vec![0; 10]).unwrap();
        fs::write(snaps_dir.path().join("firefox_8664.snap"), vec![0; 10]).unwrap();
        fs::write(snaps_dir.path().join("firefox_8754.snap"), vec![0; 10]).unwrap();
        fs::create_dir_all(snap_dir.path().join("firefox")).unwrap();
        symlink(Path::new("8754"), &snap_dir.path().join("firefox/current"));

        let (items, warnings) = discover_disabled_snap_revisions(snaps_dir.path(), snap_dir.path());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "packages.snap.firefox.8664");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn discovery_degrades_gracefully_when_the_snaps_directory_is_unreadable() {
        let missing = Path::new("/nonexistent/tuxcleaner-snap-revision-test-dir");
        let snap_dir = tempdir().unwrap();
        let (items, warnings) = discover_disabled_snap_revisions(missing, snap_dir.path());
        assert!(items.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    fn item(id: &str, group: CleanupGroup, estimated_bytes: u64) -> CleanupItem {
        CleanupItem {
            id: id.into(),
            group,
            label: id.into(),
            estimated_bytes,
            risk: Risk::Low,
            action: CleanupAction::RemovePath {
                path: PathBuf::from("/tmp").join(id),
                contents_only: false,
            },
        }
    }

    #[test]
    fn sort_orders_groups_by_total_and_items_by_size_within_group() {
        let mut items = vec![
            item("system.a", CleanupGroup::System, 100),
            item("user.a", CleanupGroup::User, 50),
            item("user.b", CleanupGroup::User, 80),
            item("dev.a", CleanupGroup::Dev, 200),
        ];
        sort_items_by_group_size(&mut items);
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        // Dev totals 200, User totals 130, System totals 100, Containers is
        // absent. Within User, the 80-byte item must precede the 50-byte one.
        assert_eq!(ids, ["dev.a", "user.b", "user.a", "system.a"]);
    }

    #[test]
    fn sort_breaks_ties_deterministically_by_id() {
        let mut items = vec![
            item("dev.b", CleanupGroup::Dev, 100),
            item("dev.a", CleanupGroup::Dev, 100),
        ];
        sort_items_by_group_size(&mut items);
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["dev.a", "dev.b"]);
    }

    #[test]
    fn scan_report_groups_order_matches_item_order() {
        let mut items = vec![
            item("system.a", CleanupGroup::System, 100),
            item("user.a", CleanupGroup::User, 50),
            item("user.b", CleanupGroup::User, 80),
            item("dev.a", CleanupGroup::Dev, 200),
        ];
        sort_items_by_group_size(&mut items);
        let report = ScanReport::from_items("Test".into(), items, Vec::new());

        let groups_with_items: Vec<_> = report
            .groups
            .iter()
            .filter(|summary| summary.item_count > 0)
            .map(|summary| summary.group)
            .collect();
        assert_eq!(
            groups_with_items,
            [CleanupGroup::Dev, CleanupGroup::User, CleanupGroup::System]
        );

        // The group order (largest total first) must match the order in
        // which those groups first appear among the sorted items.
        let mut first_seen = Vec::new();
        for item in &report.items {
            if !first_seen.contains(&item.group) {
                first_seen.push(item.group);
            }
        }
        assert_eq!(groups_with_items, first_seen);
    }
}
