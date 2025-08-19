use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::str;

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(str::contains("cja"));
}

#[test]
fn test_version_short_flag() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("-V")
        .assert()
        .success()
        .stdout(str::contains("cja"));
}

#[test]
fn test_help_flag() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(str::contains("CJA CLI for project scaffolding"));
}

#[test]
fn test_new_command_without_args() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("new")
        .assert()
        .failure()
        .stderr(str::contains("required"));
}

#[test]
fn test_new_command_with_project_name() {
    let temp = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", "test-project-1"])
        .current_dir(&temp)
        .assert()
        .success();
}

#[test]
fn test_new_command_with_no_cron_flag() {
    let temp = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", "test-project-2", "--no-cron"])
        .current_dir(&temp)
        .assert()
        .success();
}

#[test]
fn test_new_command_with_no_jobs_flag() {
    let temp = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", "test-project-3", "--no-jobs"])
        .current_dir(&temp)
        .assert()
        .success();
}

#[test]
fn test_new_command_with_no_sessions_flag() {
    let temp = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", "test-project-4", "--no-sessions"])
        .current_dir(&temp)
        .assert()
        .success();
}

#[test]
fn test_new_command_with_all_flags() {
    let temp = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args([
        "new",
        "test-project-5",
        "--no-cron",
        "--no-jobs",
        "--no-sessions",
    ])
    .current_dir(&temp)
    .assert()
    .success();
}

#[test]
fn test_new_command_with_both_no_jobs_and_no_cron_shows_warning() {
    let temp = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", "test-project-6", "--no-jobs", "--no-cron"])
        .current_dir(&temp)
        .assert()
        .success()
        .stderr(str::contains("--no-jobs implies --no-cron"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("invalid")
        .assert()
        .failure()
        .stderr(str::contains("unrecognized"));
}

#[test]
fn test_init_command_not_in_cargo_project() {
    let temp_dir = assert_fs::TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.current_dir(&temp_dir)
        .arg("init")
        .assert()
        .failure()
        .stderr(str::contains("No Cargo.toml found"));
}

#[test]
fn test_init_command_help() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(str::contains("Initialize CJA in an existing Rust project"))
        .stdout(str::contains("--github"))
        .stdout(str::contains("--branch"));
}

#[test]
fn test_init_command_github_flag_help() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(str::contains("Use GitHub version instead of crates.io"));
}

#[test]
fn test_init_command_branch_requires_github() {
    let temp_dir = assert_fs::TempDir::new().unwrap();
    temp_dir
        .child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.current_dir(&temp_dir)
        .args(["init", "--branch", "develop"])
        .assert()
        .failure();
}
