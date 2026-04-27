use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let frontend_dir = manifest_dir.join("frontend");

    println!("cargo:rerun-if-changed=frontend/src/");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/pnpm-lock.yaml");
    println!("cargo:rerun-if-changed=frontend/tsconfig.json");
    println!("cargo:rerun-if-env-changed=CJA_PASSKEY_PREBUILT_JS");
    println!("cargo:rerun-if-env-changed=CJA_PASSKEY_STRICT_FRONTEND_BUILD");

    if let Ok(prebuilt) = env::var("CJA_PASSKEY_PREBUILT_JS")
        && !prebuilt.is_empty()
    {
        let prebuilt_path = PathBuf::from(&prebuilt);
        fs::copy(
            prebuilt_path.join("passkey-client.js"),
            out_dir.join("passkey-client.js"),
        )?;
        return Ok(());
    }

    if !frontend_dir.exists() {
        println!(
            "cargo:warning=cja-passkey frontend/ directory not found. Writing placeholder JS."
        );
        write_placeholder(&out_dir, "frontend directory missing")?;
        return Ok(());
    }

    let install_status = Command::new("pnpm")
        .args(["install", "--frozen-lockfile"])
        .current_dir(&frontend_dir)
        .status();
    let install_ok = matches!(install_status, Ok(s) if s.success());
    let install_ok = install_ok || {
        println!(
            "cargo:warning=Using npm instead of pnpm — dependency versions may differ from lockfile"
        );
        let s = Command::new("npm")
            .args(["install"])
            .current_dir(&frontend_dir)
            .status();
        matches!(s, Ok(s) if s.success())
    };

    if !install_ok {
        if strict_mode() {
            return Err(
                "CJA_PASSKEY_STRICT_FRONTEND_BUILD: Neither pnpm nor npm install succeeded".into(),
            );
        }
        println!("cargo:warning=Neither pnpm nor npm available. Writing placeholder JS.");
        write_placeholder(&out_dir, "pnpm/npm not available")?;
        return Ok(());
    }

    let esbuild_bin = frontend_dir.join("node_modules/.bin/esbuild");
    let outfile_arg = format!("--outfile={}", out_dir.join("passkey-client.js").display());

    let status = Command::new(&esbuild_bin)
        .args([
            "src/passkey-client.ts",
            "--bundle",
            "--format=iife",
            "--global-name=cjaPasskey",
            "--target=es2020",
            &outfile_arg,
        ])
        .current_dir(&frontend_dir)
        .status();

    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            if strict_mode() {
                return Err("CJA_PASSKEY_STRICT_FRONTEND_BUILD: esbuild failed".into());
            }
            println!("cargo:warning=esbuild failed for passkey-client. Writing placeholder JS.");
            write_placeholder(&out_dir, "esbuild failed")?;
            Ok(())
        }
    }
}

fn strict_mode() -> bool {
    env::var("CJA_PASSKEY_STRICT_FRONTEND_BUILD")
        .is_ok_and(|v| !v.is_empty() && v != "0" && v != "false")
}

fn write_placeholder(out_dir: &std::path::Path, reason: &str) -> std::io::Result<()> {
    fs::write(
        out_dir.join("passkey-client.js"),
        format!("console.error('cja-passkey JS not built: {reason}');"),
    )
}
