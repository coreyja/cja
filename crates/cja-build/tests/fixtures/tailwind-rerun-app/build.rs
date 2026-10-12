use std::{env, fs::OpenOptions, io::Write as _};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    cja_build::AssetsBuilder::new()
        .tailwind(cja_build::Tailwind::new("frontend/app.css", "app.css"))
        .build()?;
    writeln!(OpenOptions::new().create(true).append(true).open(env::var("CJA_RERUN_MARKER")?)?, "run")?;
    Ok(())
}
