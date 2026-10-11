use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

static SPAWN: Mutex<()> = Mutex::new(());

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
#[ignore = "launches nested Cargo builds"]
fn warm_add_ambient_declaration() {
    let _guard = SPAWN.lock().unwrap();
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
    build();
    assert_eq!(
        fs::read_to_string(&marker).unwrap().lines().count(),
        1,
        "unchanged inputs must not rerun the build script"
    );
    fs::write(
        fixture.join("frontend/src/new-ambient.d.ts"),
        "declare const newAmbient: string;\n",
    )
    .unwrap();
    build();
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 2);
    fs::write(
        fixture.join("types/new-external.d.ts"),
        "declare const externalAmbient: string;\n",
    )
    .unwrap();
    build();
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 3);
}

#[test]
#[ignore = "launches nested Cargo builds"]
fn warm_tailwind_rust_source() {
    let _guard = SPAWN.lock().unwrap();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("app");
    copy_tree(
        &manifest.join("tests/fixtures/tailwind-rerun-app"),
        &fixture,
    );
    let cargo_toml = fixture.join("Cargo.toml");
    fs::write(
        &cargo_toml,
        fs::read_to_string(&cargo_toml)
            .unwrap()
            .replace("../../..", &manifest.display().to_string()),
    )
    .unwrap();
    fs::copy(
        manifest.join("../../Cargo.lock"),
        fixture.join("Cargo.lock"),
    )
    .unwrap();
    let marker = temp.path().join("runs");
    let fake = manifest
        .join("tests/fixtures/bin/fake-tailwindcss")
        .canonicalize()
        .unwrap();
    let cargo = env::var("CARGO").unwrap();
    let build = || {
        let result = Command::new(&cargo)
            .args(["build", "--offline", "--manifest-path"])
            .arg(&cargo_toml)
            .env("TAILWIND_CLI", &fake)
            .env("CJA_RERUN_MARKER", &marker)
            .env("CARGO_TARGET_DIR", temp.path().join("target"))
            .env("CARGO_BUILD_BUILD_DIR", temp.path().join("build"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let manifest = temp.path().join("build/debug/build");
        let manifests = fs::read_dir(manifest)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("out/cja_assets.rs"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert_eq!(manifests.len(), 1);
        assert!(fs::read_to_string(&manifests[0]).unwrap().contains("app."));
    };
    build();
    let count = || fs::read_to_string(&marker).unwrap().lines().count();
    assert_eq!(count(), 1);
    build();
    assert_eq!(count(), 1, "no-change control");
    fs::write(
        fixture.join("src/main.rs"),
        "fn main() { let _class = \"md:flex\"; }\n",
    )
    .unwrap();
    build();
    assert_eq!(count(), 2, "Rust class edit must rerun");
    fs::write(
        fixture.join("frontend/extra.css"),
        ".fixture-extra { color: blue; }\n",
    )
    .unwrap();
    build();
    assert_eq!(count(), 3, "imported CSS edit must rerun");
    fs::write(fixture.join("outside.txt"), "unwatched").unwrap();
    build();
    assert_eq!(count(), 3, "outside edit must not rerun");
}
