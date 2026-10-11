fn main() -> Result<(), cja_build::BuildError> {
    cja_build::AssetsBuilder::new("assets").build()
}
