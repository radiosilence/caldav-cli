use crate::models::Output;

use super::RangeArgs;

/// Busy windows over a range — what you'd consult before proposing a time.
pub async fn free_busy(range: &RangeArgs) -> anyhow::Result<()> {
    let client = super::make_client()?;
    let (start, end) = range.resolve()?;
    let periods = client.free_busy(start, end).await?;
    Output::success(periods).print();
    Ok(())
}
