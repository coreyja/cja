use std::{collections::HashMap, future::Future, time::Duration};

use color_eyre::eyre::{WrapErr as _, eyre};
use tokio::task::{Id, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

/// A named wrapper around a tokio `JoinHandle`, used to track which
/// long-running task completed (or failed) first.
pub struct NamedTask {
    name: &'static str,
    handle: tokio::task::JoinHandle<crate::Result<()>>,
}

impl NamedTask {
    /// Spawn a named async task on the tokio runtime.
    pub fn spawn<F>(name: &'static str, future: F) -> Self
    where
        F: Future<Output = crate::Result<()>> + Send + 'static,
    {
        Self {
            name,
            handle: tokio::spawn(future),
        }
    }

    /// Returns the name of this task.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// Wait for the first task in the set to complete and return its name alongside
/// the join result.
///
/// For long-running services (server, job worker, cron) any task exiting is
/// typically an error — use this to detect which one stopped.
pub async fn wait_for_first_task(
    tasks: Vec<NamedTask>,
) -> (
    &'static str,
    Result<crate::Result<()>, tokio::task::JoinError>,
) {
    let (handles, names): (Vec<_>, Vec<_>) = tasks.into_iter().map(|t| (t.handle, t.name)).unzip();

    let (result, index, _remaining) = futures::future::select_all(handles).await;
    (names[index], result)
}

/// Wait for the first task to complete and convert the outcome into a
/// single `Result`.
///
/// All three exit conditions (clean exit, error, panic) are treated as
/// errors because long-running tasks are not expected to return.
pub async fn wait_for_first_error(tasks: Vec<NamedTask>) -> crate::Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }

    let (name, result) = wait_for_first_task(tasks).await;

    match result {
        Ok(Ok(())) => {
            tracing::error!(task = name, "Task exited unexpectedly");
            Err(eyre!("Task '{}' exited unexpectedly", name))
        }
        Ok(Err(e)) => {
            tracing::error!(task = name, error = ?e, "Task failed with error");
            Err(e)
        }
        Err(join_error) => {
            tracing::error!(task = name, error = ?join_error, "Task panicked");
            Err(eyre!("Task '{}' panicked: {}", name, join_error))
        }
    }
}

/// How long a process may spend shutting down, split into the two phases a
/// [`Supervisor`] runs.
///
/// The sum has to fit inside the platform's kill window, which is short and
/// mostly out of the app's hands: Cloud Run sends `SIGKILL` a fixed 10 seconds
/// after `SIGTERM`; Fly waits `kill_timeout`, 5 seconds unless `fly.toml` says
/// otherwise. Whatever is still running when the platform kills the process
/// keeps its job locks until the lock timeout, so the defaults (2s + 2s) are
/// sized for the smallest of those windows. Raise them where there is room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownBudget {
    /// How long an in-flight job may keep running after shutdown starts
    /// before it is dropped and its lock released. Hand this to
    /// [`crate::jobs::worker::job_worker_with_shutdown_drain`].
    pub job_drain: Duration,
    /// Extra time for every task to exit once the job drain is over: workers
    /// release their locks, the HTTP server closes connections.
    pub exit_grace: Duration,
}

impl Default for ShutdownBudget {
    fn default() -> Self {
        Self {
            job_drain: Duration::from_secs(2),
            exit_grace: Duration::from_secs(2),
        }
    }
}

impl ShutdownBudget {
    /// Read `CJA_SHUTDOWN_JOB_DRAIN_SECS` and `CJA_SHUTDOWN_EXIT_GRACE_SECS`,
    /// falling back to the defaults for anything unset or unparseable.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            job_drain: env_secs("CJA_SHUTDOWN_JOB_DRAIN_SECS").unwrap_or(defaults.job_drain),
            exit_grace: env_secs("CJA_SHUTDOWN_EXIT_GRACE_SECS").unwrap_or(defaults.exit_grace),
        }
    }

    /// Total time from the shutdown signal until remaining tasks are aborted.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        self.job_drain + self.exit_grace
    }

    /// Warn when the budget cannot fit a kill window cja can detect. Only
    /// Cloud Run qualifies: its window is fixed, while Fly's is configurable
    /// and not visible to the process.
    fn warn_if_over_platform_window(&self) {
        const CLOUD_RUN_WINDOW: Duration = Duration::from_secs(10);

        if std::env::var_os("K_SERVICE").is_some() && self.deadline() >= CLOUD_RUN_WINDOW {
            tracing::warn!(
                deadline_secs = self.deadline().as_secs(),
                window_secs = CLOUD_RUN_WINDOW.as_secs(),
                "Shutdown budget does not fit Cloud Run's termination window; \
                 in-flight job locks will be stranded on every deploy"
            );
        }
    }
}

fn env_secs(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// Runs an app's long-lived tasks and owns their shutdown.
///
/// Spawn the server, job workers and cron through it, handing each one
/// [`Supervisor::shutdown_token`], then await [`Supervisor::run`]. It returns
/// when the process should exit:
///
/// 1. `SIGTERM` or `SIGINT` arrives (handlers are registered in
///    [`Supervisor::new`], so a signal during startup is not lost), or any
///    task exits on its own.
/// 2. The token is cancelled: workers stop claiming jobs, in-flight jobs get
///    [`ShutdownBudget::job_drain`] to finish, and whatever is left is
///    dropped with its lock released so another instance re-runs it at once
///    instead of after the lock timeout.
/// 3. Tasks still running at [`ShutdownBudget::deadline`] are aborted. With
///    websockets or other long-lived connections that is routine, since they
///    hold axum's graceful shutdown open indefinitely.
///
/// A signal is a clean exit. A task exiting by itself is an error, because
/// every task is meant to run for the life of the process; its peers are
/// still drained first.
///
/// ```ignore
/// let mut supervisor = cja::tasks::Supervisor::new(ShutdownBudget::from_env())?;
/// let shutdown = supervisor.shutdown_token();
///
/// supervisor.spawn(
///     "server",
///     cja::server::run_server_until(routes(app_state.clone()), shutdown.clone().cancelled_owned()),
/// );
/// supervisor.spawn(
///     "jobs",
///     cja::jobs::worker::job_worker_with_shutdown_drain(
///         app_state.clone(),
///         Jobs,
///         Duration::from_secs(60),
///         cja::jobs::DEFAULT_MAX_RETRIES,
///         shutdown.clone(),
///         cja::jobs::DEFAULT_LOCK_TIMEOUT,
///         supervisor.budget().job_drain,
///     ),
/// );
///
/// supervisor.run().await
/// ```
pub struct Supervisor {
    tasks: JoinSet<crate::Result<()>>,
    names: HashMap<Id, &'static str>,
    shutdown: CancellationToken,
    budget: ShutdownBudget,
    signals: ShutdownSignals,
}

impl Supervisor {
    /// Register the signal handlers and create the shared shutdown token.
    /// Must be called from within a tokio runtime.
    ///
    /// # Errors
    ///
    /// Fails if a signal handler cannot be registered.
    pub fn new(budget: ShutdownBudget) -> crate::Result<Self> {
        budget.warn_if_over_platform_window();
        Ok(Self {
            tasks: JoinSet::new(),
            names: HashMap::new(),
            shutdown: CancellationToken::new(),
            budget,
            signals: ShutdownSignals::register()?,
        })
    }

    /// The token every supervised task should watch. Cancelled once, when
    /// shutdown starts.
    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    #[must_use]
    pub fn budget(&self) -> ShutdownBudget {
        self.budget
    }

    /// Spawn a named task on the tokio runtime.
    pub fn spawn<F>(&mut self, name: &'static str, future: F)
    where
        F: Future<Output = crate::Result<()>> + Send + 'static,
    {
        let handle = self.tasks.spawn(future);
        self.names.insert(handle.id(), name);
    }

    /// Run until a shutdown signal or a task exit, then stop everything
    /// within the budget.
    ///
    /// # Errors
    ///
    /// Returns the failure of the task that ended the process. A
    /// signal-initiated stop is `Ok`, even when stragglers had to be aborted.
    pub async fn run(mut self) -> crate::Result<()> {
        let signals = std::mem::take(&mut self.signals);
        self.run_until(signals.recv()).await
    }

    async fn run_until(mut self, signal: impl Future<Output = &'static str>) -> crate::Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let result = tokio::select! {
            signal = signal => {
                tracing::info!(
                    signal,
                    deadline_secs = self.budget.deadline().as_secs(),
                    "Shutdown signal received; draining"
                );
                Ok(())
            }
            Some(outcome) = self.tasks.join_next_with_id() => {
                let (name, result) = self.outcome(outcome);
                let error = match result {
                    Ok(()) => eyre!("Task '{name}' exited unexpectedly"),
                    Err(error) => error,
                };
                tracing::error!(task = name, error = %format!("{error:#}"), "Task ended the process");
                Err(error)
            }
        };

        self.shutdown.cancel();
        let drained = tokio::time::timeout(self.budget.deadline(), async {
            while let Some(outcome) = self.tasks.join_next_with_id().await {
                match self.outcome(outcome) {
                    (name, Ok(())) => tracing::debug!(task = name, "Task stopped"),
                    (name, Err(error)) => {
                        tracing::warn!(task = name, error = %format!("{error:#}"), "Task failed while stopping");
                    }
                }
            }
        })
        .await;

        if drained.is_ok() {
            tracing::info!("All tasks stopped");
        } else {
            let mut stragglers: Vec<_> = self.names.values().copied().collect();
            stragglers.sort_unstable();
            tracing::warn!(
                ?stragglers,
                "Shutdown deadline reached; aborting remaining tasks"
            );
            self.tasks.shutdown().await;
        }

        result
    }

    /// Resolve a finished task to its name, folding a panic into the error.
    fn outcome(
        &mut self,
        outcome: Result<(Id, crate::Result<()>), JoinError>,
    ) -> (&'static str, crate::Result<()>) {
        match outcome {
            Ok((id, result)) => {
                let name = self.names.remove(&id).unwrap_or("unknown");
                (
                    name,
                    result.wrap_err_with(|| format!("Task '{name}' failed")),
                )
            }
            Err(error) => {
                let name = self.names.remove(&error.id()).unwrap_or("unknown");
                (name, Err(eyre!("Task '{name}' panicked: {error}")))
            }
        }
    }
}

/// `SIGTERM` (Cloud Run, Kubernetes, systemd) and `SIGINT` (Fly's default,
/// Ctrl-C). Off unix only Ctrl-C exists.
#[derive(Default)]
struct ShutdownSignals {
    #[cfg(unix)]
    unix: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
}

impl ShutdownSignals {
    #[cfg(unix)]
    fn register() -> crate::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        let sigterm = signal(SignalKind::terminate()).wrap_err("Failed to register SIGTERM")?;
        let sigint = signal(SignalKind::interrupt()).wrap_err("Failed to register SIGINT")?;
        Ok(Self {
            unix: Some((sigterm, sigint)),
        })
    }

    #[cfg(not(unix))]
    #[allow(clippy::unnecessary_wraps)]
    fn register() -> crate::Result<Self> {
        Ok(Self {})
    }

    #[cfg(unix)]
    async fn recv(self) -> &'static str {
        let Some((mut sigterm, mut sigint)) = self.unix else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        }
    }

    #[cfg(not(unix))]
    async fn recv(self) -> &'static str {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "Ctrl-C",
            Err(_) => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    const BUDGET: ShutdownBudget = ShutdownBudget {
        job_drain: Duration::from_secs(5),
        exit_grace: Duration::from_secs(3),
    };

    /// A task that stops as soon as it is asked to, like the real workers.
    fn spawn_cooperative(supervisor: &mut Supervisor, name: &'static str) -> Arc<AtomicBool> {
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let shutdown = supervisor.shutdown_token();
        supervisor.spawn(name, async move {
            shutdown.cancelled().await;
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        stopped
    }

    #[tokio::test(start_paused = true)]
    async fn signal_stops_every_task_and_exits_cleanly() {
        let mut supervisor = Supervisor::new(BUDGET).unwrap();
        let shutdown = supervisor.shutdown_token();
        let jobs = spawn_cooperative(&mut supervisor, "jobs");
        let cron = spawn_cooperative(&mut supervisor, "cron");

        let result = supervisor.run_until(async { "SIGTERM" }).await;

        assert!(result.is_ok(), "a signal is a clean exit: {result:?}");
        assert!(shutdown.is_cancelled());
        assert!(jobs.load(Ordering::SeqCst) && cron.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn task_ignoring_shutdown_is_aborted_at_the_deadline() {
        let mut supervisor = Supervisor::new(BUDGET).unwrap();
        spawn_cooperative(&mut supervisor, "jobs");
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        supervisor.spawn("server", async move {
            tokio::time::sleep(Duration::from_hours(1)).await;
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });

        let started = tokio::time::Instant::now();
        let result = supervisor.run_until(async { "SIGTERM" }).await;

        assert!(
            result.is_ok(),
            "stragglers do not turn a signal into a failure"
        );
        assert_eq!(started.elapsed(), BUDGET.deadline());
        tokio::time::sleep(Duration::from_hours(2)).await;
        assert!(
            !finished.load(Ordering::SeqCst),
            "the straggler must be aborted, not left running"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn task_ending_on_its_own_fails_the_process_but_still_drains_its_peers() {
        for mode in ["error", "exit", "panic"] {
            let mut supervisor = Supervisor::new(BUDGET).unwrap();
            let peer = spawn_cooperative(&mut supervisor, "jobs");
            supervisor.spawn("cron", async move {
                match mode {
                    "error" => Err(eyre!("database went away")),
                    "panic" => panic!("cron blew up"),
                    _ => Ok(()),
                }
            });

            let error = supervisor
                .run_until(std::future::pending())
                .await
                .expect_err("a task ending by itself is a failure");

            let rendered = format!("{error:#}");
            let expected = match mode {
                "error" => "database went away",
                "panic" => "cron blew up",
                _ => "exited unexpectedly",
            };
            assert!(rendered.contains("'cron'"), "{mode}: {rendered}");
            assert!(rendered.contains(expected), "{mode}: {rendered}");
            assert!(
                peer.load(Ordering::SeqCst),
                "{mode}: surviving workers must get the chance to release their locks"
            );
        }
    }

    #[tokio::test]
    async fn no_tasks_returns_immediately() {
        let supervisor = Supervisor::new(BUDGET).unwrap();
        assert!(supervisor.run_until(std::future::pending()).await.is_ok());
    }

    #[test]
    fn default_budget_fits_the_smallest_platform_window() {
        // Fly's default `kill_timeout` is 5 seconds.
        assert!(ShutdownBudget::default().deadline() < Duration::from_secs(5));
    }
}
