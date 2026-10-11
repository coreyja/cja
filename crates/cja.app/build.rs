fn main() -> Result<(), cja_build::BuildError> {
    cja_build::AssetsBuilder::new()
        .static_dir("assets")
        .entry(cja_build::Entry::new("src/studio.ts", "studio.js"))
        .build()
}
