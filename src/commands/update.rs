use crate::models::Output;

use super::EventArgs;

/// Update an event by UID. Only the fields you pass are changed.
pub async fn update_event(
    event_id: &str,
    calendar: Option<&str>,
    args: &EventArgs,
) -> anyhow::Result<()> {
    let client = super::make_client()?;
    let attendees = args.parsed_attendees();
    let categories = args.parsed_categories();
    let fields = args.fields(attendees.as_deref(), categories.as_deref());

    let event = client.update_event(event_id, calendar, &fields).await?;
    Output::success(event).print();
    Ok(())
}
