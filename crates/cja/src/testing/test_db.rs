//! Destructive helpers for creating isolated `PostgreSQL` test databases.
//!
//! These APIs create, terminate connections to, and drop databases. They are
//! intended only for test harnesses whose prefix is exclusively owned by the
//! caller.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Context, eyre};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Mutex;
use tracing::{error, warn};
use uuid::Uuid;

use crate::Result;

/// Default minimum age of a disconnected test database before it is reaped.
pub const DEFAULT_STALE_TEST_DATABASE_AGE: Duration = Duration::from_hours(1);

static REAPED_PREFIXES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Owns a test database and removes it promptly when dropped.
pub struct TestDatabaseGuard {
    db_name: String,
    admin_options: PgConnectOptions,
}

impl Drop for TestDatabaseGuard {
    fn drop(&mut self) {
        let db_name = self.db_name.clone();
        let admin_options = self.admin_options.clone();
        let cleanup = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    error!(%error, database = %db_name, "failed to create test database cleanup runtime");
                    return;
                }
            };
            runtime.block_on(async move {
                if let Err(error) = cleanup_database(&admin_options, &db_name).await {
                    error!(database = %db_name, error = %format_args!("{error:#}"), "failed to clean up test database");
                }
            });
        });
        if cleanup.join().is_err() {
            error!(database = %self.db_name, "test database cleanup thread panicked");
        }
    }
}

/// Creates an isolated database and a guard that drops it on normal exit.
///
/// Before the first creation for each literal prefix in a process, this makes a
/// best-effort attempt to reap stale databases created by previous processes.
pub async fn create_test_database(prefix: &str) -> Result<(PgPool, TestDatabaseGuard)> {
    validate_prefix(prefix)?;
    let base_options = database_options()?;
    let admin_options = base_options.clone().database("postgres");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(admin_options.clone())
        .await
        .wrap_err("failed to connect to PostgreSQL for test database administration")?;

    let reap_result = reap_prefix_once(&admin_pool, prefix).await;

    let db_name = generate_database_name(prefix, SystemTime::now())?;
    let quoted_name = quote_identifier(&db_name);
    continue_after_reap(prefix, reap_result, async {
        sqlx::query(&format!("CREATE DATABASE {quoted_name}"))
            .execute(&admin_pool)
            .await
            .wrap_err("failed to create test database")
    })
    .await?;
    admin_pool.close().await;

    let test_options = base_options.database(&db_name);
    let pool_result = PgPoolOptions::new()
        .max_connections(5)
        .min_connections(1)
        .idle_timeout(None)
        .connect_with(test_options)
        .await;
    let pool = match pool_result {
        Ok(pool) => pool,
        Err(error) => {
            best_effort_cleanup(&admin_options, &db_name).await;
            return Err(error).wrap_err("failed to connect to newly created test database");
        }
    };
    if let Err(error) = pool.acquire().await {
        pool.close().await;
        best_effort_cleanup(&admin_options, &db_name).await;
        return Err(error).wrap_err("failed to establish test database liveness connection");
    }

    Ok((
        pool,
        TestDatabaseGuard {
            db_name,
            admin_options,
        },
    ))
}

/// Drops disconnected databases in the generated naming format that are at
/// least `max_age` old, returning only names this call actually dropped.
pub async fn reap_stale_test_databases(
    admin_pool: &PgPool,
    prefix: &str,
    now: SystemTime,
    max_age: Duration,
) -> Result<Vec<String>> {
    reap_stale_test_databases_with_hook(admin_pool, prefix, now, max_age, || async { Ok(()) }).await
}

async fn reap_stale_test_databases_with_hook<F, Fut>(
    admin_pool: &PgPool,
    prefix: &str,
    now: SystemTime,
    max_age: Duration,
    mut before_drop: F,
) -> Result<Vec<String>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    validate_prefix(prefix)?;
    let pattern = format!("{}%", escape_like(prefix));
    let names: Vec<String> = sqlx::query_scalar(
        r"SELECT datname FROM pg_database WHERE datname LIKE $1 ESCAPE '\' ORDER BY datname",
    )
    .bind(pattern)
    .fetch_all(admin_pool)
    .await
    .wrap_err("failed to list test databases for stale reaping")?;

    let mut dropped = Vec::new();
    for name in names {
        if !is_stale_database_name(&name, prefix, now, max_age) {
            continue;
        }
        if database_has_connections(admin_pool, &name).await? {
            continue;
        }
        let mut connection = admin_pool
            .acquire()
            .await
            .wrap_err("failed to acquire connection for stale database drop")?;
        let locked = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock(hashtext($1))")
            .bind(&name)
            .fetch_one(&mut *connection)
            .await
            .wrap_err("failed to lock stale test database candidate")?;
        if !locked {
            continue;
        }
        let live = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = $1)",
        )
        .bind(&name)
        .fetch_one(&mut *connection)
        .await
        .wrap_err("failed to recheck test database activity")?;
        if live {
            let _ = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(hashtext($1))")
                .bind(&name)
                .fetch_one(&mut *connection)
                .await;
            continue;
        }
        if let Err(error) = before_drop().await {
            let _ = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(hashtext($1))")
                .bind(&name)
                .fetch_one(&mut *connection)
                .await;
            return Err(error);
        }
        let query = format!("DROP DATABASE IF EXISTS {}", quote_identifier(&name));
        let result = sqlx::query(&query).execute(&mut *connection).await;
        let _ = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(&name)
            .fetch_one(&mut *connection)
            .await;
        match result {
            Ok(_) => dropped.push(name),
            Err(error) if is_database_in_use(&error) => {}
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("failed to reap test database {name}"));
            }
        }
    }
    Ok(dropped)
}

use std::future::Future;

async fn database_has_connections(admin_pool: &PgPool, name: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = $1)",
    )
    .bind(name)
    .fetch_one(admin_pool)
    .await
    .wrap_err("failed to inspect test database activity")
}

async fn reap_prefix_once(admin_pool: &PgPool, prefix: &str) -> Result<()> {
    let prefixes = REAPED_PREFIXES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut prefixes = prefixes.lock().await;
    if prefixes.contains(prefix) {
        return Ok(());
    }
    let max_age = configured_max_age();
    let result = reap_stale_test_databases(admin_pool, prefix, SystemTime::now(), max_age)
        .await
        .map(|_| ());
    prefixes.insert(prefix.to_owned());
    result
}

async fn continue_after_reap<T, Fut>(prefix: &str, reap_result: Result<()>, next: Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    if let Err(error) = reap_result {
        warn!(prefix, error = %format_args!("{error:#}"), "failed to reap stale test databases; continuing");
    }
    next.await
}

fn configured_max_age() -> Duration {
    parse_max_age(std::env::var("CJA_TEST_DB_MAX_AGE_SECS"))
}

fn parse_max_age(value: std::result::Result<String, std::env::VarError>) -> Duration {
    match value {
        Ok(value) => match value.parse::<u64>() {
            Ok(seconds) => Duration::from_secs(seconds),
            Err(error) => {
                warn!(%error, value, "invalid CJA_TEST_DB_MAX_AGE_SECS; using default");
                DEFAULT_STALE_TEST_DATABASE_AGE
            }
        },
        Err(std::env::VarError::NotPresent) => DEFAULT_STALE_TEST_DATABASE_AGE,
        Err(error) => {
            warn!(%error, "invalid CJA_TEST_DB_MAX_AGE_SECS; using default");
            DEFAULT_STALE_TEST_DATABASE_AGE
        }
    }
}

fn database_options() -> Result<PgConnectOptions> {
    let value = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres:///postgres".into());
    PgConnectOptions::from_str(&value)
        .wrap_err("failed to parse DATABASE_URL as PostgreSQL connection options")
}

async fn cleanup_database(options: &PgConnectOptions, db_name: &str) -> Result<()> {
    validate_generated_name(db_name)?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .wrap_err("failed to connect for test database cleanup")?;
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(db_name)
    .execute(&pool)
    .await
    .wrap_err("failed to terminate test database connections")?;
    sqlx::query(&format!(
        "DROP DATABASE IF EXISTS {}",
        quote_identifier(db_name)
    ))
    .execute(&pool)
    .await
    .wrap_err("failed to drop test database")?;
    pool.close().await;
    Ok(())
}

async fn best_effort_cleanup(options: &PgConnectOptions, db_name: &str) {
    if let Err(error) = cleanup_database(options, db_name).await {
        warn!(database = db_name, error = %format_args!("{error:#}"), "failed to clean up database after setup failure");
    }
}

fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty()
        || !prefix.ends_with('_')
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(eyre!(
            "test database prefix must be a nonempty lowercase ASCII identifier fragment ending in '_'"
        ));
    }
    if prefix.len() + 43 > 63 {
        return Err(eyre!("test database prefix is too long for PostgreSQL"));
    }
    Ok(())
}

fn generate_database_name(prefix: &str, now: SystemTime) -> Result<String> {
    validate_prefix(prefix)?;
    let epoch = now
        .duration_since(UNIX_EPOCH)
        .wrap_err("system clock is before the Unix epoch")?
        .as_secs();
    let name = format!("{prefix}{epoch}_{}", Uuid::new_v4().simple());
    validate_generated_name(&name)?;
    Ok(name)
}

fn parse_database_epoch(name: &str, prefix: &str) -> Option<u64> {
    validate_prefix(prefix).ok()?;
    let suffix = name.strip_prefix(prefix)?;
    let (epoch, uuid) = suffix.split_once('_')?;
    if epoch.is_empty()
        || !epoch.bytes().all(|byte| byte.is_ascii_digit())
        || uuid.len() != 32
        || !uuid.bytes().all(|byte| byte.is_ascii_hexdigit())
        || Uuid::parse_str(uuid).ok()?.simple().to_string() != uuid
    {
        return None;
    }
    epoch.parse().ok()
}

fn is_stale_database_name(name: &str, prefix: &str, now: SystemTime, max_age: Duration) -> bool {
    let Some(epoch) = parse_database_epoch(name, prefix) else {
        return false;
    };
    let Some(created) = UNIX_EPOCH.checked_add(Duration::from_secs(epoch)) else {
        return false;
    };
    now.duration_since(created).is_ok_and(|age| age >= max_age)
}

fn validate_generated_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(eyre!("invalid generated test database name"));
    }
    Ok(())
}

fn quote_identifier(name: &str) -> String {
    debug_assert!(validate_generated_name(name).is_ok());
    format!("\"{name}\"")
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}

fn is_database_in_use(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(error) if error.code().as_deref() == Some("55006"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::{Connection, PgConnection};
    use tokio::sync::Mutex;

    use super::*;

    #[test]
    fn generated_name_round_trips() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let name = generate_database_name("cja_test_", now).unwrap();
        assert_eq!(
            parse_database_epoch(&name, "cja_test_"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn rejects_invalid_names() {
        let now = UNIX_EPOCH + Duration::from_secs(200);
        let uuid = Uuid::nil().simple();
        assert!(!is_stale_database_name(
            &format!("cja_test_{uuid}"),
            "cja_test_",
            now,
            Duration::ZERO
        ));
        assert!(!is_stale_database_name(
            &format!("other_100_{uuid}"),
            "cja_test_",
            now,
            Duration::ZERO
        ));
        assert!(!is_stale_database_name(
            &format!("cja_test_300_{uuid}"),
            "cja_test_",
            now,
            Duration::ZERO
        ));
        assert!(!is_stale_database_name(
            "cja_test_100_not-a-uuid",
            "cja_test_",
            now,
            Duration::ZERO
        ));
        assert!(!is_stale_database_name(
            &format!("cja_test_100_{}", Uuid::nil()),
            "cja_test_",
            now,
            Duration::ZERO
        ));
    }

    #[test]
    fn validates_prefixes_and_identifier_budget() {
        for prefix in ["", "CJA_", "cja-test_", "cja"] {
            assert!(validate_prefix(prefix).is_err(), "{prefix}");
        }
        assert!(validate_prefix(&format!("{}_,", "a".repeat(20))).is_err());
        assert!(validate_prefix(&format!("{}_", "a".repeat(20))).is_err());
        assert!(validate_prefix(&format!("{}_", "a".repeat(19))).is_ok());
    }

    #[test]
    fn checked_time_rejects_future_names() {
        let uuid = Uuid::nil().simple();
        assert!(!is_stale_database_name(
            &format!("cja_test_11_{uuid}"),
            "cja_test_",
            UNIX_EPOCH + Duration::from_secs(10),
            Duration::ZERO,
        ));
    }

    #[test]
    fn escapes_like_metacharacters() {
        assert_eq!(escape_like(r"a_b%c\d"), r"a\_b\%c\\d");
    }

    #[test]
    fn parses_socket_and_tcp_options() {
        assert!(PgConnectOptions::from_str("postgres:///cja?host=/var/run/postgresql").is_ok());
        assert!(
            PgConnectOptions::from_str("postgres://user:secret@localhost:5432/cja?sslmode=prefer")
                .is_ok()
        );
    }

    #[test]
    fn parses_configured_max_age_without_mutating_environment() {
        assert_eq!(parse_max_age(Ok("42".into())), Duration::from_secs(42));
        assert_eq!(
            parse_max_age(Ok("invalid".into())),
            DEFAULT_STALE_TEST_DATABASE_AGE
        );
        assert_eq!(
            parse_max_age(Err(std::env::VarError::NotPresent)),
            DEFAULT_STALE_TEST_DATABASE_AGE
        );
    }

    #[tokio::test]
    async fn late_connection_wins_drop_race() {
        let options = database_options().unwrap();
        let admin_options = options.clone().database("postgres");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(admin_options.clone())
            .await
            .unwrap();
        let prefix = format!("r{}_", &Uuid::new_v4().simple().to_string()[..8]);
        let name = format!("{prefix}1_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {}", quote_identifier(&name)))
            .execute(&admin)
            .await
            .unwrap();

        let held = Arc::new(Mutex::new(None::<PgConnection>));
        let hook_held = Arc::clone(&held);
        let hook_options = options.database(&name);
        let dropped = reap_stale_test_databases_with_hook(
            &admin,
            &prefix,
            SystemTime::now(),
            Duration::ZERO,
            move || {
                let held = Arc::clone(&hook_held);
                let options = hook_options.clone();
                async move {
                    *held.lock().await = Some(
                        PgConnection::connect_with(&options)
                            .await
                            .wrap_err("failed to open late test connection")?,
                    );
                    Ok(())
                }
            },
        )
        .await
        .unwrap();

        assert!(dropped.is_empty());
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)"
            )
            .bind(&name)
            .fetch_one(&admin)
            .await
            .unwrap()
        );
        held.lock().await.take();
        cleanup_database(&admin_options, &name).await.unwrap();
    }

    #[tokio::test]
    async fn reaper_failure_is_fail_open_but_setup_failure_propagates() {
        let value =
            continue_after_reap("cja_test_", Err(eyre!("simulated reaper failure")), async {
                Ok(42)
            })
            .await
            .unwrap();
        assert_eq!(value, 42);

        let error = continue_after_reap::<(), _>("cja_test_", Ok(()), async {
            Err(eyre!("simulated creation failure"))
        })
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("simulated creation failure"));
    }
}
