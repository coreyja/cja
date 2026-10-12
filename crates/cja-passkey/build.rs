fn main() -> Result<(), cja_build::BuildError> {
    cja_build::AssetsBuilder::new()
        .entry(
            cja_build::Entry::new("src/passkey-client.ts", "passkey-client.js").format(
                cja_build::EntryFormat::Iife {
                    global_name: Some("cjaPasskey".into()),
                },
            ),
        )
        .build()
}
