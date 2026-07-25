//! Writes to the calendar collections themselves, rather than their contents.
//!
//! These reach further than an event write does. A rename or a recolour shows up
//! in every calendar app the user owns; deleting a collection takes every event
//! in it; and the default calendar decides where their phone puts a new event,
//! not just where this tool does. So all but the two additive ones are
//! preview-then-confirm, and the previews say what the blast radius is.

use async_graphql::{Context, Object, Result};

use super::super::types::*;
use super::super::{SharedClient, types};
use crate::models::CalendarFields;

#[derive(Default)]
pub struct CalendarMutation;

/// Human-readable before → after for a calendar patch, listing only what changes.
fn preview_update(existing: &crate::models::Calendar, input: &CalendarFields<'_>) -> String {
    let mut lines = vec![format!("Update calendar: {}", existing.name)];
    let mut changes = Vec::new();

    let mut diff = |label: &str, from: Option<&str>, to: Option<&str>| {
        if let Some(to) = to.map(str::trim)
            && Some(to) != from.map(str::trim)
        {
            changes.push(format!(
                "  {label}: {} → {}",
                from.unwrap_or("(unset)"),
                if to.is_empty() { "(cleared)" } else { to }
            ));
        }
    };
    diff("Name", Some(existing.name.as_str()), input.name);
    diff(
        "Description",
        existing.description.as_deref(),
        input.description,
    );
    diff("Colour", existing.color.as_deref(), input.color);
    if let Some(order) = input.order {
        changes.push(format!("  Sidebar position: → {order}"));
    }

    if changes.is_empty() {
        lines.push("No changes requested.".to_string());
    } else {
        lines.push("Changes:".to_string());
        lines.extend(changes);
    }
    lines.join("\n")
}

#[Object]
#[allow(clippy::too_many_arguments)]
impl CalendarMutation {
    /// Create a calendar. Writes immediately — no preview step, since it takes
    /// nothing away. Tell the user the id it landed on.
    ///
    /// The id is derived from the name, so "Work Trips" becomes `work-trips`. A
    /// name whose id collides with an existing collection is refused rather than
    /// silently suffixed.
    async fn create_calendar(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Display name, e.g. 'Work Trips'")] name: String,
        #[graphql(desc = "What the calendar is for")] description: Option<String>,
        #[graphql(desc = "Hex colour, #RRGGBB or #RRGGBBAA")] color: Option<String>,
        #[graphql(desc = "Position in a calendar app's sidebar")] order: Option<i32>,
    ) -> Result<GqlCalendarResult> {
        let client = ctx.data::<SharedClient>()?;
        let fields = CalendarFields {
            name: Some(&name),
            description: description.as_deref(),
            color: color.as_deref(),
            order,
        };
        match client.create_calendar(&name, &fields).await {
            Ok(calendar) => Ok(GqlCalendarResult::done(calendar)),
            Err(e) => Ok(GqlCalendarResult::failed(e.to_string())),
        }
    }

    /// Rename, recolour, or re-describe a calendar. Only the arguments you pass
    /// are changed; pass `""` to clear one.
    ///
    /// The change is visible in every calendar app on the account, so ALWAYS call
    /// with action=PREVIEW first and read the diff back to the user.
    async fn update_calendar(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "PREVIEW first, then CONFIRM to write")] action: WriteAction,
        #[graphql(desc = "Calendar name or id")] id: String,
        #[graphql(desc = "New display name. Cannot be cleared.")] name: Option<String>,
        #[graphql(desc = "New description. Pass \"\" to remove it.")] description: Option<String>,
        #[graphql(desc = "New hex colour. Pass \"\" to remove it.")] color: Option<String>,
        #[graphql(desc = "New sidebar position")] order: Option<i32>,
        #[graphql(desc = "Token from the PREVIEW response — required for CONFIRM")]
        confirmation_token: Option<String>,
    ) -> Result<GqlCalendarResult> {
        let client = ctx.data::<SharedClient>()?;
        let nonce_store = ctx.data::<NonceStore>()?;
        let fields = CalendarFields {
            name: name.as_deref(),
            description: description.as_deref(),
            color: color.as_deref(),
            order,
        };
        let order_part = order.map(|v| v.to_string()).unwrap_or_default();
        let parts = [
            id.as_str(),
            name.as_deref().unwrap_or(""),
            description.as_deref().unwrap_or(""),
            color.as_deref().unwrap_or(""),
            order_part.as_str(),
        ];

        if action == WriteAction::Preview {
            let calendars = types::all_calendars(ctx).await?;
            let existing = match crate::caldav::resolve_calendar(&calendars, Some(&id)) {
                Ok(c) => c,
                Err(e) => return Ok(GqlCalendarResult::failed(e.to_string())),
            };
            let token = issue_nonce(nonce_store, &parts).await;
            return Ok(GqlCalendarResult::pending(
                preview_update(&existing, &fields),
                token,
            ));
        }
        if let Err(msg) = consume_nonce(nonce_store, confirmation_token.as_deref(), &parts).await {
            return Ok(GqlCalendarResult::failed(msg));
        }

        match client.update_calendar(&id, &fields).await {
            Ok(calendar) => Ok(GqlCalendarResult::done(calendar)),
            Err(e) => Ok(GqlCalendarResult::failed(e.to_string())),
        }
    }

    /// Delete a calendar **and every event in it**.
    ///
    /// The single most destructive thing this API can do. ALWAYS call with
    /// action=PREVIEW first — the preview counts what would be lost — and get the
    /// user's explicit approval. The account's own default calendar is refused
    /// outright; point the default elsewhere first.
    async fn delete_calendar(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "PREVIEW first, then CONFIRM to delete")] action: WriteAction,
        #[graphql(desc = "Calendar name or id")] id: String,
        #[graphql(desc = "Token from the PREVIEW response — required for CONFIRM")]
        confirmation_token: Option<String>,
    ) -> Result<GqlCalendarResult> {
        let client = ctx.data::<SharedClient>()?;
        let nonce_store = ctx.data::<NonceStore>()?;
        let parts = [id.as_str()];

        if action == WriteAction::Preview {
            let calendars = types::all_calendars(ctx).await?;
            let existing = match crate::caldav::resolve_calendar(&calendars, Some(&id)) {
                Ok(c) => c,
                Err(e) => return Ok(GqlCalendarResult::failed(e.to_string())),
            };
            // Counting is a year-wide read, paid only on the preview. A bare
            // "this deletes the calendar" tells the user nothing about the
            // scale of what they are agreeing to.
            let year = crate::commands::RangeArgs {
                days: Some(365),
                ..Default::default()
            }
            .resolve()?;
            let events = client
                .events_in_range(Some(&existing.id), year.0, year.1, false, 500)
                .await
                .map(|e| e.len());
            let scale = match events {
                Ok(0) => "It holds no events in the coming year.".to_string(),
                Ok(n) => format!("It holds {n} event(s) in the coming year, and any older ones."),
                // The count is a courtesy; failing it must not block the preview.
                Err(_) => "Its contents could not be counted.".to_string(),
            };

            let token = issue_nonce(nonce_store, &parts).await;
            return Ok(GqlCalendarResult::pending(
                format!(
                    "Delete calendar: {} ({})\n{scale}\nEvery event in it is deleted too. \
                     This cannot be undone.",
                    existing.name, existing.id
                ),
                token,
            ));
        }
        if let Err(msg) = consume_nonce(nonce_store, confirmation_token.as_deref(), &parts).await {
            return Ok(GqlCalendarResult::failed(msg));
        }

        match client.delete_calendar(&id).await {
            Ok(calendar) => Ok(GqlCalendarResult::done(calendar)),
            Err(e) => Ok(GqlCalendarResult::failed(e.to_string())),
        }
    }

    /// Point the account's default calendar at this one (RFC 6638).
    ///
    /// This is the property the user's own calendar apps read to decide where a
    /// new event goes, so it changes behaviour beyond this tool. Writes
    /// immediately — nothing is lost and the previous default is reported, so it
    /// is trivially reversible — but say which calendar it was, and which it now
    /// is.
    ///
    /// Not every server lets this be set: it lives on the scheduling inbox, and
    /// an account without scheduling support has nowhere to keep it. A refusal
    /// comes back in `error`.
    async fn set_default_calendar(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Calendar name or id to make the default")] id: String,
    ) -> Result<GqlCalendarResult> {
        let client = ctx.data::<SharedClient>()?;
        let previous = types::all_calendars(ctx)
            .await?
            .iter()
            .find(|c| c.is_default)
            .map(|c| c.name.clone());

        match client.set_default_calendar(&id).await {
            Ok(calendar) => Ok(GqlCalendarResult {
                previous_default: previous,
                ..GqlCalendarResult::done(calendar)
            }),
            Err(e) => Ok(GqlCalendarResult::failed(e.to_string())),
        }
    }
}
