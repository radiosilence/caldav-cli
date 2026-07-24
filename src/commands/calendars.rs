use crate::models::Output;

/// List every calendar collection on the account.
pub async fn list_calendars() -> anyhow::Result<()> {
    let client = super::make_client()?;
    let calendars = client.list_calendars().await?;
    Output::success(calendars).print();
    Ok(())
}
