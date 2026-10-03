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
//!     .await?;
//!
//! // Or with defaults (UTC, 60s poll interval)
//! Worker::new(app_state, registry)
//!     .run(shutdown_token)
//!     .await?;
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
//! that interval is skipped and the next interval fires normally.
//! During a rollout, an older scheduler overlapping an upgraded one can still
//! double-fire.
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
