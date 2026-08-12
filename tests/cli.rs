use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::tempdir;

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tuxcleaner"))
}

#[test]
fn help_lists_the_mvp_commands() {
    command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("clean"))
        .stdout(predicate::str::contains("uninstall"))
        .stdout(predicate::str::contains("analyze"))
        .stdout(predicate::str::contains("purge"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("history"));
}

#[test]
fn uninstall_yes_requires_exact_application_ids() {
    command()
        .args(["uninstall", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "requires at least one exact --app",
        ));
}

#[test]
fn uninstall_dry_run_uses_fixture_catalog_without_removing_anything() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().unwrap();
    let state = tempdir().unwrap();
    let fake_bin = home.path().join("bin");
    let desktop_dir = home.path().join("desktop");
    let desktop = desktop_dir.join("firefox.desktop");
    let os_release = home.path().join("os-release");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&desktop_dir).unwrap();
    fs::write(&os_release, "NAME=Arch Linux\nID=arch\n").unwrap();
    fs::write(
        &desktop,
        "[Desktop Entry]\nType=Application\nName=Firefox\n",
    )
    .unwrap();

    let pacman = fake_bin.join("pacman");
    fs::write(
        &pacman,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  -Qqe) printf 'firefox\\n' ;;\n  -Qqo) printf 'firefox\\n' ;;\n  -Qi) printf 'Name : firefox\\nVersion : 1.0-1\\nInstalled Size : 250 MiB\\n' ;;\n  -Rs) printf 'firefox\\t1.0-1\\t262144000\\n' ;;\n  *) exit 1 ;;\nesac\n# {}\n",
            desktop.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&pacman, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!("{}:/usr/bin:/bin", fake_bin.display());
    let output = command()
        .args([
            "uninstall",
            "--app",
            "pacman:firefox",
            "--dry-run",
            "--yes",
            "--json",
        ])
        .env("HOME", home.path())
        .env("PATH", path)
        .env("XDG_STATE_HOME", state.path())
        .env("TUXCLEANER_OS_RELEASE", os_release)
        .env("TUXCLEANER_DESKTOP_DIRS", &desktop_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["applications"][0]["id"], "pacman:firefox");
    assert_eq!(value["results"][0]["dry_run"], true);
    assert!(desktop.exists());

    command()
        .args([
            "uninstall",
            "--app",
            "pacman:not-installed",
            "--dry-run",
            "--yes",
            "--json",
        ])
        .env("HOME", home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("XDG_STATE_HOME", state.path())
        .env("TUXCLEANER_OS_RELEASE", home.path().join("os-release"))
        .env("TUXCLEANER_DESKTOP_DIRS", &desktop_dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "none of the requested application IDs were found",
        ));
}

#[test]
fn analyze_json_is_machine_readable_and_read_only() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("large.bin"), vec![0; 128]).unwrap();

    let output = command()
        .args([
            "analyze",
            root.path().to_str().unwrap(),
            "--min-size",
            "100",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["total_files"], 1);
    assert_eq!(value["large_files"].as_array().unwrap().len(), 1);
    assert!(root.path().join("large.bin").exists());
}

#[test]
fn clean_json_without_yes_only_reports_candidates() {
    let home = tempdir().unwrap();
    let os_release = home.path().join("os-release");
    fs::write(&os_release, "NAME=Test Linux\nID=test\n").unwrap();
    fs::create_dir_all(home.path().join(".cache/pip")).unwrap();
    fs::write(home.path().join(".cache/pip/archive"), vec![0; 64]).unwrap();

    let output = command()
        .args(["clean", "--json"])
        .env("HOME", home.path())
        .env("TUXCLEANER_OS_RELEASE", os_release)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["results"].as_array().unwrap().is_empty());
    assert!(home.path().join(".cache/pip/archive").exists());
}

#[test]
fn purge_dry_run_preserves_selected_artifacts() {
    let home = tempdir().unwrap();
    let state = tempdir().unwrap();
    let os_release = home.path().join("os-release");
    let target = home.path().join("Projects/example/target");
    fs::write(&os_release, "NAME=Test Linux\nID=test\n").unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("binary"), vec![0; 64]).unwrap();

    command()
        .args([
            "purge",
            "--path",
            home.path().join("Projects").to_str().unwrap(),
            "--older-than-days",
            "0",
            "--dry-run",
            "--yes",
        ])
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", state.path())
        .env("TUXCLEANER_OS_RELEASE", os_release)
        .assert()
        .success()
        .stdout(predicate::str::contains("[dry-run]"));

    assert!(target.join("binary").exists());
}
