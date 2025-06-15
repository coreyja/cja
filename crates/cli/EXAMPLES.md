# CJA CLI Examples

This document provides detailed examples of using the CJA CLI to create different types of applications.

## Table of Contents

1. [Basic Usage](#basic-usage)
2. [Web Applications](#web-applications)
3. [API Services](#api-services)
4. [Background Workers](#background-workers)
5. [Advanced Configurations](#advanced-configurations)
6. [Real-World Scenarios](#real-world-scenarios)

## Basic Usage

### Your First CJA Project

```bash
# Create a new project
cja new hello-world

# Navigate to the project
cd hello-world

# Set up environment
export DATABASE_URL="postgres://localhost/hello_world_dev"

# Create the database
createdb hello_world_dev

# Run the application
cargo run
```

Visit `http://localhost:3000` to see your application running.

## Web Applications

### Full-Featured Web Application

```bash
cja new my-blog
```

**Generated Features:**
- HTTP server with routing
- Session management for user authentication
- Background job processing for email notifications
- Cron jobs for maintenance tasks
- Database with full migration support

**Use Cases:**
- Blogs and content management
- E-commerce platforms
- Social media applications
- Multi-user dashboards

### Minimal Web Server

```bash
cja new simple-site --no-jobs --no-cron --no-sessions
```

**Generated Features:**
- HTTP server only
- Basic routing and responses
- Minimal dependencies

**Use Cases:**
- Static file servers
- Simple landing pages
- Health check endpoints
- Webhook receivers

### Session-Based Web App

```bash
cja new user-portal --no-jobs --no-cron
```

**Generated Features:**
- HTTP server with routing
- User session management
- Database with session storage
- No background processing

**Use Cases:**
- User dashboards
- Authentication-required sites
- Profile management systems
- Admin panels

## API Services

### REST API Service

```bash
cja new user-api --no-sessions --no-cron --no-jobs
```

**Generated Features:**
- HTTP server with Axum
- JSON request/response handling
- Database connectivity
- Minimal overhead

**Example Application:**
```rust
// In your generated main.rs, add routes like:
.route("/users", get(list_users).post(create_user))
.route("/users/:id", get(get_user).put(update_user).delete(delete_user))
```

**Use Cases:**
- Microservices
- API gateways
- Data access layers
- Third-party integrations

### GraphQL API

```bash
cja new graphql-api --no-sessions --no-cron --no-jobs
```

Add GraphQL dependencies to `Cargo.toml`:
```toml
async-graphql = "7.0"
async-graphql-axum = "7.0"
```

**Use Cases:**
- Complex data APIs
- Mobile backend services
- Federated GraphQL gateways

### WebSocket Service

```bash
cja new chat-server --no-cron --no-jobs
```

Add WebSocket dependencies to `Cargo.toml`:
```toml
axum = { version = "0.7", features = ["ws"] }
tokio-tungstenite = "0.20"
```

**Use Cases:**
- Real-time chat applications
- Live data streaming
- Gaming backends
- Collaborative tools

## Background Workers

### Job Processing Service

```bash
cja new job-processor --no-sessions
```

**Generated Features:**
- Background job queue processing
- Cron scheduling for periodic tasks
- Database for job persistence
- No HTTP endpoints

**Use Cases:**
- Email processing
- Image/video processing
- Data transformation
- Report generation

### Pure Background Worker

```bash
cja new worker-service --no-sessions --no-cron
```

**Generated Features:**
- Job queue processing only
- Database connectivity
- No HTTP server
- No scheduled tasks

**Use Cases:**
- Queue workers
- Event processors
- Data migration tools

### Cron Service

```bash
cja new scheduler --no-sessions
```

**Generated Features:**
- Scheduled task execution
- Job queue for task persistence
- Cron-style scheduling
- Monitoring and logging

**Use Cases:**
- Backup services
- Cleanup tasks
- Data synchronization
- Health checks

## Advanced Configurations

### Multi-Service Architecture

Create different services for different purposes:

```bash
# API Gateway
cja new api-gateway --no-sessions --no-jobs --no-cron

# User Service
cja new user-service --no-cron --no-jobs

# Background Processor
cja new job-processor --no-sessions

# Scheduled Tasks
cja new scheduler --no-sessions
```

### Development vs Production

#### Development Setup
```bash
cja new dev-app
cd dev-app

# Use local PostgreSQL
export DATABASE_URL="postgres://localhost/dev_app_dev"
export PORT=3000

cargo run
```

#### Production Setup
```bash
# Same project, different environment
export DATABASE_URL="postgres://prod-host/app_prod"
export PORT=8080
export JOBS_DISABLED=false
export CRON_DISABLED=false
export SERVER_DISABLED=false

cargo run --release
```

## Real-World Scenarios

### E-commerce Platform

```bash
# Main web application
cja new shop-web
cd shop-web

# Add e-commerce specific dependencies
cat >> Cargo.toml << EOF
stripe = "0.25"
serde_json = "1.0"
reqwest = { version = "0.11", features = ["json"] }
EOF
```

**Features Used:**
- **Sessions**: User authentication and shopping carts
- **Jobs**: Order processing, payment handling, email notifications
- **Cron**: Inventory updates, abandoned cart cleanup, sales reports
- **HTTP**: Product catalog, checkout flow, admin interface

### SaaS Application

```bash
# Multi-tenant SaaS platform
cja new saas-platform
cd saas-platform

# Add SaaS specific dependencies
cat >> Cargo.toml << EOF
jsonwebtoken = "8.3"
lettre = "0.10"
redis = "0.23"
EOF
```

**Features Used:**
- **Sessions**: User accounts and tenant isolation
- **Jobs**: User onboarding, subscription billing, data exports
- **Cron**: Usage analytics, subscription renewals, cleanup tasks
- **HTTP**: Dashboard, API endpoints, webhook handlers

### Content Management System

```bash
# CMS with file uploads and media processing
cja new cms-platform
cd cms-platform

# Add media processing dependencies
cat >> Cargo.toml << EOF
image = "0.24"
aws-sdk-s3 = "0.34"
tokio-stream = "0.1"
EOF
```

**Features Used:**
- **Sessions**: Author authentication and permissions
- **Jobs**: Image resizing, file uploads, content indexing
- **Cron**: Content publication, backup creation, analytics
- **HTTP**: Content editing interface, public website

### Data Pipeline

```bash
# ETL and data processing service
cja new data-pipeline --no-sessions
cd data-pipeline

# Add data processing dependencies
cat >> Cargo.toml << EOF
csv = "1.2"
polars = "0.33"
arrow = "50.0"
EOF
```

**Features Used:**
- **Jobs**: Data ingestion, transformation, validation
- **Cron**: Scheduled data imports, cleanup, reporting
- **HTTP**: Health checks, manual trigger endpoints

### Monitoring Service

```bash
# Application monitoring and alerting
cja new monitor-service
cd monitor-service

# Add monitoring dependencies
cat >> Cargo.toml << EOF
prometheus = "0.13"
slack-hook = "0.8"
lettre = "0.10"
EOF
```

**Features Used:**
- **Sessions**: Dashboard access and user preferences
- **Jobs**: Alert processing, notification delivery
- **Cron**: Health checks, metric collection, report generation
- **HTTP**: Metrics exposition, webhook receivers, dashboard

## Testing Your Generated Projects

### Unit Testing
```bash
cd your-project

# Run tests
cargo test

# Run with output
cargo test -- --nocapture

# Test specific module
cargo test jobs::tests
```

### Integration Testing
```bash
# Set test database
export DATABASE_URL="postgres://localhost/your_project_test"
createdb your_project_test

# Run integration tests
cargo test --test integration
```

### Load Testing
```bash
# Install bombardier
go install github.com/codesenberg/bombardier@latest

# Test your API
bombardier -c 10 -n 1000 http://localhost:3000/health
```

## Environment Configuration

### Development Environment
```bash
# .env file for development
cat > .env << EOF
DATABASE_URL=postgres://localhost/app_dev
PORT=3000
RUST_LOG=debug
SERVER_DISABLED=false
JOBS_DISABLED=false
CRON_DISABLED=false
EOF
```

### Production Environment
```bash
# Environment variables for production
export DATABASE_URL="postgres://prod-host:5432/app_prod?sslmode=require"
export PORT=8080
export RUST_LOG=info
export WORKER_THREADS=4
export MAX_CONNECTIONS=20
```

### Docker Deployment
```dockerfile
# Dockerfile for generated project
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/your-app /usr/local/bin/app
EXPOSE 8080
CMD ["app"]
```

## Performance Tuning

### Database Configuration
```toml
# Add to Cargo.toml for production
[dependencies.sqlx]
version = "0.8"
features = ["runtime-tokio-rustls", "postgres", "uuid", "json", "chrono"]
default-features = false

# Configure connection pool
sqlx = { version = "0.8", features = ["runtime-tokio-rustls", "postgres"], default-features = false }
```

### Async Runtime Tuning
```rust
// In your generated main.rs, customize the runtime
tokio::runtime::Builder::new_multi_thread()
    .worker_threads(num_cpus::get())
    .max_blocking_threads(512)
    .enable_all()
    .build()?
    .block_on(async { run_application().await })
```

### Memory and CPU Optimization
```bash
# Build with optimizations
cargo build --release

# Profile memory usage
valgrind --tool=massif ./target/release/your-app

# Profile CPU usage
cargo install flamegraph
cargo flamegraph --bin your-app
```

## Troubleshooting Common Issues

### Database Connection Issues
```bash
# Check PostgreSQL is running
pg_isready -h localhost -p 5432

# Test connection manually
psql $DATABASE_URL -c "SELECT 1;"

# Check migrations
cargo sqlx migrate info
```

### Build Issues
```bash
# Clean build
cargo clean && cargo build

# Update dependencies
cargo update

# Check for conflicts
cargo tree --duplicates
```

### Runtime Issues
```bash
# Enable debug logging
export RUST_LOG=debug

# Check system resources
htop
df -h

# Monitor network connections
netstat -tlnp | grep :3000
```

## Next Steps

After generating your project:

1. **Customize the generated code** to fit your specific needs
2. **Add your business logic** to the route handlers
3. **Implement your data models** and database schemas
4. **Add authentication and authorization** if using sessions
5. **Create your job types** if using background processing
6. **Set up monitoring and logging** for production
7. **Write comprehensive tests** for your application logic
8. **Deploy to your preferred platform** (Docker, cloud services, etc.)

For more advanced usage and framework details, see the [CJA Documentation](https://github.com/your-org/cja).