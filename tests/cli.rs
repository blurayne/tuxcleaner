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
        .stdout(predicate::str::contains("analyze"))
        .stdout(predicate::str::contains("purge"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("history"));
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
