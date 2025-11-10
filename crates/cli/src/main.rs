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
//! - **Database Migrations**: Automatically includes relevant SQL migrations based on enabled features
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
//! └── migrations/         # Database migrations (feature-dependent)
//!     ├── *_AddJobsTable.sql      # Jobs support (if enabled)
//!     ├── *_AddCrons.up.sql       # Cron scheduling (if enabled)
//!     └── *_AddSessions.up.sql    # Session management (if enabled)
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
                        .help("Use GitHub version instead of crates.io (defaults to https://github.com/coreyja/cja)")
                        .value_name("REPO")
                        .num_args(0..=1)
                        .default_missing_value("https://github.com/coreyja/cja"),
                )
                .arg(
                    Arg::new("branch")
                        .long("branch")
                        .help("GitHub branch to use (defaults to main)")
                        .value_name("BRANCH")
                        .default_value("main")
                        .requires("github"),
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
                        .help("Use GitHub version instead of crates.io (defaults to https://github.com/coreyja/cja)")
                        .value_name("REPO")
                        .num_args(0..=1)
                        .default_missing_value("https://github.com/coreyja/cja"),
                )
                .arg(
                    Arg::new("branch")
                        .long("branch")
                        .help("GitHub branch to use (defaults to main)")
                        .value_name("BRANCH")
                        .default_value("main")
                        .requires("github"),
                ),
        )
        .subcommand(
            Command::new("sync-migrations")
                .about("Sync migration files from the CJA crate to your project")
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
/// - Database migration copying
/// - Feature-based conditional logic
///
/// # Arguments
///
/// * `project_name` - The name of the project to create (used as directory name and in Cargo.toml)
/// * `no_cron` - If true, excludes cron scheduling functionality and migrations
/// * `no_jobs` - If true, excludes background job processing functionality and migrations
/// * `no_sessions` - If true, excludes session management functionality and migrations
/// * `github_repo` - Optional GitHub repository URL to use instead of crates.io
/// * `branch` - GitHub branch to use (defaults to "main")
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
/// - Migration file copying fails
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
/// // Create a full-featured project from crates.io
/// create_project("my-app", false, false, false, None, "main")?;
///
/// // Create a minimal API server from GitHub
/// create_project("my-api", true, true, true, Some(&"https://github.com/coreyja/cja".to_string()), "main")?;
///
/// // Create a project with jobs but no cron from a custom branch
/// create_project("my-worker", true, false, false, Some(&"https://github.com/coreyja/cja".to_string()), "develop")?;
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
        anyhow::bail!("Directory '{}' already exists", project_name);
    }

    // Create project directory structure
    fs::create_dir(project_path)
        .with_context(|| format!("Failed to create project directory '{project_name}'"))?;

    fs::create_dir(project_path.join("src")).context("Failed to create src directory")?;

    fs::create_dir(project_path.join("migrations"))
        .context("Failed to create migrations directory")?;

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

    // Copy migration files based on feature flags
    if !no_jobs {
        copy_jobs_migration(project_path)?;
        copy_job_error_tracking_migration(project_path)?;
    }

    if !no_cron && !no_jobs {
        copy_cron_migrations(project_path)?;
    }

    if !no_sessions {
        copy_session_migrations(project_path)?;
    }

    println!("Created new CJA project '{project_name}'");

    Ok(())
}

/// Initializes CJA in an existing Rust project.
///
/// This function adds CJA to an existing Cargo project, creating the necessary
/// files and structure while preserving the existing project configuration.
///
/// # Arguments
///
/// * `bin_name` - Optional binary name (defaults to project name from Cargo.toml)
/// * `no_cron` - If true, excludes cron scheduling functionality
/// * `no_jobs` - If true, excludes background job processing functionality
/// * `no_sessions` - If true, excludes session management functionality
/// * `github_repo` - Optional GitHub repository URL to use instead of crates.io
/// * `branch` - GitHub branch to use (defaults to "main")
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

    let toml_value: toml::Value = cargo_toml_content
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
                anyhow::bail!("A binary named '{}' already exists in Cargo.toml", bin_name);
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
    if !migrations_dir.exists() {
        fs::create_dir(migrations_dir).context("Failed to create migrations directory")?;
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

    // Copy migration files based on feature flags
    if !no_jobs {
        copy_jobs_migration(Path::new("."))?;
        copy_job_error_tracking_migration(Path::new("."))?;
    }

    if !no_cron && !no_jobs {
        copy_cron_migrations(Path::new("."))?;
    }

    if !no_sessions {
        copy_session_migrations(Path::new("."))?;
    }

    println!("Successfully initialized CJA in your project!");
    println!("Binary created at: {}", bin_file_path.display());
    println!("\nTo run your CJA application:");
    println!("  cargo run --bin {bin_name}");

    Ok(())
}

// Helper function to add dependencies to a Cargo project
fn add_dependencies(github_repo: Option<&String>, branch: &str) -> Result<()> {
    // Add CJA dependency using cargo add
    println!("Adding CJA dependency...");
    let mut cargo_add_cmd = ProcessCommand::new("cargo");
    cargo_add_cmd.arg("add");

    // Use GitHub dependency if specified
    if let Some(repo) = github_repo {
        cargo_add_cmd.arg("cja"); // Specify the package name from the workspace
        cargo_add_cmd.arg("--git").arg(repo);
        cargo_add_cmd.arg("--branch").arg(branch);
        println!("Using GitHub repository: {repo} (branch: {branch})");
    } else {
        cargo_add_cmd.arg("cja");
    }

    let output = cargo_add_cmd
        .output()
        .context("Failed to execute cargo add command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cargo add failed: {}", stderr);
    }

    // Add other required dependencies
    println!("Adding additional dependencies...");
    let deps = vec![
        ("axum", Some("0.7"), vec![]),
        ("tokio", Some("1"), vec!["full"]),
        (
            "sqlx",
            Some("0.8"),
            vec!["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono"],
        ),
        ("serde", Some("1.0"), vec!["derive"]),
        ("serde_json", Some("1.0"), vec![]),
        ("tracing", Some("0.1"), vec![]),
        ("tracing-subscriber", Some("0.3"), vec!["env-filter"]),
        ("color-eyre", Some("0.6"), vec![]),
        ("uuid", Some("1.5"), vec!["v4", "serde"]),
        ("chrono", Some("0.4"), vec!["serde"]),
        ("maud", Some("0.26"), vec!["axum"]),
        ("futures", Some("0.3"), vec![]),
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
    let toml_check: toml::Value = cargo_toml_content
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
/// - Project metadata (name, version, edition)
/// - Core CJA framework dependency
/// - HTTP server dependencies (Axum, Tokio)
/// - Database dependencies (`SQLx` with `PostgreSQL` support)
/// - Utility dependencies (serde, tracing, error handling)
/// - HTML templating (Maud)
///
/// # Arguments
///
/// * `project_name` - The name to use in the `[package]` section
/// * `github_repo` - Optional GitHub repository URL to use instead of crates.io
/// * `branch` - GitHub branch to use (defaults to "main")
/// * `no_cron` - If true, excludes cron feature
/// * `no_jobs` - If true, excludes jobs feature
/// * `no_sessions` - If true, excludes sessions feature
///
/// # Returns
///
/// A complete `Cargo.toml` file content as a `String`
///
/// # Generated Dependencies
///
/// The function includes these key dependencies:
/// - `cja = "0.0.0"` - Core CJA framework
/// - `axum = "0.7"` - HTTP server framework
/// - `tokio` - Async runtime with full features
/// - `sqlx` - Database toolkit with `PostgreSQL` support
/// - `serde` - Serialization framework
/// - `tracing` - Structured logging
/// - `color-eyre` - Enhanced error reporting
/// - `maud` - Type-safe HTML templating
///
/// # Examples
///
/// ```rust
/// let toml_content = generate_cargo_toml("my-awesome-app");
/// assert!(toml_content.contains("name = \"my-awesome-app\""));
/// assert!(toml_content.contains("cja = { version = \"0.0.0\" }"));
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
    let cja_version = env!("CARGO_PKG_VERSION");
    let cja_dep = if let Some(repo) = github_repo {
        format!(r#"cja = {{ git = "{repo}", branch = "{branch}" }}"#)
    } else {
        format!(r#"cja = {{ version = "{cja_version}" }}"#)
    };

    format!(
        r#"[package]
name = "{project_name}"
version = "0.1.0"
edition = "2021"

[dependencies]
{cja_dep}
axum = "0.7"
tokio = {{ version = "1", features = ["full"] }}
sqlx = {{ version = "0.8", features = ["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono"] }}
serde = {{ version = "1.0", features = ["derive"] }}
serde_json = "1.0"
tracing = "0.1"
tracing-subscriber = {{ version = "0.3", features = ["env-filter"] }}
color-eyre = "0.6"
uuid = {{ version = "1.5", features = ["v4", "serde"] }}
chrono = {{ version = "0.4", features = ["serde"] }}
maud = {{ version = "0.26", features = ["axum"] }}
futures = "0.3"
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
/// - Database connection pooling with advisory locking
/// - Tokio runtime setup and application lifecycle
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
/// - Example `NoopJob` implementation
/// - Job registry macro invocation
/// - Job worker spawning logic
/// - Integration with CJA jobs system
///
/// ### Cron Module (`!no_cron`)
/// - Cron registry setup and configuration
/// - Cron worker spawning
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
fn generate_imports(no_sessions: bool) -> String {
    let mut imports = String::from(
        r"use axum::response::IntoResponse;
use cja::{
    color_eyre::{
        self,
        eyre::{Context as _, eyre},
    },
    server::{
        cookies::CookieKey,
        run_server,
",
    );

    // Session imports only if sessions are enabled
    if !no_sessions {
        imports.push_str("        session::{AppSession, CJASession, Session},\n");
    }

    imports.push_str(
        r"    },
    setup::{setup_sentry, setup_tracing},
};
use maud::html;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tracing::info;
use async_trait::async_trait;
",
    );

    imports
}

fn generate_app_state_impl() -> &'static str {
    r#"
#[derive(Clone)]
struct AppState {
    db: sqlx::PgPool,
    cookie_key: CookieKey,
}

impl AppState {
    async fn from_env() -> color_eyre::Result<Self> {
        let db = setup_db_pool().await?;
        let cookie_key = CookieKey::from_env_or_generate()?;

        Ok(Self { db, cookie_key })
    }
}

impl cja::app_state::AppState for AppState {
    fn version(&self) -> &'static str {
        concat!(
            env!("CARGO_PKG_VERSION"),
            " (",
            env!("VERGEN_GIT_SHA"),
            ")"
        )
    }

    fn db(&self) -> &sqlx::PgPool {
        &self.db
    }

    fn cookie_key(&self) -> &CookieKey {
        &self.cookie_key
    }
}

fn main() -> color_eyre::Result<()> {
    // Initialize Sentry for error tracking
    let _sentry_guard = setup_sentry();

    // Create and run the tokio runtime
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(async { run_application().await })
}

#[tracing::instrument(err)]
pub async fn setup_db_pool() -> cja::Result<PgPool> {
    const MIGRATION_LOCK_ID: i64 = 0xDB_DB_DB_DB_DB_DB_DB;

    let database_url = std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    sqlx::query!("SELECT pg_advisory_lock($1)", MIGRATION_LOCK_ID)
        .execute(&pool)
        .await?;

    sqlx::migrate!().run(&pool).await?;

    let unlock_result = sqlx::query!("SELECT pg_advisory_unlock($1)", MIGRATION_LOCK_ID)
        .fetch_one(&pool)
        .await?
        .pg_advisory_unlock;

    match unlock_result {
        Some(b) => {
            if b {
                tracing::info!("Migration lock unlocked");
            } else {
                tracing::info!("Failed to unlock migration lock");
            }
        }
        None => return Err(eyre!("Failed to unlock migration lock")),
    }

    Ok(pool)
}

async fn run_application() -> cja::Result<()> {
    // Initialize tracing
    setup_tracing("cja-site")?;

    let app_state = AppState::from_env().await?;

    // Spawn application tasks
    info!("Spawning application tasks");
    let futures = spawn_application_tasks(&app_state);

    // Wait for all tasks to complete
    futures::future::try_join_all(futures).await?;

    Ok(())
}

fn routes(app_state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(root))
        .with_state(app_state)
}
"#
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
    async fn from_db(pool: &sqlx::PgPool, session_id: uuid::Uuid) -> cja::Result<Self> {
        let row = sqlx::query!(
            "SELECT session_id, created_at, updated_at FROM sessions WHERE session_id = $1",
            session_id
        )
        .fetch_one(pool)
        .await?;

        let session = SiteSession {
            inner: CJASession {
                session_id: row.session_id,
                created_at: row.created_at,
                updated_at: row.updated_at,
            },
        };

        Ok(session)
    }

    async fn create(pool: &sqlx::PgPool) -> cja::Result<Self> {
        let row = sqlx::query!(
            "INSERT INTO sessions DEFAULT VALUES RETURNING session_id, created_at, updated_at",
        )
        .fetch_one(pool)
        .await?;

        let inner = CJASession {
            session_id: row.session_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
        };

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

fn generate_spawn_tasks(no_jobs: bool, no_cron: bool) -> String {
    let mut tasks = String::from(
        r#"
/// Spawn all application background tasks
fn spawn_application_tasks(
    app_state: &AppState,
) -> std::vec::Vec<tokio::task::JoinHandle<std::result::Result<(), cja::color_eyre::Report>>> {
    let mut futures = vec![];

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        futures.push(tokio::spawn(run_server(routes(app_state.clone()))));
    } else {
        info!("Server Disabled");
    }
"#,
    );

    // Add jobs worker if jobs are enabled
    if !no_jobs {
        tasks.push_str(
            r#"
    // Initialize job worker if enabled
    if is_feature_enabled("JOBS") {
        use std::time::Duration;

        info!("Jobs Enabled");
        futures.push(tokio::spawn(cja::jobs::worker::job_worker(
            app_state.clone(),
            jobs::Jobs,
            Duration::from_secs(60),
            20,
        )));
    } else {
        info!("Jobs Disabled");
    }
"#,
        );
    }

    // Add cron worker if cron is enabled
    if !no_cron {
        tasks.push_str(
            r#"
    // Initialize cron worker if enabled
    if is_feature_enabled("CRON") {
        info!("Cron Enabled");
        futures.push(tokio::spawn(cron::run_cron(app_state.clone())));
    } else {
        info!("Cron Disabled");
    }
"#,
        );
    }

    tasks.push_str(
        r#"
    info!("All application tasks spawned successfully");
    futures
}

/// Check if a feature is enabled based on environment variables
fn is_feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("{feature}_DISABLED");
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}
"#,
    );

    tasks
}

fn generate_jobs_module() -> &'static str {
    r#"
mod jobs {
    use cja::jobs::{Job, JobInfo};
    use serde::{Deserialize, Serialize};

    // Example job
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ExampleJob {
        pub message: String,
    }

    #[async_trait::async_trait]
    impl Job for ExampleJob {
        const NAME: &'static str = "ExampleJob";

        async fn run(&self, _app_state: impl cja::app_state::AppState) -> cja::Result<()> {
            tracing::info!("Running ExampleJob with message: {}", self.message);
            Ok(())
        }
    }

    // Job registry
    #[derive(Clone)]
    pub struct Jobs;

    cja::impl_job_registry!(Jobs => ExampleJob);
}
"#
}

fn generate_cron_module() -> &'static str {
    r#"
mod cron {
    use cja::cron::{Cron, CronRegistry};

    pub async fn run_cron(app_state: impl cja::app_state::AppState) -> cja::Result<()> {
        let mut cron = CronRegistry::new(app_state);

        // Example cron job that runs every hour
        // cron.register_fn(
        //     "hourly_task",
        //     "@hourly",
        //     |app_state| async move {
        //         let job = super::jobs::ExampleJob {
        //             message: "Hourly cron job".to_string(),
        //         };
        //         job.enqueue(&app_state).await?;
        //         Ok(())
        //     }
        // )?;

        cron.run().await
    }
}
"#
}

fn generate_main_rs(no_cron: bool, no_jobs: bool, no_sessions: bool) -> String {
    let mut content = String::new();

    // Add imports
    content.push_str(&generate_imports(no_sessions));

    // Add app state implementation
    content.push_str(generate_app_state_impl());

    // Add session handler
    content.push_str(generate_session_handler(no_sessions));

    // Add spawn tasks function
    content.push_str(&generate_spawn_tasks(no_jobs, no_cron));

    // Add jobs module if jobs are enabled
    if !no_jobs {
        content.push_str(generate_jobs_module());
    }

    // Add cron module if cron is enabled
    if !no_cron && !no_jobs {
        content.push_str(generate_cron_module());
    }

    content
}

/// Copies the jobs table migration file to the new project.
///
/// Creates the SQL migration file for the background jobs system. This migration
/// establishes the core `Jobs` table used by the CJA jobs system for persistent
/// job queuing and execution tracking.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migration
///
/// Creates `migrations/20231210151519_AddJobsTable.sql` with:
/// - `Jobs` table with UUID primary key
/// - Job metadata fields (name, payload, priority)
/// - Scheduling fields (`run_at`, `created_at`)
/// - Locking mechanism fields (`locked_at`, `locked_by`)
/// - Context field for job execution context
///
/// # Database Schema
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS Jobs (
///     job_id UUID PRIMARY KEY NOT NULL,
///     name TEXT NOT NULL,
///     payload JSONB NOT NULL,
///     priority INT NOT NULL,
///     run_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
///     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
///     locked_at TIMESTAMPTZ,
///     locked_by TEXT,
///     context TEXT NOT NULL
/// );
/// ```
fn copy_jobs_migration(project_path: &Path) -> Result<()> {
    let jobs_migration = r"-- Add migration script here
CREATE TABLE
  IF NOT EXISTS Jobs (
    job_id UUID PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    payload JSONB NOT NULL,
    priority INT NOT NULL,
    run_at TIMESTAMPTZ NOT NULL DEFAULT NOW (),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW (),
    locked_at TIMESTAMPTZ,
    locked_by TEXT,
    context TEXT NOT NULL
  );
";

    fs::write(
        project_path
            .join("migrations")
            .join("20231210151519_AddJobsTable.sql"),
        jobs_migration,
    )
    .context("Failed to write jobs migration")?;

    Ok(())
}

/// Copies the cron scheduling migration files to the new project.
///
/// Creates both up and down SQL migration files for the cron scheduling system.
/// These migrations establish the `Crons` table used to track cron job execution
/// and prevent duplicate runs.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migrations
///
/// Creates two migration files:
/// - `migrations/20240228040146_AddCrons.up.sql` - Creates the cron table
/// - `migrations/20240228040146_AddCrons.down.sql` - Removes the cron table
///
/// # Database Schema (Up Migration)
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS Crons (
///     cron_id UUID PRIMARY KEY,
///     name TEXT NOT NULL,
///     last_run_at TIMESTAMP WITH TIME ZONE NOT NULL,
///     created_at TIMESTAMP WITH TIME ZONE NOT NULL,
///     updated_at TIMESTAMP WITH TIME ZONE NOT NULL
/// );
/// CREATE UNIQUE INDEX idx_crons_name ON Crons (name);
/// ```
///
/// # Rollback (Down Migration)
///
/// ```sql
/// DROP TABLE Crons;
/// ```
fn copy_cron_migrations(project_path: &Path) -> Result<()> {
    let cron_up_migration = r"-- Add migration script here
CREATE TABLE
  IF NOT EXISTS Crons (
    cron_id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    last_run_at TIMESTAMP
    WITH
      TIME ZONE NOT NULL,
      created_at TIMESTAMP
    WITH
      TIME ZONE NOT NULL,
      updated_at TIMESTAMP
    WITH
      TIME ZONE NOT NULL
  );

CREATE UNIQUE INDEX idx_crons_name ON Crons (name);
";

    let cron_down_migration = r"-- Add migration script here
DROP TABLE Crons;
";

    fs::write(
        project_path
            .join("migrations")
            .join("20240228040146_AddCrons.up.sql"),
        cron_up_migration,
    )
    .context("Failed to write cron up migration")?;

    fs::write(
        project_path
            .join("migrations")
            .join("20240228040146_AddCrons.down.sql"),
        cron_down_migration,
    )
    .context("Failed to write cron down migration")?;

    Ok(())
}

/// Copies the session management migration files to the new project.
///
/// Creates both up and down SQL migration files for the session management system.
/// These migrations establish the `Sessions` table with automatic timestamp updates
/// and include PostgreSQL-specific trigger functions.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migrations
///
/// Creates two migration files:
/// - `migrations/20250413182934_AddSessions.up.sql` - Creates sessions table and triggers
/// - `migrations/20250413182934_AddSessions.down.sql` - Removes sessions table and functions
///
/// # Database Schema (Up Migration)
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS Sessions (
///     session_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
///     created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
///     updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
/// );
///
/// -- Automatic timestamp update function
/// CREATE OR REPLACE FUNCTION update_updated_at_column()
/// RETURNS TRIGGER AS $$
/// BEGIN
///    NEW.updated_at = NOW();
///    RETURN NEW;
/// END;
/// $$ language 'plpgsql';
///
/// -- Trigger for automatic updates
/// CREATE TRIGGER update_sessions_updated_at
///   BEFORE UPDATE ON sessions
///   FOR EACH ROW
///   EXECUTE FUNCTION update_updated_at_column();
/// ```
///
/// # Rollback (Down Migration)
///
/// ```sql
/// DROP TABLE IF EXISTS Sessions;
/// DROP FUNCTION IF EXISTS update_updated_at_column();
/// ```
///
/// Copies the job error tracking migration file to the new project.
///
/// Creates the SQL migration file for job error tracking. This migration
/// adds error tracking fields to the Jobs table to support retry logic
/// and error reporting.
///
/// # Arguments
///
/// * `project_path` - Path to the project directory where migrations should be created
///
/// # Returns
///
/// - `Ok(())` on successful migration file creation
/// - `Err(anyhow::Error)` if file writing fails
///
/// # Generated Migration
///
/// Creates `migrations/20251109000000_AddJobErrorTracking.sql` with:
/// - `error_count` column to track number of failures
/// - `last_error_message` column to store most recent error message
/// - `last_failed_at` column to store timestamp of most recent failure
///
/// # Database Schema
///
/// ```sql
/// ALTER TABLE Jobs
/// ADD COLUMN IF NOT EXISTS error_count INTEGER NOT NULL DEFAULT 0;
///
/// ALTER TABLE Jobs
/// ADD COLUMN IF NOT EXISTS last_error_message TEXT;
///
/// ALTER TABLE Jobs
/// ADD COLUMN IF NOT EXISTS last_failed_at TIMESTAMPTZ;
/// ```
fn copy_job_error_tracking_migration(project_path: &Path) -> Result<()> {
    let job_error_tracking_migration = r"-- Add error tracking fields to jobs table
-- Add migration script here

-- Add error_count column (tracks number of failures)
ALTER TABLE Jobs
ADD COLUMN IF NOT EXISTS error_count INTEGER NOT NULL DEFAULT 0;

-- Add last_error_message column (stores most recent error message)
ALTER TABLE Jobs
ADD COLUMN IF NOT EXISTS last_error_message TEXT;

-- Add last_failed_at column (stores timestamp of most recent failure)
ALTER TABLE Jobs
ADD COLUMN IF NOT EXISTS last_failed_at TIMESTAMPTZ;
";

    fs::write(
        project_path
            .join("migrations")
            .join("20251109000000_AddJobErrorTracking.sql"),
        job_error_tracking_migration,
    )
    .context("Failed to write job error tracking migration")?;

    Ok(())
}

/// # `PostgreSQL` Features
///
/// - Uses `gen_random_uuid()` for automatic UUID generation
/// - Includes PL/pgSQL trigger function for timestamp updates
/// - Provides proper cleanup in down migration
fn copy_session_migrations(project_path: &Path) -> Result<()> {
    let session_up_migration = r"-- Add migration script here
CREATE TABLE
  IF NOT EXISTS Sessions (
    session_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    created_at TIMESTAMP
    WITH
      TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
      updated_at TIMESTAMP
    WITH
      TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL
  );

-- Create a function to update the updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
   NEW.updated_at = NOW();
   RETURN NEW;
END;
$$ language 'plpgsql';

-- Create a trigger to automatically update the updated_at column
CREATE TRIGGER update_sessions_updated_at
  BEFORE UPDATE ON sessions
  FOR EACH ROW
  EXECUTE FUNCTION update_updated_at_column();
";

    let session_down_migration = r"-- Add migration script here
DROP TABLE IF EXISTS Sessions;

DROP FUNCTION IF EXISTS update_updated_at_column ();
";

    fs::write(
        project_path
            .join("migrations")
            .join("20250413182934_AddSessions.up.sql"),
        session_up_migration,
    )
    .context("Failed to write session up migration")?;

    fs::write(
        project_path
            .join("migrations")
            .join("20250413182934_AddSessions.down.sql"),
        session_down_migration,
    )
    .context("Failed to write session down migration")?;

    Ok(())
}
