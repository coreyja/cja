# Changelog

All notable changes to the CJA CLI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial CLI implementation with `cja new` command
- Project scaffolding with configurable features
- Feature flags: `--no-jobs`, `--no-cron`, `--no-sessions`
- Comprehensive template generation system
- Database migration management
- Intelligent feature dependency handling
- Comprehensive documentation with examples
- Full test suite with 26 tests covering all functionality

### Features
- **Project Generation**: Creates complete CJA applications with HTTP server, database integration, and optional background processing
- **Feature Flags**: Opt-out of specific features for minimal deployments
- **Smart Defaults**: All features enabled by default with intelligent dependency handling
- **Database Migrations**: Automatically includes relevant SQL migrations based on enabled features
- **Production Ready**: Generated projects include proper error handling, logging, and configuration

### Generated Project Structure
- `Cargo.toml` with all necessary dependencies
- `src/main.rs` with conditional feature modules
- `migrations/` directory with feature-specific SQL files
- Complete application setup with database connectivity
- Environment-based feature toggling

### Documentation
- Comprehensive API documentation with examples
- User guide with installation and usage instructions
- Real-world examples and use cases
- Troubleshooting guide
- Development and contribution guidelines

### Testing
- 11 CLI command tests
- 15 project generation tests
- Feature flag combination testing
- Error scenario validation
- Template content verification

## [0.0.0] - Initial Development

### Added
- Basic CLI structure with clap
- Test-driven development foundation
- Project scaffolding architecture
- Template generation system
- Database migration handling