use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

#[test]
fn test_new_project_creates_directory() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "test-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    temp.child(project_name).assert(predicate::path::is_dir());
}

#[test]
fn test_new_project_creates_cargo_toml() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "test-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    temp.child(project_name)
        .child("Cargo.toml")
        .assert(predicate::path::exists());
}

#[test]
fn test_new_project_creates_src_directory() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "test-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    temp.child(project_name)
        .child("src")
        .assert(predicate::path::is_dir());
}

#[test]
fn test_new_project_creates_main_rs() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "test-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::path::exists());
}

#[test]
fn test_new_project_creates_migrations_directory() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "test-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    temp.child(project_name)
        .child("migrations")
        .assert(predicate::path::is_dir());
}

#[test]
fn test_project_with_all_features() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "full-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that main.rs contains job registry
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::path::exists())
        .assert(predicate::str::contains("impl_job_registry!"));
    
    // Check that main.rs contains cron registry
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::str::contains("CronRegistry"));
    
    // Check that main.rs contains session implementation
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::str::contains("impl AppSession"));
}

#[test]
fn test_project_without_jobs() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "no-jobs-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name, "--no-jobs"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that main.rs does NOT contain job registry
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::path::exists())
        .assert(predicate::str::contains("impl_job_registry!").not());
}

#[test]
fn test_project_without_cron() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "no-cron-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name, "--no-cron"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that main.rs does NOT contain cron registry
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::path::exists())
        .assert(predicate::str::contains("CronRegistry").not());
}

#[test]
fn test_project_without_sessions() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "no-sessions-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name, "--no-sessions"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that main.rs does NOT contain session implementation
    temp.child(project_name)
        .child("src")
        .child("main.rs")
        .assert(predicate::path::exists())
        .assert(predicate::str::contains("impl AppSession").not());
}

#[test]
fn test_project_fails_if_directory_exists() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "existing-project";
    
    // Create directory first
    temp.child(project_name).create_dir_all().unwrap();
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn test_cargo_toml_contains_project_name() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "my-awesome-project";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    temp.child(project_name)
        .child("Cargo.toml")
        .assert(predicate::str::contains(format!("name = \"{project_name}\"")));
}

#[test]
fn test_creates_proper_migration_files() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "migration-test";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check for jobs migration
    temp.child(project_name)
        .child("migrations")
        .child("20231210151519_AddJobsTable.sql")
        .assert(predicate::path::exists());
    
    // Check for cron migrations
    temp.child(project_name)
        .child("migrations")
        .child("20240228040146_AddCrons.up.sql")
        .assert(predicate::path::exists());
    
    temp.child(project_name)
        .child("migrations")
        .child("20240228040146_AddCrons.down.sql")
        .assert(predicate::path::exists());
    
    // Check for session migrations
    temp.child(project_name)
        .child("migrations")
        .child("20250413182934_AddSessions.up.sql")
        .assert(predicate::path::exists());
    
    temp.child(project_name)
        .child("migrations")
        .child("20250413182934_AddSessions.down.sql")
        .assert(predicate::path::exists());
}

#[test]
fn test_no_jobs_skips_job_migration() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "no-job-migrations";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name, "--no-jobs"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that jobs migration is NOT created
    temp.child(project_name)
        .child("migrations")
        .child("20231210151519_AddJobsTable.sql")
        .assert(predicate::path::exists().not());
}

#[test]
fn test_no_cron_skips_cron_migrations() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "no-cron-migrations";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name, "--no-cron"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that cron migrations are NOT created
    temp.child(project_name)
        .child("migrations")
        .child("20240228040146_AddCrons.up.sql")
        .assert(predicate::path::exists().not());
}

#[test]
fn test_no_sessions_skips_session_migrations() {
    let temp = assert_fs::TempDir::new().unwrap();
    let project_name = "no-session-migrations";
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["new", project_name, "--no-sessions"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that session migrations are NOT created
    temp.child(project_name)
        .child("migrations")
        .child("20250413182934_AddSessions.up.sql")
        .assert(predicate::path::exists().not());
}

#[test]
#[ignore = "Requires local crate installation"]
fn test_init_in_existing_project() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // Create a basic Cargo.toml
    temp.child("Cargo.toml")
        .write_str(r#"[package]
name = "test-existing-project"
version = "0.1.0"
edition = "2021"

[dependencies]
"#)
        .unwrap();
    
    // Create src directory
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .success()
        .stdout(predicate::str::contains("Successfully initialized CJA"));
    
    // Check that src/bin directory was created
    temp.child("src/bin").assert(predicate::path::is_dir());
    
    // Check that binary file was created with default name
    temp.child("src/bin/test-existing-project.rs")
        .assert(predicate::path::exists());
    
    // Check that migrations directory was created
    temp.child("migrations").assert(predicate::path::is_dir());
}

#[test]
#[ignore = "Requires local crate installation"]
fn test_init_with_custom_bin_name() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // Create a basic Cargo.toml
    temp.child("Cargo.toml")
        .write_str(r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[dependencies]
"#)
        .unwrap();
    
    // Create src directory
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--bin-name", "custom-server"])
        .current_dir(&temp)
        .assert()
        .success();
    
    // Check that binary file was created with custom name
    temp.child("src/bin/custom-server.rs")
        .assert(predicate::path::exists());
}

#[test]
#[ignore = "Requires local crate installation"]
fn test_init_fails_if_binary_exists() {
    let temp = assert_fs::TempDir::new().unwrap();
    
    // Create a basic Cargo.toml
    temp.child("Cargo.toml")
        .write_str(r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[dependencies]
"#)
        .unwrap();
    
    // Create src/bin directory and existing binary
    temp.child("src/bin").create_dir_all().unwrap();
    temp.child("src/bin/test-project.rs").write_str("fn main() {}").unwrap();
    
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Binary file"));
}