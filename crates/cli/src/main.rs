use clap::{Arg, Command};

fn main() {
    let matches = Command::new("cja")
        .version(env!("CARGO_PKG_VERSION"))
        .about("CJA CLI for project scaffolding")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("new")
                .about("Create a new CJA project")
                .arg(
                    Arg::new("name")
                        .help("The name of the project to create")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::new("no-cron")
                        .long("no-cron")
                        .help("Create project without cron support")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no-jobs")
                        .long("no-jobs")
                        .help("Create project without jobs support")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no-sessions")
                        .long("no-sessions")
                        .help("Create project without sessions support")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .get_matches();

    match matches.subcommand() {
        Some(("new", sub_matches)) => {
            let project_name = sub_matches.get_one::<String>("name").unwrap();
            let _no_cron = sub_matches.get_flag("no-cron");
            let _no_jobs = sub_matches.get_flag("no-jobs");
            let _no_sessions = sub_matches.get_flag("no-sessions");
            
            // TODO: Implement project creation logic
            println!("Creating project: {project_name}");
        }
        _ => unreachable!("Subcommand required"),
    }
}