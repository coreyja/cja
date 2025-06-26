use crate::app_state::{self};

use super::worker::JobFromDB;

/// A trait for job registries that can dispatch jobs based on their name.
/// 
/// This trait is typically implemented using the `impl_job_registry!` macro,
/// which generates the necessary dispatch logic for all registered job types.
#[async_trait::async_trait]
pub trait JobRegistry<AppState: app_state::AppState> {
    /// Run a job from the database by dispatching to the appropriate handler.
    async fn run_job(&self, job: &JobFromDB, app_state: AppState) -> color_eyre::Result<()>;
}

/// A macro for implementing a job registry that handles job dispatch.
/// 
/// This macro generates a `Jobs` struct that implements `JobRegistry` for your application state.
/// It creates a match statement that routes jobs to their appropriate handlers based on the job name.
/// 
/// # Usage
/// 
/// ```text
/// use cja::impl_job_registry;
/// use cja::jobs::Job;
/// use serde::{Serialize, Deserialize};
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
/// impl<AS: cja::app_state::AppState> Job<AS> for ProcessPaymentJob {
///     const NAME: &'static str = "ProcessPaymentJob";
///     async fn run(&self, _: AS) -> color_eyre::Result<()> { 
///         println!("Processing payment for user {}", self.user_id);
///         Ok(()) 
///     }
/// }
/// 
/// #[async_trait::async_trait]
/// impl<AS: cja::app_state::AppState> Job<AS> for SendNotificationJob {
///     const NAME: &'static str = "SendNotificationJob";
///     async fn run(&self, _: AS) -> color_eyre::Result<()> { 
///         println!("Sending notification to user {}: {}", self.user_id, self.message);
///         Ok(()) 
///     }
/// }
/// 
/// // Register all your job types with the macro
/// impl_job_registry!(MyAppState, ProcessPaymentJob, SendNotificationJob);
/// 
/// // You can register as many job types as needed
/// impl_job_registry!(
///     MyAppState,
///     EmailJob,
///     ReportJob,
///     CleanupJob,
///     DataImportJob,
///     // ... more job types
/// );
/// ```
#[macro_export]
macro_rules! impl_job_registry {
    ($state:ty, $($job_type:ty),*) => {
        pub(crate) struct Jobs;

        #[async_trait::async_trait]
        impl $crate::jobs::registry::JobRegistry<$state> for Jobs {
            async fn run_job(&self, job: &$crate::jobs::worker::JobFromDB, app_state: $state) -> $crate::Result<()> {
                use $crate::jobs::Job as _;

                let payload = job.payload.clone();

                match job.name.as_str() {
                    $(
                        <$job_type>::NAME => <$job_type>::run_from_value(payload, app_state).await,
                    )*
                    _ => Err($crate::color_eyre::eyre::eyre!("Unknown job type: {}", job.name)),
                }
            }
        }
    };
}
