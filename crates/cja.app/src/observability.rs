//! Product events emitted by the application also power these trends.
use cja::observability::{
    AlertBuilder, Comparison, Dashboard, Declarations, Item, ProductMetricBuilder, Section,
};
pub fn declarations() -> Result<Declarations, String> {
    Ok(Declarations::new()
        .metric(
            ProductMetricBuilder::count("pages", "page_view")
                .breakdown("path")
                .build()?,
        )
        .metric(ProductMetricBuilder::count("sessions", "session_created").build()?)
        .metric(
            ProductMetricBuilder::count("daily", "session_created")
                .bucket_seconds(86400)
                .build()?,
        )
        .dashboard(
            Dashboard::new("activity", "Product activity")?
                .default_range_seconds(86400)
                .section(
                    Section::new()
                        .item(Item::table("pages"))
                        .item(Item::stat("sessions"))
                        .item(Item::time_series("daily")),
                ),
        )
        .alert(
            AlertBuilder::new("quiet", "daily")
                .window_seconds(86400)
                .evaluation_interval_seconds(3600)
                .warning(Comparison::Below, 1.)
                .build()?,
        ))
}
