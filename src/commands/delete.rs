use crate::models::Output;

/// Delete an event by UID. Returns what was deleted, so the removal is
/// recoverable by hand if it was a mistake.
pub async fn delete_event(event_id: &str, calendar: Option<&str>) -> anyhow::Result<()> {
    let client = super::make_client()?;
    let event = client.delete_event(event_id, calendar).await?;
    Output::success(event).print();
    Ok(())
}
