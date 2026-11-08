//! End-to-end scaffold tests. The `#[ignore]`d test compiles a freshly
//! generated project; run it via the CI `scaffold-check` lane or locally with
//! `cargo test -p cja-cli --test scaffold -- --ignored --nocapture`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

fn generate(project_parent: &Path, flags: &[&str]) -> PathBuf {
    Command::cargo_bin("cja")
        .unwrap()
        .arg("new")
        .arg("myapp")
        .args(flags)
        .current_dir(project_parent)
        .assert()
        .success();
    project_parent.join("myapp")
}

#[test]
fn generated_project_carries_no_cja_migration_files() {
    let dir = assert_fs::TempDir::new().unwrap();
    let project = generate(dir.path(), &[]);

    let migrations = fs::read_dir(project.join("migrations")).unwrap();
    for entry in migrations {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert_eq!(name, ".gitkeep", "unexpected file in migrations/: {name}");
    }
}

#[test]
#[ignore = "compiles generated projects against this checkout (minutes cold); CI lane scaffold-check"]
fn generated_projects_compile() {
    let cja_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cja");
    let target = std::env::temp_dir().join("cja-cli-scaffold-check-target");

    // Every flag combination: the generated Cargo.toml is identical across
    // them, so after the first run each one only re-checks myapp.
    for mask in 0..8u8 {
        let flags: Vec<&str> = [(1u8, "--no-jobs"), (2, "--no-cron"), (4, "--no-sessions")]
            .into_iter()
            .filter(|(bit, _)| mask & bit != 0)
            .map(|(_, flag)| flag)
            .collect();
        let dir = assert_fs::TempDir::new().unwrap();
        let project = generate(dir.path(), &flags);

        // Point the generated cja dependency at this checkout instead of git,
        // so the test compiles against the code in this PR.
        let manifest = project.join("Cargo.toml");
        let toml = fs::read_to_string(&manifest).unwrap();
        let patched = toml
            .lines()
            .map(|line| {
                if line.starts_with("cja = ") {
                    format!(r#"cja = {{ path = "{}" }}"#, cja_path.display())
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&manifest, patched + "\n").unwrap();

        // Scrub the env: an ambient DATABASE_URL would flip cja's one live
        // sqlx query! macro (cron/worker.rs) into live-check mode against a
        // DB that may lack the crons table. Offline mode resolves from this
        // checkout's committed root .sqlx cache. CARGO_BUILD_BUILD_DIR is
        // dropped so intermediates stay under this test's own target dir.
        let output = std::process::Command::new("cargo")
            .args(["check", "--message-format=short"])
            .current_dir(&project)
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_INCREMENTAL", "0")
            .env_remove("DATABASE_URL")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env("SQLX_OFFLINE", "true")
            .output()
            .expect("failed to spawn cargo");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "cargo check failed for flags {flags:?}:\n{stderr}"
        );

        // A scaffold that compiles with warnings (unused import, unused
        // variable in some flag combination) is still broken for its user.
        // Generated-crate paths are relative (`src/main.rs:..`); the cja path
        // dep's are absolute, and vergen's outside-git notes start with
        // `warning:` — neither matches.
        let warnings: Vec<&str> = stderr
            .lines()
            .filter(|line| line.starts_with("src/") && line.contains("warning:"))
            .collect();
        assert!(
            warnings.is_empty(),
            "generated code has warnings for flags {flags:?}:\n{}",
            warnings.join("\n")
        );
    }
}
