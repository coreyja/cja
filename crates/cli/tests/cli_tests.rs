use assert_cmd::Command;
use predicates::str;

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(str::contains("cja"));
}

#[test] 
fn test_version_short_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("-V")
        .assert()
        .success()
        .stdout(str::contains("cja"));
}

#[test]
fn test_help_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(str::contains("CJA CLI for project scaffolding"));
}

#[test]
fn test_new_command_without_args() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("new")
        .assert()
        .failure()
        .stderr(str::contains("required"));
}

#[test]
fn test_new_command_with_project_name() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.args(&["new", "test-project"])
        .assert()
        .success();
}

#[test]
fn test_new_command_with_no_cron_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.args(&["new", "test-project", "--no-cron"])
        .assert()
        .success();
}

#[test]
fn test_new_command_with_no_jobs_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.args(&["new", "test-project", "--no-jobs"])
        .assert()
        .success();
}

#[test]
fn test_new_command_with_no_sessions_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.args(&["new", "test-project", "--no-sessions"])
        .assert()
        .success();
}

#[test]
fn test_new_command_with_all_flags() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.args(&["new", "test-project", "--no-cron", "--no-jobs", "--no-sessions"])
        .assert()
        .success();
}

#[test]
fn test_new_command_with_both_no_jobs_and_no_cron_shows_warning() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.args(&["new", "test-project", "--no-jobs", "--no-cron"])
        .assert()
        .success()
        .stderr(str::contains("--no-jobs implies --no-cron"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("invalid")
        .assert()
        .failure()
        .stderr(str::contains("unrecognized"));
}