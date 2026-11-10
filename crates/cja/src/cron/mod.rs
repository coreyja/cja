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
//! instead. The old pause-by-future-stamp runbook is dead; use the
//! first-class per-cron pause instead — see ["Pausing a cron at
//! runtime"](#pausing-a-cron-at-runtime) below.
//!
//! Registering two crons with the same name (callback or job — one shared
//! namespace) panics at registration instead of silently replacing the
//! first.
//!
//! # Pausing a cron at runtime
//!
//! Any single cron can be paused and resumed with SQL against the app's
//! database — no deploy, no restart. The pause is consulted inside the cron
//! claim against the database clock, so host/DB clock skew can neither
//! create nor clear it, and it stops the cron on every scheduler sharing the
//! database. It takes effect for any claim that begins after it commits.
//!
//! A claim that already passed its pause check when your statement commits
//! can still finish and enqueue one more fire after you see success — for a
//! destructive job, re-check the queue a few seconds after pausing before
//! concluding nothing is queued. A pause stops new enqueues only: rows
//! already in the `jobs` queue (and their retries) keep running, and a
//! callback cron already past its claim commit runs to completion.
//!
//! Pause a cron that has already fired — the name is read from `crons`
//! itself, so a typo matches nothing and returns zero rows (`INSERT 0 0`)
//! instead of silently pausing nothing:
//!
//! ```sql
//! INSERT INTO cron_pauses (name, paused_until)
//! SELECT name, now() + interval '4 hours' FROM crons WHERE name = 'TurnRetentionJob'
//! ON CONFLICT (name) DO UPDATE
//!   SET paused_until = EXCLUDED.paused_until, paused_at = clock_timestamp()
//! RETURNING name, paused_until;
//! ```
//!
//! Pause a cron that has never fired (no `crons` row exists yet). SQL cannot
//! detect a typo here — the registry is not in the database — so copy the
//! name exactly from the registry or the deploy, and check after the next
//! deploy that no `crons` row appears for it while the pause is in force
//! (`SELECT last_run_at FROM crons WHERE name = 'FreshJob'` — a row
//! appearing means the pause did not match):
//!
//! ```sql
//! INSERT INTO cron_pauses (name, paused_until)
//! VALUES ('FreshJob', now() + interval '4 hours')
//! ON CONFLICT (name) DO UPDATE
//!   SET paused_until = EXCLUDED.paused_until, paused_at = clock_timestamp()
//! RETURNING name, paused_until;
//! ```
//!
//! `'infinity'` in place of the `now() + interval` expression pauses
//! indefinitely. Always compute the expiry on the database (`now() +
//! interval '...'`), never paste a timestamp from your laptop.
//!
//! Resume — `DELETE 0` means no pause row existed: mistyped pause, or
//! already resumed:
//!
//! ```sql
//! DELETE FROM cron_pauses WHERE name = 'TurnRetentionJob';
//! ```
//!
//! Inspect — a pause row whose `last_run_at` is NULL names a cron that has
//! never fired, or a typo (if the cron has fired before, it is a typo):
//!
//! ```sql
//! SELECT p.name, p.paused_at, p.paused_until,
//!       p.paused_until > clock_timestamp() AS in_force,
//!       c.last_run_at
//! FROM cron_pauses p LEFT JOIN crons c ON c.name = p.name
//! ORDER BY p.name;
//! ```
//!
//! While a pause is in force the cron emits no `Enqueuing Task` receipt and
//! its `last_run_at` stops advancing while siblings' keep moving — that is
//! how you confirm it live. On resume an overdue cron fires **once** on the
//! next tick (not once per missed interval) and then resumes cadence; a cron
//! paused before its first fire still reports `last_run = None` on that
//! first fire. Expired rows linger harmlessly (`in_force = false`).
//! Schedulers still running a cja older than this feature ignore the table
//! entirely.
//!
//! Two cautions: (1) receipt-based missed-run monitors (e.g. eyes
//! `cron_missed` on apps with eyes telemetry) will correctly see a paused
//! cron as not firing — silence or expect that alert for the pause's
//! duration; (2) a `last_run_at` ahead of `now()` with no `cron_pauses` row
//! in force is clock skew, not a pause: the cron still fires, WARNs and
//! restamps on its own. Use
//! `SELECT name, last_run_at FROM crons WHERE last_run_at > now();` to tell
//! the two apart.
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
