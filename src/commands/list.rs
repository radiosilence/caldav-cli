use crate::caldav::MAX_EVENTS;
use crate::models::Output;

use super::RangeArgs;

/// List events in a time window, across all calendars or just one.
pub async fn list_events(
    calendar: Option<&str>,
    range: &RangeArgs,
    limit: usize,
    expand: bool,
) -> anyhow::Result<()> {
    let client = super::make_client()?;
    let (start, end) = range.resolve()?;
    let events = client
        .events_in_range(calendar, start, end, expand, limit.min(MAX_EVENTS))
        .await?;
    Output::success(events).print();
    Ok(())
}
