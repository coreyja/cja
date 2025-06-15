# CJA CLI

A command-line interface for scaffolding new [CJA](https://github.com/your-org/cja) projects.

CJA CLI generates fully functional web applications built on the CJA framework, with configurable features including background jobs, cron scheduling, and session management.

## Installation

### From Source
```bash
cargo install --path crates/cli
```

### From Cargo (when published)
```bash
cargo install cja-cli
```

## Quick Start

```bash
# Create a full-featured CJA project
cja new my-web-app

# Navigate to your project
cd my-web-app

# Set up your database
export DATABASE_URL="postgres://user:password@localhost/my_web_app"

# Run the application
cargo run
```

Your application will be available at `http://localhost:3000`

## Features

- **🚀 Full-Stack Project Generation**: Creates complete CJA applications with HTTP server, database integration, and optional background processing
- **🔧 Feature Flags**: Opt-out of specific features (`--no-jobs`, `--no-cron`, `--no-sessions`) for minimal deployments
- **🧠 Smart Defaults**: All features enabled by default, with intelligent dependency handling
- **🗄️ Database Migrations**: Automatically includes relevant SQL migrations based on enabled features
- **📦 Production Ready**: Generated projects include proper error handling, logging, and configuration
- **🔒 Type Safety**: Generated code uses Rust's type system for safety and performance

## Usage

### Basic Commands

```bash
# Show help
cja --help

# Show version
cja --version

# Create a new project
cja new <project-name> [FLAGS]
```

### Project Creation Options

#### Create a Full-Featured Application
```bash
cja new ecommerce-site
```

Includes:
- HTTP server with Axum
- PostgreSQL database with migrations
- Background job processing
- Cron scheduling system
- Session management
- HTML templating with Maud
- Structured logging and error handling

#### Create a Microservice
```bash
cja new user-service --no-sessions --no-cron --no-jobs
```

Creates a lightweight HTTP API server without background processing, perfect for:
- REST APIs
- GraphQL services
- Proxy services
- Simple web services

#### Create a Background Worker
```bash
cja new data-processor --no-sessions
```

Creates an application optimized for background job processing with cron scheduling:
- Job queue processing
- Scheduled task execution
- Data processing pipelines
- Batch operations

#### Create a Static Site Server
```bash
cja new blog-site --no-jobs --no-cron
```

Creates a web server with session management but no background processing:
- Content management systems
- User-facing websites
- Authentication-required sites
- Multi-user applications

### Feature Flags

| Flag | Description | Impact |
|------|-------------|--------|
| `--no-jobs` | Disable background job processing | Removes job queue, worker spawning, and job migrations |
| `--no-cron` | Disable cron scheduling | Removes cron worker and cron migrations |
| `--no-sessions` | Disable session management | Removes session handling, database table, and session-aware routes |

### Feature Dependencies

- **Cron requires Jobs**: The `--no-jobs` flag automatically disables cron scheduling since cron depends on the jobs system
- **Independent Sessions**: Session support is independent of jobs and cron
- **HTTP Server**: Always included (core CJA functionality)

When both `--no-jobs` and `--no-cron` are specified, a warning is displayed to inform you that jobs being disabled automatically disables cron.

## Generated Project Structure

```
my-project/
├── Cargo.toml              # Project dependencies and metadata
├── src/
│   └── main.rs             # Application entry point with conditional features
└── migrations/             # Database migrations (feature-dependent)
    ├── 20231210151519_AddJobsTable.sql       # Jobs support (if enabled)
    ├── 20240228040146_AddCrons.up.sql        # Cron scheduling (if enabled)
    ├── 20240228040146_AddCrons.down.sql      # Cron rollback (if enabled)
    ├── 20250413182934_AddSessions.up.sql     # Session management (if enabled)
    └── 20250413182934_AddSessions.down.sql   # Session rollback (if enabled)
```

### Generated Dependencies

All generated projects include these core dependencies:

```toml
[dependencies]
cja = "0.0.0"                # CJA framework core
axum = "0.7"                 # HTTP server framework
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.8", features = ["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
color-eyre = "0.6"
uuid = { version = "1.5", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
maud = { version = "0.26", features = ["axum"] }
futures = "0.3"
async-trait = "0.1"
```

## Environment Variables

Generated projects support these environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | PostgreSQL connection string | **Required** |
| `PORT` | HTTP server port | `3000` |
| `SERVER_DISABLED` | Disable HTTP server | `false` |
| `JOBS_DISABLED` | Disable job worker | `false` |
| `CRON_DISABLED` | Disable cron worker | `false` |

## Database Setup

Generated projects require PostgreSQL. Set up your database:

### Local Development
```bash
# Install PostgreSQL (macOS)
brew install postgresql

# Start PostgreSQL
brew services start postgresql

# Create database
createdb my_project_dev

# Set DATABASE_URL
export DATABASE_URL="postgres://username:password@localhost/my_project_dev"
```

### Docker
```bash
# Run PostgreSQL in Docker
docker run --name postgres -e POSTGRES_PASSWORD=password -d -p 5432:5432 postgres

# Set DATABASE_URL
export DATABASE_URL="postgres://postgres:password@localhost/postgres"
```

## Examples

### E-commerce Application
```bash
cja new shop-backend
cd shop-backend

# The generated application includes:
# - Product catalog endpoints
# - Order processing jobs
# - Inventory update cron jobs
# - User session management
# - Payment processing background tasks

cargo run
```

### API Gateway
```bash
cja new api-gateway --no-sessions --no-jobs --no-cron
cd api-gateway

# Minimal HTTP server for:
# - Request routing
# - Authentication middleware
# - Rate limiting
# - Response transformation

cargo run
```

### Data Pipeline
```bash
cja new data-pipeline --no-sessions
cd data-pipeline

# Background processing system for:
# - ETL job processing
# - Scheduled data imports
# - Report generation
# - Data validation tasks

cargo run
```

## Troubleshooting

### Common Issues

#### Directory Already Exists
```
Error: Directory 'my-project' already exists
```
**Solution**: Choose a different project name or remove the existing directory.

#### Database Connection Failed
```
Error: Failed to connect to database
```
**Solution**: Ensure PostgreSQL is running and `DATABASE_URL` is correctly set.

#### Permission Denied
```
Error: Failed to create project directory
```
**Solution**: Ensure you have write permissions in the current directory.

### Getting Help

- Use `cja --help` for command-line help
- Check the [CJA Documentation](https://github.com/your-org/cja) for framework details
- Open an issue on [GitHub](https://github.com/your-org/cja/issues) for bugs or feature requests

## Development

### Running Tests

```bash
# Run all tests
cargo test

# Run specific test suite
cargo test --test cli_tests
cargo test --test project_generation_tests

# Run with output
cargo test -- --nocapture
```

### Test Coverage

The CLI includes comprehensive test coverage:
- **26 total tests** covering all functionality
- **Command parsing and validation**
- **Project generation and file creation**
- **Feature flag combinations**
- **Error handling scenarios**
- **Template content validation**

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Install locally
cargo install --path .
```

## Contributing

1. Fork the repository
2. Create a feature branch
3. Add tests for new functionality
4. Ensure all tests pass
5. Submit a pull request

## License

This project is licensed under the same terms as the CJA framework.