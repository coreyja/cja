use crate::app_state::{self};

use super::worker::JobFromDB;

/// A trait for job registries that can dispatch jobs based on their name.
///
/// This trait is typically implemented using the `impl_job_registry!` macro,
/// which generates the necessary dispatch logic for all registered job types.
#[async_trait::async_trait]
pub trait JobRegistry<AppState: app_state::AppState> {
    /// Run a job from the database by dispatching to the appropriate handler.
    async fn run_job(
        &self,
        job: &JobFromDB,
        app_state: AppState,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> color_eyre::Result<()>;

    /// The names of every job type registered with this registry.
    ///
    /// Used for boot-time manifest emission (see [`crate::eyes_manifest`]).
    /// The `impl_job_registry!` macro implements this automatically from the
    /// registered job types' `NAME` constants.
    fn job_names() -> &'static [&'static str]
    where
        Self: Sized;
}

/// A macro for implementing a job registry that handles job dispatch.
///
/// This macro generates a `Jobs` struct that implements `JobRegistry` for your application state.
/// It creates a match statement that routes jobs to their appropriate handlers based on the job name.
///
/// # Usage
///
/// ```rust
/// use cja::impl_job_registry;
/// use cja::jobs::Job;
/// use cja::app_state::AppState;
/// use cja::server::cookies::CookieKey;
/// use serde::{Serialize, Deserialize};
///
/// // Define your app state
/// #[derive(Clone)]
/// struct MyAppState {
///     db: sqlx::PgPool,
///     cookie_key: CookieKey,
/// }
///
/// impl AppState for MyAppState {
///     fn version(&self) -> &str { "1.0.0" }
///     fn db(&self) -> &sqlx::PgPool { &self.db }
///     fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
/// }
///
/// // Define your job types
/// #[derive(Debug, Serialize, Deserialize, Clone)]
/// struct ProcessPaymentJob {
///     user_id: i32,
///     amount_cents: i64,
/// }
///
/// #[derive(Debug, Serialize, Deserialize, Clone)]
/// struct SendNotificationJob {
///     user_id: i32,
///     message: String,
/// }
///
/// // Implement the Job trait for each job type
/// #[async_trait::async_trait]
/// impl Job<MyAppState> for ProcessPaymentJob {
///     const NAME: &'static str = "ProcessPaymentJob";
///     async fn run(&self, _: MyAppState) -> color_eyre::Result<()> {
///         println!("Processing payment for user {}", self.user_id);
///         Ok(())
///     }
/// }
///
/// #[async_trait::async_trait]
/// impl Job<MyAppState> for SendNotificationJob {
///     const NAME: &'static str = "SendNotificationJob";
///     async fn run(&self, _: MyAppState) -> color_eyre::Result<()> {
///         println!("Sending notification to user {}: {}", self.user_id, self.message);
///         Ok(())
///     }
/// }
///
/// // Register all your job types with the macro
/// impl_job_registry!(MyAppState, ProcessPaymentJob, SendNotificationJob);
/// ```
#[macro_export]
macro_rules! impl_job_registry {
    ($state:ty, $($job_type:ty),*) => {
        pub struct Jobs;

        #[async_trait::async_trait]
        impl $crate::jobs::registry::JobRegistry<$state> for Jobs {
            async fn run_job(
                &self,
                job: &$crate::jobs::worker::JobFromDB,
                app_state: $state,
                cancellation_token: $crate::jobs::CancellationToken,
            ) -> $crate::Result<()> {
                use $crate::jobs::Job as _;

                let payload = job.payload.clone();

                match job.name.as_str() {
                    $(
                        <$job_type>::NAME => <$job_type>::run_from_value(payload, app_state, cancellation_token).await,
                    )*
                    _ => Err($crate::color_eyre::eyre::eyre!("Unknown job type: {}", job.name)),
                }
            }

            fn job_names() -> &'static [&'static str] {
                use $crate::jobs::Job as _;

                &[$(<$job_type>::NAME),*]
            }
        }
    };
}

#[cfg(test)]
mod test {
    use crate::app_state::AppState;
    use crate::jobs::Job;
    use crate::server::cookies::CookieKey;

    #[derive(Clone)]
    struct TestAppState {
        db: sqlx::PgPool,
        cookie_key: CookieKey,
    }

    impl AppState for TestAppState {
        fn db(&self) -> &sqlx::PgPool {
            &self.db
        }

        fn version(&self) -> &'static str {
            "test"
        }

        fn cookie_key(&self) -> &CookieKey {
            &self.cookie_key
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct FirstJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for FirstJob {
        const NAME: &'static str = "FirstJob";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
    struct SecondJob;

    #[async_trait::async_trait]
    impl Job<TestAppState> for SecondJob {
        const NAME: &'static str = "SecondJob";

        async fn run(&self, _app_state: TestAppState) -> color_eyre::Result<()> {
            Ok(())
        }
    }

    impl_job_registry!(TestAppState, FirstJob, SecondJob);

    #[test]
    fn test_job_names_lists_all_registered_jobs() {
        use crate::jobs::registry::JobRegistry;

        let names = <Jobs as JobRegistry<TestAppState>>::job_names();
        assert_eq!(names, &["FirstJob", "SecondJob"]);
    }
}
