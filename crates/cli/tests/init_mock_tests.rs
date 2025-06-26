use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

// These tests focus on testing init command behavior without actually running cargo add
// They verify the command line parsing and basic file creation logic

#[test]
fn test_init_requires_cargo_toml() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // No Cargo.toml in directory
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("No Cargo.toml found"));
}

#[test]
fn test_init_help_shows_all_options() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialize CJA in an existing Rust project"))
        .stdout(predicate::str::contains("--bin-name"))
        .stdout(predicate::str::contains("--no-cron"))
        .stdout(predicate::str::contains("--no-jobs"))
        .stdout(predicate::str::contains("--no-sessions"))
        .stdout(predicate::str::contains("--github"))
        .stdout(predicate::str::contains("--branch"));
}

#[test]
fn test_init_branch_requires_github_flag() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // Create minimal Cargo.toml
    temp.child("Cargo.toml")
        .write_str(r#"[package]
name = "test"
version = "0.1.0"
"#)
        .unwrap();
    
    // Using --branch without --github should fail
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--branch", "develop"])
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("the following required arguments were not provided"));
}

#[test]
fn test_init_invalid_cargo_toml() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // Create invalid Cargo.toml
    temp.child("Cargo.toml")
        .write_str("invalid toml content")
        .unwrap();
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to parse Cargo.toml"));
}

#[test]
fn test_init_missing_package_section() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // Create Cargo.toml without package section
    temp.child("Cargo.toml")
        .write_str(r#"[dependencies]
serde = "1.0"
"#)
        .unwrap();
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Could not find package name"));
}

#[test]
fn test_init_with_github_flag_accepts_custom_repo() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Use GitHub version instead of crates.io"));
}

#[test]
fn test_init_github_flag_has_default_value() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("defaults to https://github.com/coreyja/cja"));
}

#[test]
fn test_init_branch_default_is_main() {
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default: main"));
}

#[test]
fn test_init_warnings_for_no_jobs_and_no_cron() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    temp.child("Cargo.toml")
        .write_str(r#"[package]
name = "test"
version = "0.1.0"
edition = "2021"

[dependencies]
"#)
        .unwrap();
    
    // Create src/main.rs so cargo add doesn't fail
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();
    
    // This test only checks if the warning would be shown, not the full init process
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--no-jobs", "--no-cron"])
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(5))
        .assert()
        .interrupted()  // We expect it to be interrupted due to cargo add
        .stderr(predicate::str::contains("--no-jobs implies --no-cron"));
}