use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
#[ignore = "launches two nested Cargo builds"]
fn warm_add_ambient_declaration() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("app");
    copy_tree(&manifest.join("tests/fixtures/rerun-app"), &fixture);
    let cargo_toml = fixture.join("Cargo.toml");
    let text = fs::read_to_string(&cargo_toml)
        .unwrap()
        .replace("../../..", &manifest.display().to_string());
    fs::write(&cargo_toml, text).unwrap();
    fs::copy(
        manifest.join("../../Cargo.lock"),
        fixture.join("Cargo.lock"),
    )
    .unwrap();
    let marker = temp.path().join("runs");
    let cargo = env::var("CARGO").expect("Cargo sets CARGO for tests");
    let fake_tsc = manifest
        .join("tests/fixtures/bin/fake-tsc")
        .canonicalize()
        .unwrap();
    let build = || {
        let output = Command::new(&cargo)
            .args(["build", "--offline", "--manifest-path"])
            .arg(&cargo_toml)
            .env("TSC", &fake_tsc)
            .env("CJA_RERUN_MARKER", &marker)
            .env("CARGO_TARGET_DIR", temp.path().join("target"))
            .env("CARGO_BUILD_BUILD_DIR", temp.path().join("build"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    build();
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
    fs::write(
        fixture.join("frontend/src/new-ambient.d.ts"),
        "declare const newAmbient: string;\n",
    )
    .unwrap();
    build();
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 2);
}
