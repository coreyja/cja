use clap::Command;

fn main() {
    let _app = Command::new("cja")
        .version(env!("CARGO_PKG_VERSION"))
        .about("CJA CLI for project scaffolding")
        .get_matches();
}