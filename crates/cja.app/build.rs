fn main() -> Result<(), cja_build::BuildError> {
    cja_build::AssetsBuilder::new()
        .static_dir("assets")
        .tailwind(cja_build::Tailwind::new("frontend/app.css", "app.css"))
        .entry(cja_build::Entry::new("src/studio.ts", "studio.js"))
        .build()
}
