# CJA Framework Testing Strategy

This document outlines the comprehensive testing approach for the CJA meta-framework, focusing on integration tests and documentation tests without mocking.

## Testing Philosophy

- **Integration tests over unit tests**: We test real interactions with databases and systems
- **No mocking**: All tests use real PostgreSQL databases and actual HTTP requests
- **Doc tests as documentation**: Examples in documentation are executable and verified

## Test Infrastructure

### Database Management

- Each test suite creates an isolated PostgreSQL test database named
  `<prefix><unix_seconds>_<uuid-simple>`
- Guard-based cleanup removes databases promptly after clean test completion
- The first creation for each literal prefix in a process reaps disconnected
  timestamped databases older than one hour. Set `CJA_TEST_DB_MAX_AGE_SECS` to
  override the threshold; invalid values warn and use the default.
- Every live pool retains an idle connection, so concurrent test processes remain
  visible in `pg_stat_activity` and are never force-dropped by startup reaping.
- Reaping failures warn and allow creation to continue. Reaping is a backstop and
  runs only when a later test process starts.
- Connection pooling for efficient resource usage
- Migrations are automatically run for each test database

### Test Helpers (`tests/common/`)

- `mod.rs`: Test module declarations
- `db.rs`: Database helpers for seeding test data
- `app.rs`: Test application state and HTTP client utilities

## Doc Tests

Doc tests serve dual purposes: providing usage examples and verifying API functionality.

## Running Tests

### All Tests (Library + Integration)

```bash
cargo test --package cja --lib --test lib
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

### Doc Tests

Doc tests (`cargo test --doc`) **do not work** for this project because `sqlx::query!`
macros require a live database connection at compile time, and doctest crates don't
inherit the `DATABASE_URL` environment variable. Use `--lib --test lib` instead.

### Unix Socket Database (Linux / Lima VM)

If PostgreSQL uses Unix sockets, one `PgConnectOptions`-compatible URL works for
both compile-time and runtime access:

```bash
DATABASE_URL="postgres:///cja_dev?host=/var/run/postgresql" cargo test --package cja --lib --test lib
```

Parallel test execution is supported; no `--test-threads=1` workaround is needed.

## Test Database Configuration

Tests use `DATABASE_URL` when set and otherwise default to `postgres:///postgres`.

Prefixes are nonempty lowercase ASCII identifier fragments ending in `_`. They
must leave 43 bytes for the epoch separator and simple UUID within PostgreSQL's
63-byte identifier limit. All producers for a prefix must use the timestamped
format before stale growth is globally bounded.

Legacy UUID-only `cja_test_*` and `cja_passkey_test_*` databases are deliberately
not reaped because their age cannot be derived safely. During rollout, perform the
existing manual sweep: select only names matching the exact legacy patterns, verify
zero matching sessions in `pg_stat_activity` immediately before each ordinary
`DROP DATABASE`, and never terminate sessions for the sweep.

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
