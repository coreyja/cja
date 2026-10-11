use std::{env, fs::OpenOptions, io::Write as _};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    cja_build::AssetsBuilder::new()
        .entry(cja_build::Entry::new("src/main.ts", "studio.js"))
        .build()?;
    let path = env::var("CJA_RERUN_MARKER")?;
    writeln!(OpenOptions::new().create(true).append(true).open(path)?, "run")?;
    Ok(())
}
