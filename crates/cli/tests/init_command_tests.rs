use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;
use std::fs;

#[test]
fn test_init_command_in_valid_project() {
    let temp = assert_fs::TempDir::new().unwrap();

    // Create a basic Cargo project structure
    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
        )
        .unwrap();

    // Create src directory
    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs")
        .write_str("fn main() { println!(\"Hello, world!\"); }")
        .unwrap();

    // Run init command
    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success()
        .stdout(predicate::str::contains("Successfully initialized CJA"))
        .stdout(predicate::str::contains("cargo run --bin test-project"));

    // Verify directory structure
    temp.child("src/bin").assert(predicate::path::is_dir());
    temp.child("src/bin/test-project.rs")
        .assert(predicate::path::exists());
    temp.child("migrations").assert(predicate::path::is_dir());
    temp.child("build.rs").assert(predicate::path::exists());

    // Verify migrations were created
    temp.child("migrations/20231210151519_AddJobsTable.sql")
        .assert(predicate::path::exists());
    temp.child("migrations/20240228040146_AddCrons.up.sql")
        .assert(predicate::path::exists());
    temp.child("migrations/20240228040146_AddCrons.down.sql")
        .assert(predicate::path::exists());
    temp.child("migrations/20250413182934_AddSessions.up.sql")
        .assert(predicate::path::exists());
    temp.child("migrations/20250413182934_AddSessions.down.sql")
        .assert(predicate::path::exists());

    // Verify Cargo.toml was updated with [[bin]] section
    let cargo_content = fs::read_to_string(temp.child("Cargo.toml").path()).unwrap();
    assert!(cargo_content.contains("[[bin]]"));
    assert!(cargo_content.contains("name = \"test-project\""));
    assert!(cargo_content.contains("path = \"src/bin/test-project.rs\""));
}

#[test]
fn test_init_with_custom_bin_name() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "my-package"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/lib.rs")
        .write_str("pub fn hello() {}")
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--bin-name", "web-server"])
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Binary created at: src/bin/web-server.rs",
        ))
        .stdout(predicate::str::contains("cargo run --bin web-server"));

    // Verify custom binary was created
    temp.child("src/bin/web-server.rs")
        .assert(predicate::path::exists());

    // Verify Cargo.toml has correct bin entry
    let cargo_content = fs::read_to_string(temp.child("Cargo.toml").path()).unwrap();
    assert!(cargo_content.contains("name = \"web-server\""));
    assert!(cargo_content.contains("path = \"src/bin/web-server.rs\""));
}

#[test]
fn test_init_with_no_jobs_flag() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--no-jobs"])
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify jobs migration was NOT created
    temp.child("migrations/20231210151519_AddJobsTable.sql")
        .assert(predicate::path::exists().not());

    // Verify cron migrations were also NOT created (since cron depends on jobs)
    temp.child("migrations/20240228040146_AddCrons.up.sql")
        .assert(predicate::path::exists().not());

    // Verify binary doesn't contain jobs code
    let binary_content = fs::read_to_string(temp.child("src/bin/test-project.rs").path()).unwrap();
    assert!(!binary_content.contains("mod jobs"));
    assert!(!binary_content.contains("impl_job_registry!"));
}

#[test]
fn test_init_with_no_cron_flag() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--no-cron"])
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify cron migrations were NOT created
    temp.child("migrations/20240228040146_AddCrons.up.sql")
        .assert(predicate::path::exists().not());
    temp.child("migrations/20240228040146_AddCrons.down.sql")
        .assert(predicate::path::exists().not());

    // But jobs migration should still exist
    temp.child("migrations/20231210151519_AddJobsTable.sql")
        .assert(predicate::path::exists());

    // Verify binary doesn't contain cron code
    let binary_content = fs::read_to_string(temp.child("src/bin/test-project.rs").path()).unwrap();
    assert!(!binary_content.contains("mod cron"));
    assert!(!binary_content.contains("CronRegistry"));
}

#[test]
fn test_init_with_no_sessions_flag() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--no-sessions"])
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify session migrations were NOT created
    temp.child("migrations/20250413182934_AddSessions.up.sql")
        .assert(predicate::path::exists().not());
    temp.child("migrations/20250413182934_AddSessions.down.sql")
        .assert(predicate::path::exists().not());

    // Verify binary doesn't contain session code
    let binary_content = fs::read_to_string(temp.child("src/bin/test-project.rs").path()).unwrap();
    assert!(!binary_content.contains("impl AppSession"));
    assert!(!binary_content.contains("struct SiteSession"));
}

#[test]
fn test_init_with_all_no_flags() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.args(["init", "--no-jobs", "--no-cron", "--no-sessions"])
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success()
        .stderr(predicate::str::contains("--no-jobs implies --no-cron"));

    // Verify NO migrations were created
    let migrations_dir = temp.child("migrations");
    migrations_dir.assert(predicate::path::is_dir());

    // Check that the directory is empty or only has minimal files
    let entries = fs::read_dir(migrations_dir.path()).unwrap();
    let count = entries.count();
    assert_eq!(count, 0, "Expected no migration files but found {count}");
}

#[test]
fn test_init_fails_if_binary_already_exists() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    // Create existing binary
    temp.child("src/bin").create_dir_all().unwrap();
    temp.child("src/bin/test-project.rs")
        .write_str("fn main() {}")
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Binary file 'src/bin/test-project.rs' already exists",
        ));
}

#[test]
fn test_init_creates_build_rs_if_missing() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    // Ensure build.rs doesn't exist
    temp.child("build.rs")
        .assert(predicate::path::exists().not());

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify build.rs was created
    temp.child("build.rs").assert(predicate::path::exists());

    // Verify build.rs content
    let build_content = fs::read_to_string(temp.child("build.rs").path()).unwrap();
    assert!(build_content.contains("vergen::EmitBuilder"));
    assert!(build_content.contains("all_build()"));
    assert!(build_content.contains("all_git()"));
}

#[test]
fn test_init_preserves_existing_build_rs() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    // Create existing build.rs with custom content
    let custom_build_content = "// Custom build script\nfn main() { println!(\"Custom build\"); }";
    temp.child("build.rs")
        .write_str(custom_build_content)
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify build.rs still has original content
    let build_content = fs::read_to_string(temp.child("build.rs").path()).unwrap();
    assert_eq!(build_content, custom_build_content);
}

#[test]
fn test_init_workspace_member() {
    let temp = assert_fs::TempDir::new().unwrap();

    // Create a workspace root
    temp.child("Cargo.toml")
        .write_str(
            r#"[workspace]
members = ["my-app"]
"#,
        )
        .unwrap();

    // Create member project
    temp.child("my-app").create_dir_all().unwrap();
    temp.child("my-app/Cargo.toml")
        .write_str(
            r#"[package]
name = "my-app"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("my-app/src").create_dir_all().unwrap();
    temp.child("my-app/src/main.rs")
        .write_str("fn main() {}")
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(temp.child("my-app").path())
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify files were created in the member directory
    temp.child("my-app/src/bin/my-app.rs")
        .assert(predicate::path::exists());
    temp.child("my-app/migrations")
        .assert(predicate::path::is_dir());
}

#[test]
fn test_init_handles_missing_package_name_gracefully() {
    let temp = assert_fs::TempDir::new().unwrap();

    // Create invalid Cargo.toml without package section
    temp.child("Cargo.toml")
        .write_str(
            r#"[dependencies]
serde = "1.0"
"#,
        )
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Could not find package name in Cargo.toml",
        ));
}

#[test]
fn test_init_creates_migrations_dir_if_missing() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    // Ensure migrations directory doesn't exist
    temp.child("migrations")
        .assert(predicate::path::exists().not());

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify migrations directory was created
    temp.child("migrations").assert(predicate::path::is_dir());
}

#[test]
fn test_init_preserves_existing_migrations_dir() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    // Create existing migrations directory with a file
    temp.child("migrations").create_dir_all().unwrap();
    temp.child("migrations/existing.sql")
        .write_str("-- Existing migration")
        .unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify existing migration is preserved
    temp.child("migrations/existing.sql")
        .assert(predicate::path::exists());

    // And new migrations are added
    temp.child("migrations/20231210151519_AddJobsTable.sql")
        .assert(predicate::path::exists());
}

#[test]
fn test_init_updates_cargo_toml_correctly() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"

[dev-dependencies]
test-case = "3.0"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Read updated Cargo.toml
    let cargo_content = fs::read_to_string(temp.child("Cargo.toml").path()).unwrap();

    // Verify original content is preserved (cargo may reformat dependencies)
    assert!(cargo_content.contains("[dependencies]"));
    assert!(cargo_content.contains("serde")); // Cargo may reformat this
    assert!(cargo_content.contains("[dev-dependencies]"));
    assert!(cargo_content.contains("test-case"));

    // Verify new bin section is added
    assert!(cargo_content.contains("[[bin]]"));
    assert!(cargo_content.contains("name = \"test-project\""));
    assert!(cargo_content.contains("path = \"src/bin/test-project.rs\""));
}

#[test]
fn test_init_skips_duplicate_bin_entry() {
    let temp = assert_fs::TempDir::new().unwrap();

    temp.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "test-project"
path = "src/bin/old-binary.rs"
"#,
        )
        .unwrap();

    temp.child("src").create_dir_all().unwrap();
    temp.child("src/main.rs").write_str("fn main() {}").unwrap();

    let mut cmd = Command::cargo_bin("cja").unwrap();
    cmd.arg("init")
        .current_dir(&temp)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "A binary named 'test-project' already exists in Cargo.toml",
        ));
}
