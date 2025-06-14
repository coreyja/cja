# CJA CLI Development Plan

## Overview
This plan outlines the development of a CLI tool for the CJA web framework that will scaffold new projects with customizable templates. The CLI will follow Test-Driven Development (TDD) principles.

## Goals
- Create a `cja new <project-name>` command for project scaffolding
- Enable cron, jobs, and sessions by default with opt-out flags
- Provide well-tested, reliable project generation
- Use existing CJA app structure as template basis

## Command Structure
```bash
# Basic usage
cja new my-project

# With opt-out flags
cja new my-project --no-cron --no-jobs --no-sessions

# Version info
cja --version
cja -V
```

## Testing Strategy
Using Test-Driven Development with:
- `assert_cmd` for CLI command testing
- `assert_fs` for filesystem operation testing
- `predicates` for assertion helpers

## Task List

### High Priority (Test Writing Phase)
1. ✅ Set up CLI dependencies (clap, fs operations) and test dependencies (assert_cmd, assert_fs)
2. ✅ Write tests for simple version sub-command using assert_cmd
3. 🔄 Write tests for CLI argument parsing and command structure using assert_cmd
4. ⏳ Write tests for project template generation with different feature combinations using assert_fs
5. ⏳ Write tests for file system operations and directory creation using assert_fs
6. ⏳ Write tests for template variable substitution using assert_fs to verify file contents

### Medium Priority (Implementation Phase)
7. ⏳ Design template system for new project scaffolding
8. ⏳ Create project template files (Cargo.toml, main.rs, basic AppState implementation)
9. ⏳ Implement 'cja new <project-name>' command with opt-out flags (--no-cron, --no-jobs, --no-sessions)
10. ⏳ Add template variable substitution (project name, feature flags)
11. ⏳ Create migration template files for new projects
12. ⏳ Create templates with cron/jobs/sessions enabled by default but conditionally included
13. ⏳ Write integration tests using assert_cmd + assert_fs for end-to-end project generation
14. ⏳ Add validation for project names and directory conflicts with tests using assert_fs

### Low Priority (Polish Phase)
15. ⏳ Implement CLI help system and command documentation
16. ⏳ Create example route and handler templates with session examples

## Template Structure
Based on the existing `crates/cja.app/` structure:

### Core Files
- `Cargo.toml` with workspace dependencies
- `src/main.rs` with AppState implementation
- Database migration files
- Basic route handlers

### Feature-Specific Components
- **Jobs**: Job registry and example job implementations
- **Cron**: Cron registry with scheduled tasks
- **Sessions**: Session trait implementation and database schema

### Configuration
- Environment variable setup
- Feature flag handling (SERVER_DISABLED, JOBS_DISABLED, CRON_DISABLED)
- Database connection configuration

## Dependencies Added
```toml
[dependencies]
clap = { version = "4.5", features = ["derive"] }

[dev-dependencies]
assert_cmd = "2.0"
assert_fs = "1.1"
predicates = "3.1"
```

## Current Progress
- ✅ Set up basic CLI structure with clap
- ✅ Added test dependencies
- ✅ Created initial version flag implementation
- ✅ Written basic version flag tests
- ✅ Fixed version flag implementation to use clap's built-in version handling
- 🔄 Currently working on CLI argument parsing tests

## Next Steps
1. Complete CLI argument parsing tests
2. Write comprehensive project generation tests
3. Implement template system based on failing tests
4. Add feature flag handling
5. Create complete project scaffolding functionality

---
*Legend: ✅ Completed, 🔄 In Progress, ⏳ Pending*