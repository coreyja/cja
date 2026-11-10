//! # CJA CLI
//!
//! A command-line interface for scaffolding new [CJA](https://github.com/coreyja/cja) projects.
//!
//! CJA CLI generates fully functional web applications built on the CJA framework,
//! with configurable features including background jobs, cron scheduling, and session management.
//!
//! ## Features
//!
//! - **Full-Stack Project Generation**: Creates complete CJA applications with HTTP server, database integration, and optional background processing
//! - **Feature Flags**: Opt-out of specific features (`--no-jobs`, `--no-cron`, `--no-sessions`) for minimal deployments
//! - **Smart Defaults**: All features enabled by default, with intelligent dependency handling
//! - **Database Migrations**: Framework migrations run at boot via `cja::db::run_migrations`; no migration files are copied (apps that keep their own copies use `cja sync-migrations`)
//! - **Production Ready**: Generated projects include proper error handling, logging, and configuration
//!
//! ## Quick Start
//!
//! ```bash
//! # Create a full-featured CJA project
//! cja new my-web-app
//!
//! # Create a minimal HTTP server (no background processing)
//! cja new my-api --no-jobs --no-cron --no-sessions
//!
//! # Create a project with jobs but no cron scheduling
//! cja new my-worker --no-cron
//! ```
//!
//! ## Generated Project Structure
//!
//! ```text
//! my-project/
//! ├── Cargo.toml          # Project dependencies and metadata
//! ├── src/
//! │   └── main.rs         # Application entry point with conditional features
//! └── migrations/         # Your app's own migrations — cja's run at boot via `cja::db::run_migrations`
//! ```
//!
//! ## Feature Dependencies
//!
//! - **Cron requires Jobs**: The `--no-jobs` flag automatically disables cron scheduling
//! - **Independent Sessions**: Session support is independent of jobs and cron
//! - **HTTP Server**: Always included (core CJA functionality)
//!
//! ## Examples
//!
//! ### Create a Full-Featured Application
//! ```bash
//! cja new ecommerce-site
//! cd ecommerce-site
//! cargo run
//! ```
//!
//! The generated application includes:
//! - HTTP server with Axum
//! - `PostgreSQL` database with migrations
//! - Background job processing
//! - Cron scheduling system
//! - Session management
//! - HTML templating with Maud
//! - Structured logging and error handling
//!
//! ### Create a Microservice
//! ```bash
//! cja new user-service --no-sessions --no-cron --no-jobs
//! ```
//!
//! Creates a lightweight HTTP API server without background processing.
//!
//! ### Create a Background Worker
//! ```bash
//! cja new data-processor --no-sessions
//! ```
//!
//! Creates an application optimized for background job processing with cron scheduling.

use anyhow::{Context, Result};
use clap::{Arg, Command};
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

/// Main entry point for the CJA CLI application.
///
/// Sets up command-line argument parsing and routes to the appropriate subcommand handler.
/// Currently supports the `new` subcommand for project generation.
///
/// # Returns
///
/// - `Ok(())` on successful command execution
/// - `Err(anyhow::Error)` if command parsing fails or project creation encounters an error
///
/// # Examples
///
/// The CLI supports several usage patterns:
///
/// ```bash
/// # Basic project creation
/// cja new my-project
///
/// # Project with feature flags
/// cja new my-project --no-sessions --no-cron
///
/// # Help and version information
/// cja --help
/// cja --version
/// ```
#[allow(clippy::too_many_lines)]
fn build_cli() -> Command {
    const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("VERGEN_GIT_SHA"), ")");

    Command::new("cja")
        .version(VERSION)
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
                )
                .arg(
                    Arg::new("github")
                        .long("github")
                        .help("Git repository for the cja dependency (default: https://github.com/coreyja/cja)")
                        .value_name("REPO")
                        .num_args(0..=1)
                        .default_missing_value("https://github.com/coreyja/cja"),
                )
                .arg(
                    Arg::new("branch")
                        .long("branch")
                        .help("GitHub branch to use (defaults to main)")
                        .value_name("BRANCH")
                        .default_value("main"),
                ),
        )
        .subcommand(
            Command::new("init")
                .about("Initialize CJA in an existing Rust project")
                .arg(
                    Arg::new("bin-name")
                        .long("bin-name")
                        .help("Name of the binary to create (defaults to project name)")
                        .value_name("NAME"),
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
                )
                .arg(
                    Arg::new("github")
                        .long("github")
                        .help("Git repository for the cja dependency (default: https://github.com/coreyja/cja)")
                        .value_name("REPO")
                        .num_args(0..=1)
                        .default_missing_value("https://github.com/coreyja/cja"),
                )
                .arg(
                    Arg::new("branch")
                        .long("branch")
                        .help("GitHub branch to use (defaults to main)")
                        .value_name("BRANCH")
                        .default_value("main"),
                ),
        )
        .subcommand(
            Command::new("sync-migrations")
                .about("Copy cja's migration files into ./migrations (for apps that keep their own copies; apps generated by `cja new` run them via cja::db::run_migrations instead)")
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .help("Show what would be copied without actually copying")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
}

fn main() -> Result<()> {
    let matches = build_cli().get_matches();

    match matches.subcommand() {
        Some(("new", sub_matches)) => {
            let project_name = sub_matches.get_one::<String>("name").unwrap();
            let no_cron = sub_matches.get_flag("no-cron");
            let no_jobs = sub_matches.get_flag("no-jobs");
            let no_sessions = sub_matches.get_flag("no-sessions");
            let github_repo = sub_matches.get_one::<String>("github");
            let branch = sub_matches
                .get_one::<String>("branch")
                .map_or("main", String::as_str);

            // Warn if both --no-jobs and --no-cron are specified
            if no_jobs && no_cron {
                eprintln!(
                    "Warning: --no-jobs implies --no-cron since cron requires the jobs system"
                );
            }

            create_project(
                project_name,
                no_cron,
                no_jobs,
                no_sessions,
                github_repo,
                branch,
            )?;
        }
        Some(("init", sub_matches)) => {
            let bin_name = sub_matches.get_one::<String>("bin-name");
            let no_cron = sub_matches.get_flag("no-cron");
            let no_jobs = sub_matches.get_flag("no-jobs");
            let no_sessions = sub_matches.get_flag("no-sessions");
            let github_repo = sub_matches.get_one::<String>("github");
            let branch = sub_matches
                .get_one::<String>("branch")
                .map_or("main", String::as_str);

            // Warn if both --no-jobs and --no-cron are specified
            if no_jobs && no_cron {
                eprintln!(
                    "Warning: --no-jobs implies --no-cron since cron requires the jobs system"
                );
            }

            init_project(bin_name, no_cron, no_jobs, no_sessions, github_repo, branch)?;
        }
        Some(("sync-migrations", sub_matches)) => {
            let dry_run = sub_matches.get_flag("dry-run");
            sync_migrations(dry_run)?;
        }
        _ => unreachable!("Subcommand required"),
    }

    Ok(())
}

/// Syncs migration files from the installed CJA crate to the current project.
///
/// This function uses `cargo metadata` to locate the installed CJA crate,
/// finds its migrations directory, and copies all migration files to the
/// current project's migrations directory.
///
/// # Arguments
///
/// * `dry_run` - If true, shows what would be copied without actually copying
///
/// # Returns
///
/// - `Ok(())` on successful sync
/// - `Err(anyhow::Error)` if sync fails
///
/// # Errors
///
/// This function will return an error if:
/// - Not in a Cargo project directory
/// - CJA crate is not found in dependencies
/// - CJA crate's migrations directory doesn't exist
/// - Unable to create migrations directory
/// - File copying fails
fn sync_migrations(dry_run: bool) -> Result<()> {
    // Check if we're in a Cargo project
    let cargo_toml_path = Path::new("Cargo.toml");
    if !cargo_toml_path.exists() {
        anyhow::bail!("No Cargo.toml found. Please run this command in a Rust project directory.");
    }

    // Create migrations directory if it doesn't exist
    let migrations_dir = Path::new("migrations");
    if !migrations_dir.exists() && !dry_run {
        fs::create_dir(migrations_dir).context("Failed to create migrations directory")?;
        println!("Created migrations directory");
    }

    // Use cargo metadata to find the cja crate
    let output = ProcessCommand::new("cargo")
        .args(["metadata", "--format-version=1"])
        .output()
        .context("Failed to run cargo metadata")?;

    if !output.status.success() {
        anyhow::bail!("cargo metadata failed");
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse cargo metadata")?;

    // Find the cja package in the dependency graph
    let packages = metadata["packages"]
        .as_array()
        .context("Invalid metadata format")?;

    let cja_package = packages
        .iter()
        .find(|p| p["name"].as_str() == Some("cja"))
        .context(
            "CJA crate not found in dependencies. Please ensure cja is listed in Cargo.toml",
        )?;

    let cja_manifest_path = cja_package["manifest_path"]
        .as_str()
        .context("Could not find cja manifest path")?;

    // Get the directory containing the manifest
    let cja_dir = Path::new(cja_manifest_path)
        .parent()
        .context("Could not determine cja crate directory")?;

    let cja_migrations_dir = cja_dir.join("migrations");

    if !cja_migrations_dir.exists() {
        anyhow::bail!(
            "CJA migrations directory not found at: {}",
            cja_migrations_dir.display()
        );
    }

    // Read all migration files from the cja crate
    let entries = fs::read_dir(&cja_migrations_dir).with_context(|| {
        format!(
            "Failed to read CJA migrations directory: {}",
            cja_migrations_dir.display()
        )
    })?;

    let mut copied_count = 0;
    let mut skipped_count = 0;

    for entry in entries {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        // Skip if not a file
        if !path.is_file() {
            continue;
        }

        let file_name = path
            .file_name()
            .context("Could not get file name")?
            .to_str()
            .context("File name is not valid UTF-8")?;

        // Only copy .sql files
        if !std::path::Path::new(file_name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
        {
            continue;
        }

        let dest_path = migrations_dir.join(file_name);

        if dest_path.exists() {
            println!("  ⏭️  Skipping {file_name} (already exists)");
            skipped_count += 1;
            continue;
        }

        if dry_run {
            println!("  📋 Would copy: {file_name}");
        } else {
            fs::copy(&path, &dest_path).with_context(|| format!("Failed to copy {file_name}"))?;
            println!("  ✅ Copied: {file_name}");
        }
        copied_count += 1;
    }

    println!("\n🎉 Migration sync complete!");
    if dry_run {
        println!("   Would copy: {copied_count} file(s)");
    } else {
        println!("   Copied: {copied_count} file(s)");
    }
    println!("   Skipped: {skipped_count} file(s) (already exist)");

    if dry_run {
        println!("\nRun without --dry-run to actually copy the files.");
    }

    Ok(())
}

/// Creates a new CJA project with the specified configuration.
///
/// This function orchestrates the complete project creation process, including:
/// - Directory structure creation
/// - Template file generation
/// - Feature-based conditional logic
///
/// The generated app runs cja's framework migrations at boot via
/// `cja::db::run_migrations` (its `migrations/` dir starts empty, reserved
/// for the app's own migrations), and depends on cja from git — `github_repo`
/// customizes the repository and `branch` the branch (both default to the
/// canonical repo's `main`).
///
/// # Arguments
///
/// * `project_name` - The name of the project to create (used as directory name and in Cargo.toml)
/// * `no_cron` - If true, excludes cron scheduling functionality
/// * `no_jobs` - If true, excludes background job processing functionality (implies `no_cron`)
/// * `no_sessions` - If true, excludes session management functionality
/// * `github_repo` - Optional git repository URL for the cja dependency
/// * `branch` - Branch of the cja git dependency (defaults to "main")
///
/// # Returns
///
/// - `Ok(())` on successful project creation
/// - `Err(anyhow::Error)` if any step fails (directory creation, file writing, etc.)
///
/// # Errors
///
/// This function will return an error in the following cases:
/// - A directory with the same name already exists
/// - Insufficient permissions to create directories or files
/// - Template generation fails
///
/// # Feature Dependencies
///
/// - If `no_jobs` is true, cron functionality is automatically disabled since cron depends on jobs
/// - A warning is displayed if both `no_jobs` and `no_cron` are explicitly specified
///
/// # Examples
///
/// ```rust,no_run
/// use anyhow::Result;
///
/// // Create a full-featured project
/// create_project("my-app", false, false, false, None, "main")?;
///
/// // Create a minimal API server
/// create_project("my-api", true, true, true, None, "main")?;
///
/// // Create a project with jobs but no cron from a custom branch
/// create_project("my-worker", true, false, false, None, "develop")?;
/// # Ok::<(), anyhow::Error>(())
/// ```
fn create_project(
    project_name: &str,
    no_cron: bool,
    no_jobs: bool,
    no_sessions: bool,
    github_repo: Option<&String>,
    branch: &str,
) -> Result<()> {
    let project_path = Path::new(project_name);

    // Check if directory already exists
    if project_path.exists() {
        anyhow::bail!("Directory '{project_name}' already exists");
    }

    // Create project directory structure
    fs::create_dir(project_path)
        .with_context(|| format!("Failed to create project directory '{project_name}'"))?;

    fs::create_dir(project_path.join("src")).context("Failed to create src directory")?;

    fs::create_dir(project_path.join("migrations"))
        .context("Failed to create migrations directory")?;
    fs::write(project_path.join("migrations").join(".gitkeep"), "")
        .context("Failed to write migrations/.gitkeep")?;

    // Create Cargo.toml
    let cargo_toml_content = generate_cargo_toml(
        project_name,
        github_repo,
        branch,
        no_cron,
        no_jobs,
        no_sessions,
    );
    fs::write(project_path.join("Cargo.toml"), cargo_toml_content)
        .context("Failed to write Cargo.toml")?;

    // Create main.rs
    let main_rs_content = generate_main_rs(no_cron, no_jobs, no_sessions);
    fs::write(project_path.join("src").join("main.rs"), main_rs_content)
        .context("Failed to write main.rs")?;

    // Create build.rs
    let build_rs_content = generate_build_rs();
    fs::write(project_path.join("build.rs"), build_rs_content)
        .context("Failed to write build.rs")?;

    println!("Created new CJA project '{project_name}'");
    println!("\nTo run it:");
    println!("  cd {project_name}");
    println!("  export DATABASE_URL=postgres:///...   # any PostgreSQL database");
    println!("  cargo run");
    println!(
        "\ncja and your app share the `_sqlx_migrations` table; if you use the \
         sqlx CLI on this database, pass `--ignore-missing`."
    );

    Ok(())
}

/// Initializes CJA in an existing Rust project.
///
/// This function adds CJA to an existing Cargo project, creating the necessary
/// files and structure while preserving the existing project configuration.
/// The generated boot path runs cja's framework migrations at boot via
/// `cja::db::run_migrations` (the `migrations/` dir starts empty, reserved
/// for the app's own migrations), and adds cja as a git dependency —
/// `github_repo` customizes the repository and `branch` the branch (both
/// default to the canonical repo's `main`).
///
/// # Arguments
///
/// * `bin_name` - Optional binary name (defaults to project name from Cargo.toml)
/// * `no_cron` - If true, excludes cron scheduling functionality
/// * `no_jobs` - If true, excludes background job processing functionality (implies `no_cron`)
/// * `no_sessions` - If true, excludes session management functionality
/// * `github_repo` - Optional git repository URL for the cja dependency
/// * `branch` - Branch of the cja git dependency (defaults to "main")
///
/// # Returns
///
/// - `Ok(())` on successful initialization
/// - `Err(anyhow::Error)` if initialization fails
///
/// # Errors
///
/// This function will return an error if:
/// - Not in a Cargo project directory
/// - Unable to read or parse Cargo.toml
/// - Unable to create necessary directories or files
fn init_project(
    bin_name: Option<&String>,
    no_cron: bool,
    no_jobs: bool,
    no_sessions: bool,
    github_repo: Option<&String>,
    branch: &str,
) -> Result<()> {
    // Check if we're in a Cargo project
    let cargo_toml_path = Path::new("Cargo.toml");
    if !cargo_toml_path.exists() {
        anyhow::bail!("No Cargo.toml found. Please run this command in a Rust project directory.");
    }

    // Read and parse Cargo.toml to get project name
    let cargo_toml_content =
        fs::read_to_string(cargo_toml_path).context("Failed to read Cargo.toml")?;

    let toml_value: toml::Table = cargo_toml_content
        .parse()
        .context("Failed to parse Cargo.toml")?;

    let package_name = toml_value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or_else(|| anyhow::anyhow!("Could not find package name in Cargo.toml"))?;

    let bin_name = bin_name.map_or(package_name, std::string::String::as_str);

    // Check if a binary with this name already exists in Cargo.toml
    if let Some(bins) = toml_value.get("bin").and_then(|b| b.as_array()) {
        for bin in bins {
            if let Some(name) = bin.get("name").and_then(|n| n.as_str())
                && name == bin_name
            {
                anyhow::bail!("A binary named '{bin_name}' already exists in Cargo.toml");
            }
        }
    }

    // Create src/bin directory if it doesn't exist
    let bin_dir = Path::new("src").join("bin");
    if !bin_dir.exists() {
        fs::create_dir_all(&bin_dir).context("Failed to create src/bin directory")?;
    }

    // Check if the binary file already exists
    let bin_file_path = bin_dir.join(format!("{bin_name}.rs"));
    if bin_file_path.exists() {
        anyhow::bail!("Binary file '{}' already exists", bin_file_path.display());
    }

    // Create migrations directory if it doesn't exist
    let migrations_dir = Path::new("migrations");
    if migrations_dir.exists() {
        // A pre-existing migrations dir is kept as-is (init never edits the
        // app's own migrations), but warn about the sharing hazard.
        let has_sql_files = fs::read_dir(migrations_dir)
            .context("Failed to read migrations directory")?
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "sql"));
        if has_sql_files {
            eprintln!(
                "Warning: migrations/ already holds .sql files. Once the CJA binary first \
                 boots, cja's rows are in `_sqlx_migrations` and any OTHER migrator on this \
                 database fails with `VersionMissing`: the project's existing binary running \
                 plain `sqlx::migrate!().run()`, or `sqlx migrate run` without \
                 `--ignore-missing`. Those migrators need `set_ignore_missing(true)` or \
                 `--ignore-missing`."
            );
        }
    } else {
        fs::create_dir(migrations_dir).context("Failed to create migrations directory")?;
        fs::write(migrations_dir.join(".gitkeep"), "")
            .context("Failed to write migrations/.gitkeep")?;
    }

    // Add dependencies
    add_dependencies(github_repo, branch)?;

    // Update Cargo.toml with [[bin]] section
    update_cargo_toml_bin(bin_name)?;

    // Create the binary file
    let main_content = generate_main_rs(no_cron, no_jobs, no_sessions);
    fs::write(&bin_file_path, main_content)
        .with_context(|| format!("Failed to write {}", bin_file_path.display()))?;

    // Create build.rs if it doesn't exist
    let build_rs_path = Path::new("build.rs");
    if !build_rs_path.exists() {
        let build_rs_content = generate_build_rs();
        fs::write(build_rs_path, build_rs_content).context("Failed to write build.rs")?;
    }

    println!("Successfully initialized CJA in your project!");
    println!("Binary created at: {}", bin_file_path.display());
    println!("\nTo run your CJA application:");
    println!("  cargo run --bin {bin_name}");

    Ok(())
}

// Helper function to add dependencies to a Cargo project
fn add_dependencies(github_repo: Option<&String>, branch: &str) -> Result<()> {
    // Add CJA dependency using cargo add. There is no usable crates.io
    // release (the one 2024 publish cannot compile the templates), so the
    // dependency is always a git one; --branch overrides the default repo's
    // branch without --github.
    println!("Adding CJA dependency...");
    let repo = github_repo.map_or("https://github.com/coreyja/cja", String::as_str);
    let mut cargo_add_cmd = ProcessCommand::new("cargo");
    cargo_add_cmd.arg("add");
    cargo_add_cmd.arg("cja");
    cargo_add_cmd.arg("--git").arg(repo);
    cargo_add_cmd.arg("--branch").arg(branch);
    println!("Using git repository: {repo} (branch: {branch})");

    let output = cargo_add_cmd
        .output()
        .context("Failed to execute cargo add command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cargo add failed: {stderr}");
    }

    // Add other required dependencies
    println!("Adding additional dependencies...");
    let deps = vec![
        ("axum", Some("0.8"), vec![]),
        ("tokio", Some("1"), vec!["full"]),
        (
            "sqlx",
            Some("0.8"),
            vec![
                "runtime-tokio-rustls",
                "postgres",
                "uuid",
                "json",
                "chrono",
                "migrate",
            ],
        ),
        ("serde", Some("1.0"), vec!["derive"]),
        ("tracing", Some("0.1"), vec![]),
        ("uuid", Some("1"), vec!["v4", "serde"]),
        ("maud", Some("0.27"), vec!["axum"]),
        ("async-trait", Some("0.1"), vec![]),
    ];

    for (dep, version, features) in deps {
        let mut cmd = ProcessCommand::new("cargo");
        cmd.arg("add");

        // Use name@version syntax
        if let Some(v) = version {
            cmd.arg(format!("{dep}@{v}"));
        } else {
            cmd.arg(dep);
        }

        if !features.is_empty() {
            cmd.arg("-F").arg(features.join(","));
        }

        let output = cmd
            .output()
            .with_context(|| format!("Failed to add dependency: {dep}"))?;

        if !output.status.success() {
            eprintln!(
                "Warning: Failed to add {}: {}",
                dep,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // Add vergen as a build dependency
    println!("Adding vergen build dependency...");
    let mut vergen_cmd = ProcessCommand::new("cargo");
    vergen_cmd
        .arg("add")
        .arg("--build")
        .arg("vergen@8")
        .arg("-F")
        .arg("build,git,gitcl");

    let output = vergen_cmd
        .output()
        .context("Failed to add vergen build dependency")?;

    if !output.status.success() {
        eprintln!(
            "Warning: Failed to add vergen: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Helper function to update Cargo.toml with [[bin]] section
fn update_cargo_toml_bin(bin_name: &str) -> Result<()> {
    let cargo_toml_path = Path::new("Cargo.toml");
    let cargo_toml_content =
        fs::read_to_string(cargo_toml_path).context("Failed to read Cargo.toml")?;

    // Parse the Cargo.toml to check for existing [[bin]] sections
    let toml_check: toml::Table = cargo_toml_content
        .parse()
        .context("Failed to parse updated Cargo.toml")?;

    let mut bin_exists = false;
    if let Some(bins) = toml_check.get("bin").and_then(|b| b.as_array()) {
        for bin in bins {
            if let Some(name) = bin.get("name").and_then(|n| n.as_str())
                && name == bin_name
            {
                bin_exists = true;
                break;
            }
        }
    }

    // Add [[bin]] section if it doesn't exist
    if !bin_exists {
        let mut updated_cargo_toml = cargo_toml_content;
        use std::fmt::Write;
        write!(
            updated_cargo_toml,
            r#"

[[bin]]
name = "{bin_name}"
path = "src/bin/{bin_name}.rs"
"#
        )
        .unwrap();

        fs::write(cargo_toml_path, updated_cargo_toml).context("Failed to update Cargo.toml")?;
    }

    Ok(())
}

/// Generates the `Cargo.toml` content for a new CJA project.
///
/// Creates a complete `Cargo.toml` file with all necessary dependencies for a CJA application.
/// The generated configuration includes:
/// - Project metadata (name, version, edition 2024)
/// - Core CJA framework dependency (git, branch-based)
/// - HTTP server dependencies (Axum 0.8, Tokio)
/// - Database dependencies (`SQLx` with `PostgreSQL` support and `migrate`)
/// - Utility dependencies (serde, tracing, uuid)
/// - HTML templating (Maud 0.27)
///
/// # Arguments
///
/// * `project_name` - The name to use in the `[package]` section
/// * `github_repo` - Optional GitHub repository URL (defaults to <https://github.com/coreyja/cja>)
/// * `branch` - Git branch to use (defaults to "main")
/// * `no_cron` - If true, excludes cron feature
/// * `no_jobs` - If true, excludes jobs feature
/// * `no_sessions` - If true, excludes sessions feature
///
/// # Returns
///
/// A complete `Cargo.toml` file content as a `String`
///
/// # Examples
///
/// ```rust
/// let toml_content = generate_cargo_toml("my-awesome-app", None, "main", false, false, false);
/// assert!(toml_content.contains("name = \"my-awesome-app\""));
/// assert!(toml_content.contains("cja = { git = \"https://github.com/coreyja/cja\", branch = \"main\" }"));
/// ```
fn generate_cargo_toml(
    project_name: &str,
    github_repo: Option<&String>,
    branch: &str,
    _no_cron: bool,
    _no_jobs: bool,
    _no_sessions: bool,
) -> String {
    // Build CJA dependency line
    let repo = github_repo.map_or("https://github.com/coreyja/cja", String::as_str);
    let cja_dep = format!(r#"cja = {{ git = "{repo}", branch = "{branch}" }}"#);

    format!(
        r#"[package]
name = "{project_name}"
version = "0.1.0"
edition = "2024"

[dependencies]
{cja_dep}
axum = "0.8"
tokio = {{ version = "1", features = ["full"] }}
sqlx = {{ version = "0.8", features = ["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono", "migrate"] }}
serde = {{ version = "1.0", features = ["derive"] }}
tracing = "0.1"
uuid = {{ version = "1", features = ["v4", "serde"] }}
maud = {{ version = "0.27", features = ["axum"] }}
async-trait = "0.1"

[build-dependencies]
vergen = {{ version = "8", features = ["build", "git", "gitcl"] }}
"#
    )
}

/// Generates the `build.rs` content for a new CJA project.
///
/// Creates a build script that uses vergen to embed git and build information
/// into the compiled binary. This allows the application to report its version
/// including the git commit hash, similar to how the CJA CLI itself works.
///
/// # Returns
///
/// The content of the build.rs file as a String.
///
/// # Generated Code
///
/// The build script:
/// - Uses vergen's `EmitBuilder` to capture build and git metadata
/// - Emits environment variables like `VERGEN_GIT_SHA` that can be used at runtime
/// - Follows the same pattern as the CJA CLI's own build.rs
fn generate_build_rs() -> String {
    r"use vergen::EmitBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    EmitBuilder::builder()
        .all_build()
        .all_git()
        .emit()?;

    Ok(())
}
"
    .to_string()
}

/// Generates the `main.rs` content for a new CJA project with conditional features.
///
/// This is the most complex template generation function, creating a complete Rust application
/// entry point that conditionally includes modules and functionality based on feature flags.
///
/// # Arguments
///
/// * `no_cron` - If true, excludes cron scheduling module and worker spawning
/// * `no_jobs` - If true, excludes background job module and worker spawning
/// * `no_sessions` - If true, excludes session management and uses simple route handlers
///
/// # Returns
///
/// A complete `main.rs` file content as a `String` with conditional compilation
///
/// # Generated Structure
///
/// The function generates these components:
///
/// ## Always Included
/// - Standard imports and `AppState` struct
/// - Database connection pooling with framework migrations run at boot via `cja::db::run_migrations`
/// - Tokio runtime setup and application lifecycle via `cja::tasks::Supervisor`
/// - HTTP server with basic routing
/// - Environment-based feature toggling
///
/// ## Conditionally Included
///
/// ### Sessions Module (`!no_sessions`)
/// - `SiteSession` struct implementing `AppSession` trait
/// - Database operations for session creation and retrieval
/// - Session-aware route handlers with HTML templating
/// - Session information display in web interface
///
/// ### Jobs Module (`!no_jobs`)
/// - Example `ExampleJob` implementation
/// - Job registry macro invocation
/// - Job worker supervised task
/// - Integration with CJA jobs system
///
/// ### Cron Module (`!no_cron`)
/// - Cron registry setup and configuration
/// - Cron worker supervised task
/// - Integration with jobs system (if available)
/// - Scheduled task execution
///
/// # Feature Interactions
///
/// - **Sessions**: Independent of other features
/// - **Jobs + Cron**: Cron integrates with jobs when both are enabled
/// - **Cron Only**: Cron can run without specific job types when jobs are disabled
///
/// # Generated Code Examples
///
/// With all features enabled:
/// ```rust,ignore
/// // Includes session imports
/// use cja::server::session::{AppSession, CJASession, Session};
///
/// // Session-aware route handler
/// async fn root(Session(session): Session<SiteSession>) -> impl IntoResponse {
///     // HTML with session display
/// }
///
/// // Background modules
/// mod jobs { /* Job implementations */ }
/// mod cron { /* Cron scheduling */ }
/// ```
///
/// With minimal features:
/// ```rust,ignore
/// // Simple route handler
/// async fn root() -> impl IntoResponse {
///     // Basic HTML response
/// }
/// // No background modules
/// ```
///
/// # Examples
///
/// ```rust
/// // Generate full-featured application
/// let full_app = generate_main_rs(false, false, false);
/// assert!(full_app.contains("impl AppSession"));
/// assert!(full_app.contains("impl_job_registry!"));
/// assert!(full_app.contains("CronRegistry"));
///
/// // Generate minimal application
/// let minimal_app = generate_main_rs(true, true, true);
/// assert!(!minimal_app.contains("Session"));
/// assert!(!minimal_app.contains("jobs::"));
/// assert!(!minimal_app.contains("cron::"));
/// ```
fn generate_imports(no_sessions: bool, no_jobs: bool) -> String {
    let mut imports = String::from(
        r"use axum::response::IntoResponse;
use cja::{
    color_eyre::eyre::Context as _,
    server::{
        cookies::CookieKey,
        run_server_until,
",
    );

    // Session imports only if sessions are enabled
    if !no_sessions {
        imports.push_str("        session::{AppSession, CJASession, Session},\n");
    }

    imports.push_str(
        r"    },
    setup::setup_tracing,
    tasks::{ShutdownBudget, Supervisor},
};
use maud::html;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::info;
",
    );

    // Only the jobs spawn uses Duration at the top level; the cron module
    // imports its own.
    if !no_jobs {
        imports.push_str("use std::time::Duration;\n");
    }

    imports
}

fn generate_app_skeleton(no_sessions: bool) -> String {
    let cookie_key = if no_sessions {
        r"        // No sessions in this app: an ephemeral key is fine (release
        // builds without COOKIE_KEY refuse when deriving from the env).
        let cookie_key = CookieKey::generate();
"
    } else {
        r"        let cookie_key = CookieKey::from_env_or_generate()?;
"
    };
    format!(
        r#"
#[derive(Clone)]
struct AppState {{
    db: sqlx::PgPool,
    cookie_key: CookieKey,
}}

impl AppState {{
    async fn from_env() -> cja::Result<Self> {{
        let db = setup_db_pool().await?;
{cookie_key}
        Ok(Self {{ db, cookie_key }})
    }}
}}

impl cja::app_state::AppState for AppState {{
    fn version(&self) -> &'static str {{
        concat!(
            env!("CARGO_PKG_VERSION"),
            " (",
            env!("VERGEN_GIT_SHA"),
            ")"
        )
    }}

    fn db(&self) -> &sqlx::PgPool {{
        &self.db
    }}

    fn cookie_key(&self) -> &CookieKey {{
        &self.cookie_key
    }}
}}

fn main() -> cja::Result<()> {{
    // Create and run the tokio runtime
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(async {{ run_application().await }})
}}

#[tracing::instrument(err)]
pub async fn setup_db_pool() -> cja::Result<PgPool> {{
    let database_url = std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    // cja's framework tables: jobs, dead_letter_jobs, crons, cron_pauses, sessions.
    cja::db::run_migrations(&pool).await?;

    // This app's own migrations, from `migrations/` at the crate root. Both
    // migrators share the `_sqlx_migrations` table, so each ignores versions
    // it does not know (checksums are still verified for shared versions).
    //
    // NOTE: `sqlx::migrate!()`'s embed goes stale on incremental rebuild
    // (cargo doesn't rerun-if-changed the migrations dir) — if a newly added
    // migration isn't picked up, `cargo clean -p <app>` or touch main.rs.
    let mut migrations = sqlx::migrate!();
    migrations.set_ignore_missing(true);
    migrations.run(&pool).await?;

    Ok(pool)
}}

fn routes(app_state: AppState) -> axum::Router {{
    axum::Router::new()
        .route("/", axum::routing::get(root))
        .with_state(app_state)
}}

/// Check if a feature is enabled based on environment variables
fn is_feature_enabled(feature: &str) -> bool {{
    let env_var_name = format!("{{feature}}_DISABLED");
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}}
"#
    )
}

fn generate_session_handler(no_sessions: bool) -> &'static str {
    if no_sessions {
        // Simple root handler without sessions
        r#"
async fn root() -> impl IntoResponse {
    html! {
        html {
            head {
                title { "CJA Site" }
            }
            body {
                h1 { "Hello, World!" }
            }
        }
    }
}
"#
    } else {
        r#"
struct SiteSession {
    inner: CJASession,
}

#[async_trait::async_trait]
impl AppSession for SiteSession {
    async fn from_db(
        pool: &sqlx::PgPool,
        session_id: uuid::Uuid,
    ) -> cja::Result<Option<Self>> {
        let inner = sqlx::query_as::<_, CJASession>(
            "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await?;

        Ok(inner.map(|inner| Self { inner }))
    }

    async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
        let inner = sqlx::query_as::<_, CJASession>(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at",
        )
        .fetch_one(pool)
        .await?;

        Ok(Self { inner })
    }

    fn from_inner(inner: CJASession) -> Self {
        Self { inner }
    }

    fn inner(&self) -> &CJASession {
        &self.inner
    }
}

async fn root(Session(session): Session<SiteSession>) -> impl IntoResponse {
    html! {
        html {
            head {
                title { "CJA Site" }
            }
            body {
                h1 { "Hello, World!" }

                h4 { "Session" }
                p { "Session ID: " (session.session_id()) }
                p { "Created At: " (session.created_at()) }
                p { "Updated At: " (session.updated_at()) }
            }
        }
    }
}
"#
    }
}

fn generate_run_application(no_jobs: bool, no_cron: bool) -> String {
    let mut tasks = String::from(
        r#"
async fn run_application() -> cja::Result<()> {
    let _eyes_handle = setup_tracing(env!("CARGO_CRATE_NAME"))?;

    let app_state = AppState::from_env().await?;

    // Registers SIGTERM/SIGINT and owns the shutdown sequence.
    let mut supervisor = Supervisor::new(ShutdownBudget::from_env())?;
    let shutdown = supervisor.shutdown_token();

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        supervisor.spawn(
            "server",
            run_server_until(routes(app_state.clone()), shutdown.clone().cancelled_owned()),
        );
    } else {
        info!("Server Disabled");
    }
"#,
    );

    // Add jobs worker if jobs are enabled
    if !no_jobs {
        tasks.push_str(
            r#"
    if is_feature_enabled("JOBS") {
        info!("Jobs Enabled");
        supervisor.spawn(
            "jobs",
            cja::jobs::worker::job_worker(
                app_state.clone(),
                jobs::Jobs,
                Duration::from_mins(1),
                cja::jobs::DEFAULT_MAX_RETRIES,
                shutdown.clone(),
                Default::default(),
            ),
        );
    } else {
        info!("Jobs Disabled");
    }
"#,
        );
    }

    // Add cron worker if cron is enabled (cron requires jobs, so --no-jobs
    // omits this block along with the cron module)
    if !no_cron && !no_jobs {
        tasks.push_str(
            r#"
    if is_feature_enabled("CRON") {
        info!("Cron Enabled");
        supervisor.spawn("cron", cron::run(app_state.clone(), shutdown));
    } else {
        info!("Cron Disabled");
    }
"#,
        );
    }

    tasks.push_str(
        r"
    supervisor.run().await
}
",
    );

    tasks
}

fn generate_jobs_module() -> &'static str {
    r#"
mod jobs {
    use cja::jobs::Job;
    use serde::{Deserialize, Serialize};

    /// Example job. With cron enabled, the cron module below enqueues it
    /// every minute.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ExampleJob {
        pub message: String,
    }

    #[async_trait::async_trait]
    impl Job<crate::AppState> for ExampleJob {
        const NAME: &'static str = "ExampleJob";

        async fn run(&self, _app_state: crate::AppState) -> cja::Result<()> {
            tracing::info!("Running ExampleJob: {}", self.message);
            Ok(())
        }
    }

    // Expands to `pub struct Jobs;` implementing
    // cja::jobs::registry::JobRegistry<crate::AppState>.
    cja::impl_job_registry!(crate::AppState, ExampleJob);
}
"#
}

fn generate_cron_module(no_sessions: bool) -> String {
    let prune = if no_sessions {
        String::new()
    } else {
        r#"
        // Keep the sessions table bounded: delete sessions idle 30+ days,
        // daily at 4am. Log-and-continue: a failed prune must not stop the
        // cron worker.
        registry
            .register_with_cron(
                "prune-sessions",
                Some("Delete sessions idle for 30+ days"),
                "0 0 4 * * * *", // 7-field cja cron: sec min hour dom mon dow year
                |state, _| {
                    Box::pin(async move {
                        // Field access, not state.db(): the AppState *trait*
                        // is not in scope in this module and the generated
                        // struct has no inherent db(). The private field is
                        // visible from this child module.
                        if let Err(err) = cja::server::session::prune(
                            &state.db,
                            Duration::from_secs(30 * 24 * 3600),
                        )
                        .await
                        {
                            tracing::error!("prune-sessions failed: {err:#}");
                        }
                        Ok::<(), std::convert::Infallible>(())
                    })
                },
            )?;

"#
        .to_string()
    };
    format!(
        r#"
mod cron {{
    use std::time::Duration;

    use cja::cron::{{CancellationToken, CronRegistry, Worker}};

    use crate::AppState;

    fn build_registry() -> cja::Result<CronRegistry<AppState>> {{
        let mut registry = CronRegistry::new();

        // Enqueue the example job once a minute. Interval crons fire once at
        // boot (first tick has no last_run) — expected, not a misconfiguration.
        // See the cja::cron docs for cron-expression schedules
        // (register_job_with_cron) and plain callbacks (register / register_with_cron).
        registry.register_job(
            crate::jobs::ExampleJob {{
                message: "hello from cron".to_string(),
            }},
            Some("Example cron"),
            Duration::from_mins(1),
        );
{prune}        Ok(registry)
    }}

    pub async fn run(app_state: AppState, shutdown: CancellationToken) -> cja::Result<()> {{
        Worker::new(app_state, build_registry()?).run(shutdown).await;
        Ok(())
    }}
}}
"#
    )
}

fn generate_main_rs(no_cron: bool, no_jobs: bool, no_sessions: bool) -> String {
    let mut content = String::new();

    // Add imports
    content.push_str(&generate_imports(no_sessions, no_jobs));

    // Add app skeleton (AppState, main, setup_db_pool, routes, feature flag)
    content.push_str(&generate_app_skeleton(no_sessions));

    // Add session handler
    content.push_str(generate_session_handler(no_sessions));

    // Add run_application (supervised server/jobs/cron tasks)
    content.push_str(&generate_run_application(no_jobs, no_cron));

    // Add jobs module if jobs are enabled
    if !no_jobs {
        content.push_str(generate_jobs_module());
    }

    // Add cron module if cron is enabled
    if !no_cron && !no_jobs {
        content.push_str(&generate_cron_module(no_sessions));
    }

    content
}

#[cfg(test)]
mod tests {
    use super::{generate_cargo_toml, generate_main_rs};

    #[test]
    fn generated_boot_path_uses_cja_migrations() {
        for (no_cron, no_jobs, no_sessions) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let main_rs = generate_main_rs(no_cron, no_jobs, no_sessions);
            assert!(
                main_rs.contains("cja::db::run_migrations(&pool)"),
                "{no_cron}/{no_jobs}/{no_sessions}"
            );
            assert!(
                main_rs.contains("set_ignore_missing(true)"),
                "app migrator must tolerate cja's rows: {no_cron}/{no_jobs}/{no_sessions}"
            );
            assert!(!main_rs.contains("color_eyre::Result"));
            assert!(!main_rs.contains("pg_advisory_lock"));
            assert!(!main_rs.contains("sqlx::query!")); // runtime SQL only: compiles without a DB
            assert!(!main_rs.contains("tokio::spawn")); // everything goes through the Supervisor
            // Sessions path derives the cookie key from COOKIE_KEY (release
            // builds refuse without it); --no-sessions uses an ephemeral key.
            if no_sessions {
                assert!(
                    main_rs.contains("CookieKey::generate()"),
                    "no-sessions scaffold must generate an ephemeral key: \
                     {no_cron}/{no_jobs}/{no_sessions}"
                );
                assert!(
                    !main_rs.contains("from_env_or_generate"),
                    "no-sessions scaffold must not require COOKIE_KEY: \
                     {no_cron}/{no_jobs}/{no_sessions}"
                );
            } else {
                assert!(
                    main_rs.contains("CookieKey::from_env_or_generate()?"),
                    "sessions scaffold must derive the key from COOKIE_KEY: \
                     {no_cron}/{no_jobs}/{no_sessions}"
                );
            }
        }
    }

    #[test]
    fn full_featured_wiring_matches_current_api() {
        let main_rs = generate_main_rs(false, false, false);
        assert!(main_rs.contains("Supervisor::new(ShutdownBudget::from_env())"));
        assert!(main_rs.contains("cja::jobs::worker::job_worker("));
        assert!(main_rs.contains("cja::impl_job_registry!(crate::AppState, ExampleJob)"));
        assert!(main_rs.contains("impl Job<crate::AppState> for ExampleJob"));
        assert!(main_rs.contains("CronRegistry::new()"));
        assert!(main_rs.contains("fn build_registry() -> cja::Result<CronRegistry<AppState>>"));
        assert!(main_rs.contains("Worker::new(app_state, build_registry()?)"));
        assert!(main_rs.contains("\"prune-sessions\""));
        assert!(!main_rs.contains("TODO"));
    }

    #[test]
    fn cargo_toml_defaults_to_git_dependency() {
        let toml = generate_cargo_toml("myapp", None, "main", false, false, false);
        assert!(
            toml.contains(r#"cja = { git = "https://github.com/coreyja/cja", branch = "main" }"#)
        );
        assert!(toml.contains(
            r#"sqlx = { version = "0.8", features = ["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono", "migrate"] }"#
        ));

        // --branch without --github overrides the default repo's branch
        let custom = generate_cargo_toml("myapp", None, "dev", false, false, false);
        assert!(
            custom.contains(r#"cja = { git = "https://github.com/coreyja/cja", branch = "dev" }"#)
        );
    }
}
