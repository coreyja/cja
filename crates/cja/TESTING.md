# CJA Framework Testing Strategy

This document outlines the comprehensive testing approach for the CJA meta-framework, focusing on integration tests and documentation tests without mocking.

## Testing Philosophy

- **Integration tests over unit tests**: We test real interactions with databases and systems
- **No mocking**: All tests use real PostgreSQL databases and actual HTTP requests
- **Doc tests as documentation**: Examples in documentation are executable and verified

## Test Infrastructure

### Database Management

- Each test suite creates isolated PostgreSQL test databases
- Automatic cleanup after tests complete
- Connection pooling for efficient resource usage
- Migrations are automatically run for each test database

### Test Helpers (`tests/common/`)

- `mod.rs`: Core test utilities and database lifecycle management
- `db.rs`: Database helpers for seeding test data
- `app.rs`: Test application state and HTTP client utilities

## Doc Tests

Doc tests serve dual purposes: providing usage examples and verifying API functionality.

## Running Tests

### All Tests

```bash
cargo test
```

### Doc Tests Only

```bash
cargo test --doc
```

### Integration Tests Only

```bash
cargo test --test lib
```

### Specific Test Suite

```bash
cargo test sessions --test lib
cargo test jobs --test lib
```

## Test Database Configuration

Tests will use the `DATABASE_URL` environment variable if set, otherwise default to `postgres://localhost/postgres`.

Each test creates a unique database with the pattern `cja_test_<uuid>` to avoid conflicts.

## Future Testing Areas

### High Priority

- Cron system integration tests
- End-to-end server tests with routing
- Job worker lifecycle and retry logic

### Medium Priority

- Cookie encryption and security
- Middleware integration
- Performance benchmarks

### Low Priority

- Migration rollback testing
- Database connection pool limits
- Concurrent request handling

## Best Practices

1. **Test Independence**: Each test should create its own database and clean up after itself
2. **Realistic Scenarios**: Tests should mirror real-world usage patterns
3. **Clear Assertions**: Use descriptive test names and clear assertions
4. **Error Cases**: Test both success and failure paths
5. **Concurrency**: Test concurrent operations where applicable

## Adding New Tests

1. Create test file in `tests/integration/`
2. Add module to `tests/integration/mod.rs`
3. Use test helpers from `tests/common/`
4. Follow existing patterns for database setup/teardown
5. Add corresponding doc tests for public APIs
