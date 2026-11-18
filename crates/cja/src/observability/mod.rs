//! Declare metrics, dashboard sections and thresholds once per application.
//!
//! Attach the declarations to an Eyes boot manifest at startup. A separate CI
//! binary calls [`posthog::sync`] to reconcile product-event trends, dashboard
//! tiles and alert bounds. Span metrics and health widgets stay in Eyes.
//! See the repository guide at `docs/posthog-sync.md` for the mapping matrix,
//! required credentials, and reusable workflow.

pub mod declarations;
pub mod posthog;
pub use declarations::*;
pub use eyes_subscriber::{AggregateFunction, MetricValueType};
