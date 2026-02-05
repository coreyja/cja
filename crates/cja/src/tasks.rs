use std::future::Future;

use color_eyre::eyre::eyre;

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
