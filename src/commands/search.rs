use crate::caldav::MAX_EVENTS;
use crate::models::Output;

use super::RangeArgs;

/// Substring search over summary, description, location, categories, attendees.
pub async fn search_events(
    query: &str,
    calendar: Option<&str>,
    range: &RangeArgs,
    limit: usize,
) -> anyhow::Result<()> {
    let client = super::make_client()?;
    let (start, end) = range.resolve()?;
    let events = client
        .search_events(query, calendar, start, end, limit.min(MAX_EVENTS))
        .await?;
    Output::success(events).print();
    Ok(())
}
