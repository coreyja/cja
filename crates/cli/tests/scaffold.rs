//! End-to-end scaffold tests. The `#[ignore]`d test compiles freshly
//! generated/initialized projects; run it via the CI `scaffold-check` lane or
//! locally with `cargo test -p cja-cli --test scaffold -- --ignored --nocapture`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use assert_fs::prelude::*;

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

    let mut names: Vec<String> = fs::read_dir(project.join("migrations"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![".gitkeep".to_string()],
        "migrations/ must contain exactly .gitkeep"
    );
}

/// Rewrite the generated `cja = ...` dependency to a path dep on this
/// checkout, so the compile runs against the code in this PR. Fails unless
/// exactly one `cja = ` line was replaced: if the manifest format drifts, the
/// test must not silently build against git main instead.
fn patch_cja_dep_to_checkout(manifest: &Path, cja_path: &Path) {
    let toml = fs::read_to_string(manifest).unwrap();
    let mut patched = Vec::with_capacity(toml.lines().count());
    let mut replaced = 0;
    for line in toml.lines() {
        if line.starts_with("cja = ") {
            replaced += 1;
            patched.push(format!(r#"cja = {{ path = "{}" }}"#, cja_path.display()));
        } else {
            patched.push(line.to_string());
        }
    }
    assert_eq!(
        replaced, 1,
        "expected exactly one `cja = ` dependency line to patch, found {replaced}; manifest:\n{toml}"
    );
    fs::write(manifest, patched.join("\n") + "\n").unwrap();
}

/// `cargo check` a generated/initialized project. The env is the point:
/// `DATABASE_URL` points at an unreachable host with `SQLX_OFFLINE` forced
/// off, so any compile-time `query!` macro anywhere in the dependency tree
/// would go live-mode and fail to connect. A generated app must compile
/// against a database that doesn't have (and here can't even be reached by)
/// cja's tables, because its boot path creates them at runtime — the exact
/// situation a user hits on `cja new myapp && export DATABASE_URL=... &&
/// cargo run` against a fresh database.
///
/// Returns `(success, stderr)` so the canary in `generated_projects_compile`
/// can reuse the same env and gate for its positive control.
fn run_cargo_check(project: &Path) -> (bool, String) {
    let target = std::env::temp_dir().join("cja-cli-scaffold-check-target");
    let output = std::process::Command::new("cargo")
        .args(["check", "--message-format=short"])
        .current_dir(project)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TERM_COLOR", "never")
        .env(
            "DATABASE_URL",
            "postgres://127.0.0.1:1/cja_scaffold_check_unreachable",
        )
        // Set explicitly, not removed: sqlx-macros falls back to
        // <manifest>/.env when the var is unset, so a developer's local .env
        // could otherwise flip this gate offline.
        .env("SQLX_OFFLINE", "false")
        .env_remove("SQLX_OFFLINE_DIR")
        // Dropped so the generated project's intermediates stay under this
        // test's own target dir rather than a shared build dir.
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .output()
        .expect("failed to spawn cargo");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Stderr lines that indicate a warning in *generated* code. Warnings show
/// up three ways in short format: rustc diagnostics for the crate's own
/// files (`src/...`, `build.rs...` — relative paths; the cja path dep's are
/// absolute), and cargo's per-package summary line for the generated
/// package (``warning: `myapp` (bin "myapp") generated N warnings``) as a
/// backstop for warnings attributed to another path. vergen's outside-git
/// `cargo:warning` notes (`warning: myapp@0.1.0: ...`) match none of these —
/// deliberately, they are build-host noise.
///
/// Kept as a named function because it is the warning gate: the canary in
/// `generated_projects_compile` proves it still catches a planted warning.
fn generated_code_warnings<'a>(stderr: &'a str, package: &str) -> Vec<&'a str> {
    let package_summary = format!("`{package}`");
    stderr
        .lines()
        .filter(|line| {
            (line.starts_with("src/") || line.starts_with("build.rs")) && line.contains("warning:")
                || (line.contains(package_summary.as_str())
                    && line.contains("generated")
                    && line.contains("warning"))
        })
        .collect()
}

fn cargo_check_generated(project: &Path, package: &str, what: &str) {
    let (success, stderr) = run_cargo_check(project);
    assert!(success, "cargo check failed for {what}:\n{stderr}");

    // A scaffold that compiles with warnings (unused import, unused variable
    // in some flag combination) is still broken for its user. Full stderr on
    // failure: the matched lines alone can hide why the gate misfired.
    let warnings = generated_code_warnings(&stderr, package);
    assert!(
        warnings.is_empty(),
        "generated code has warnings for {what}:\nfull stderr:\n{stderr}"
    );
}

#[test]
#[ignore = "compiles generated projects against this checkout (minutes cold); CI lane scaffold-check"]
fn generated_projects_compile() {
    let cja_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cja");

    // Every `cja new` flag combination: the generated Cargo.toml is identical
    // across them, so after the first run each one only re-checks myapp.
    for mask in 0..8u8 {
        let flags: Vec<&str> = [(1u8, "--no-jobs"), (2, "--no-cron"), (4, "--no-sessions")]
            .into_iter()
            .filter(|(bit, _)| mask & bit != 0)
            .map(|(_, flag)| flag)
            .collect();
        let dir = assert_fs::TempDir::new().unwrap();
        let project = generate(dir.path(), &flags);

        patch_cja_dep_to_checkout(&project.join("Cargo.toml"), &cja_path);
        cargo_check_generated(&project, "myapp", &format!("cja new {flags:?}"));
    }

    // `cja init` into a minimal edition-2021 binary crate, so the init path's
    // generated code is compile-checked too. `init` shells out to real
    // `cargo add` invocations, so this needs network — fine in CI.
    let dir = assert_fs::TempDir::new().unwrap();
    dir.child("Cargo.toml")
        .write_str(
            r#"[package]
name = "myinitapp"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
        )
        .unwrap();
    dir.child("src").create_dir_all().unwrap();
    dir.child("src/main.rs").write_str("fn main() {}").unwrap();

    Command::cargo_bin("cja")
        .unwrap()
        .arg("init")
        .current_dir(dir.path())
        .timeout(Duration::from_mins(5))
        .assert()
        .success();

    patch_cja_dep_to_checkout(dir.child("Cargo.toml").path(), &cja_path);
    cargo_check_generated(dir.path(), "myinitapp", "cja init");

    // Positive control for the warning gate (same test so CI's "1 passed"
    // grep holds): plant a warning in a default-generated project and assert
    // the filter lights up. Without this, a gate regression (rustc output
    // format change, path prefix change) would silently turn the gate into a
    // no-op that passes everything.
    let dir = assert_fs::TempDir::new().unwrap();
    let project = generate(dir.path(), &[]);
    patch_cja_dep_to_checkout(&project.join("Cargo.toml"), &cja_path);

    let main_rs = project.join("src/main.rs");
    let src = fs::read_to_string(&main_rs).unwrap();
    fs::write(
        &main_rs,
        format!("{src}\nfn canary() {{\n    let unused = 1;\n}}\n"),
    )
    .unwrap();

    let (success, stderr) = run_cargo_check(&project);
    assert!(success, "canary project must still compile:\n{stderr}");
    let warnings = generated_code_warnings(&stderr, "myapp");
    assert!(
        !warnings.is_empty(),
        "warning gate is blind: a planted unused variable in src/main.rs \
         produced no matching lines, so the gate no longer detects \
         generated-code warnings.\nfull stderr:\n{stderr}"
    );
}
