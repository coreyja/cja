//! Scheduled task execution with interval-based and cron expression scheduling.
//!
//! The cron system runs jobs on a schedule, either at fixed intervals or using
//! standard cron expressions with timezone support.
//!
//! # Interval Scheduling
//!
//! ```rust,ignore
//! use cja::cron::CronRegistry;
//! use std::time::Duration;
//!
//! let mut registry = CronRegistry::new();
//!
//! // Run a Job every 60 seconds
//! registry.register_job(MyJob, Some("Description"), Duration::from_secs(60));
//! ```
//!
//! # Cron Expression Scheduling
//!
//! ```rust,ignore
//! // Run at 9 AM every day (cron syntax: sec min hour day month weekday year)
//! registry.register_job_with_cron(
//!     DailyReportJob,
//!     Some("Daily report"),
//!     "0 0 9 * * * *",
//! )?;
//! ```
//!
//! # Running the Worker
//!
//! ```rust,ignore
//! use cja::cron::Worker;
//! use chrono_tz::US::Eastern;
//! use std::time::Duration;
//!
//! // With timezone and custom poll interval
//! Worker::new_with_timezone(app_state, registry, Eastern, Duration::from_secs(30))
//!     .run(shutdown_token)
//!     .await;
//!
//! // Or with defaults (UTC, 60s poll interval)
//! Worker::new(app_state, registry)
//!     .run(shutdown_token)
//!     .await;
//! ```
//!
//! The default poll interval is 60 seconds. To change it, use
//! [`Worker::new_with_timezone`] — there is no `new_with_interval` method.
//!
//! # Database-backed cron claims
//!
//! `register_job` and `register_job_with_cron` enqueue exactly once per due
//! interval or cron slot across schedulers sharing a database. The row lock,
//! queue insert, and `last_run_at` update commit in one transaction. A crash
//! before commit rolls them all back, so the next tick retries. Job execution
//! remains at-least-once because worker leases and retries are unchanged.
//!
//! `register` and `register_with_cron` callbacks are at-most-once per interval:
//! the claim commits before the callback runs. If it errors or the process dies,
//! that interval is skipped and the next interval fires normally. A callback
//! error is logged and the worker keeps running — no process restart is needed
//! for the next interval. A tick whose `crons` read fails is skipped the same
//! way and retried on the next poll. A panicking callback is not caught: under
//! the Supervisor it ends the process. Its claim has already committed, so the
//! restart does not retry it immediately, but a callback that always panics
//! ends the process again each interval.
//! During a rollout, an older scheduler overlapping an upgraded one can still
//! double-fire.
//!
//! The pre-filter and the claim re-check both run on database time
//! (`clock_timestamp()`). For any cron with a `crons` row, host/DB clock
//! skew can no longer delay or advance a fire. The one remaining app-clock
//! input is the first slot of a *never-run* cron expression, which anchors on
//! the worker's start time.
//!
//! A `last_run_at` ahead of the database clock is treated as due by the
//! claim re-check, logged at WARN (target `cja::cron::registry`, field
//! `cron.name`), and restamped with the claim time. **Stamping `last_run_at`
//! into the future no longer pauses a cron** — it fires on the next tick
//! instead. The old pause-by-future-stamp runbook is dead; pausing now
//! requires app-level means (`CRON_DISABLED`, app config, or the
//! first-class per-cron pause tracked in DEV-2227).
//!
//! Registering two crons with the same name (callback or job — one shared
//! namespace) panics at registration instead of silently replacing the
//! first.
//!
//! # Queue Pileup Warning
//!
//! If a cron job takes longer to run than its scheduling interval, the queue
//! grows unbounded (CJA does not deduplicate). For example, a 60-second interval
//! with a 20-minute job runtime queues ~20 copies before the first finishes.
//!
//! Mitigations: use longer intervals, add job-level deduplication, or set timeouts.

pub(crate) mod registry;
pub use registry::{CronRegistry, CronSchedule, IntervalSchedule, Schedule};
pub use tokio_util::sync::CancellationToken;

mod worker;
pub use worker::Worker;
