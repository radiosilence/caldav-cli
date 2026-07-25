//! GraphQL mutation resolvers.
//!
//! Writes that only add something, or that only record a preference, go straight
//! through: `createEvent`, `createCalendar`, `setDefaultCalendar`. Everything
//! that changes or removes state the user already has is two-phase — PREVIEW
//! renders what would change and hands back a one-shot token, CONFIRM applies
//! it. A calendar is shared, visible state, and the model should never move or
//! delete something without the user seeing the change described first.
//!
//! Split by what is being written: [`event`] for the contents of a calendar,
//! [`calendar`] for the collections themselves.

use async_graphql::MergedObject;

mod calendar;
mod event;

/// The two halves merged into one mutation root. `MergedObject` exists for
/// exactly this: `#[Object]` takes a single impl block per type, and one block
/// holding every write would be a thousand lines of unrelated concerns.
#[derive(MergedObject, Default)]
pub struct MutationRoot(event::EventMutation, calendar::CalendarMutation);

/// The best available rendering of an event's time, for preview text.
///
/// Prefers the forms a person reads over the raw wire value, but falls back to
/// it rather than showing nothing — an event we couldn't parse is exactly the one
/// a user most needs to see identified before they act on it.
fn show_time(t: &crate::models::EventTime) -> String {
    t.date
        .clone()
        .or_else(|| t.date_time.clone())
        .unwrap_or_else(|| t.raw.clone())
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}
