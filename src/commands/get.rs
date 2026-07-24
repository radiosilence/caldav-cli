use crate::error::Error;
use crate::models::Output;

/// Fetch one event by UID.
pub async fn get_event(event_id: &str, calendar: Option<&str>) -> anyhow::Result<()> {
    let client = super::make_client()?;
    match client.get_event(event_id, calendar).await? {
        Some(event) => Output::success(event).print(),
        None => return Err(Error::EventNotFound(event_id.to_string()).into()),
    }
    Ok(())
}
