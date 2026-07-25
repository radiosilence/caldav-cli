//! GraphQL type wrappers around the domain models, plus the preview/confirm
//! nonce store that guards every write.

use async_graphql::{Enum, SimpleObject};

use crate::models::{Attendee, BusyPeriod, Calendar, Event, EventTime};

// ============ Output Types ============

#[derive(SimpleObject)]
#[graphql(name = "Calendar")]
pub struct GqlCalendar {
    /// Short id — pass this (or the name) wherever a calendar is accepted.
    pub id: String,
    /// Server path to the collection.
    pub href: String,
    pub name: String,
    pub description: Option<String>,
    /// Hex colour as set in the calendar app.
    pub color: Option<String>,
    /// True when this account cannot write to the calendar.
    pub read_only: bool,
    /// False for task-only collections, which hold no events.
    pub supports_events: bool,
    /// The account's own default calendar, per the server. New events land
    /// here unless the user picked another one or `calendar` is given.
    pub is_default: bool,
}

impl From<Calendar> for GqlCalendar {
    fn from(c: Calendar) -> Self {
        Self {
            id: c.id,
            href: c.href,
            name: c.name,
            description: c.description,
            color: c.color,
            read_only: c.read_only,
            supports_events: c.supports_events,
            is_default: c.is_default,
        }
    }
}

#[derive(SimpleObject)]
#[graphql(name = "EventTime")]
pub struct GqlEventTime {
    /// RFC 3339 UTC timestamp. Null for all-day times — use `date` instead.
    pub date_time: Option<String>,
    /// `YYYY-MM-DD`, set only for all-day times.
    pub date: Option<String>,
    /// IANA timezone the event was authored in, when the server sent one.
    pub tzid: Option<String>,
    pub all_day: bool,
}

impl From<EventTime> for GqlEventTime {
    fn from(t: EventTime) -> Self {
        Self {
            date_time: t.date_time,
            date: t.date,
            tzid: t.tzid,
            all_day: t.all_day,
        }
    }
}

#[derive(SimpleObject)]
#[graphql(name = "Attendee")]
pub struct GqlAttendee {
    pub email: String,
    pub name: Option<String>,
    /// e.g. `REQ-PARTICIPANT`, `OPT-PARTICIPANT`, `CHAIR`.
    pub role: Option<String>,
    /// e.g. `ACCEPTED`, `DECLINED`, `TENTATIVE`, `NEEDS-ACTION`.
    pub status: Option<String>,
}

impl From<Attendee> for GqlAttendee {
    fn from(a: Attendee) -> Self {
        Self {
            email: a.email,
            name: a.name,
            role: a.role,
            status: a.status,
        }
    }
}

#[derive(SimpleObject)]
#[graphql(name = "Event")]
pub struct GqlEvent {
    /// The iCalendar UID — pass this to `event`, `updateEvent`, `deleteEvent`.
    pub id: String,
    /// Display name of the calendar holding the event.
    pub calendar: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    /// `CONFIRMED`, `TENTATIVE`, or `CANCELLED`.
    pub status: Option<String>,
    pub start: GqlEventTime,
    pub end: GqlEventTime,
    pub all_day: bool,
    /// Recurrence rule of the series, e.g. `FREQ=WEEKLY;BYDAY=MO`.
    pub recurrence: Option<String>,
    /// Set on one occurrence of a recurring series. Two results can share an
    /// `id` and differ only here.
    pub recurrence_id: Option<String>,
    pub organizer: Option<GqlAttendee>,
    pub attendees: Vec<GqlAttendee>,
    pub categories: Vec<String>,
    pub created: Option<String>,
    pub last_modified: Option<String>,
}

impl From<Event> for GqlEvent {
    fn from(e: Event) -> Self {
        Self {
            id: e.id,
            calendar: e.calendar,
            summary: e.summary,
            description: e.description,
            location: e.location,
            url: e.url,
            status: e.status,
            start: e.start.into(),
            end: e.end.into(),
            all_day: e.all_day,
            recurrence: e.recurrence,
            recurrence_id: e.recurrence_id,
            organizer: e.organizer.map(Into::into),
            attendees: e.attendees.into_iter().map(Into::into).collect(),
            categories: e.categories,
            created: e.created,
            last_modified: e.last_modified,
        }
    }
}

#[derive(SimpleObject)]
#[graphql(name = "BusyPeriod")]
pub struct GqlBusyPeriod {
    /// RFC 3339 UTC.
    pub start: String,
    /// RFC 3339 UTC.
    pub end: String,
    /// `BUSY`, `BUSY-TENTATIVE`, or `BUSY-UNAVAILABLE`.
    pub status: String,
}

impl From<BusyPeriod> for GqlBusyPeriod {
    fn from(p: BusyPeriod) -> Self {
        Self {
            start: p.start,
            end: p.end,
            status: p.status,
        }
    }
}

/// Two-step guard on every write: PREVIEW returns a human-readable summary and
/// a one-shot token; CONFIRM performs the change.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum WriteAction {
    /// Describe what would happen and return a `confirmationToken`.
    Preview,
    /// Apply the change. Requires the token from a matching PREVIEW.
    Confirm,
}

#[derive(SimpleObject)]
#[graphql(name = "EventMutationResult")]
pub struct GqlEventResult {
    pub success: bool,
    /// The event as it now stands. Null on PREVIEW and on failure.
    pub event: Option<GqlEvent>,
    /// Human-readable description of the pending change. Set on PREVIEW.
    pub preview: Option<String>,
    /// One-shot token to pass back with CONFIRM. Set on PREVIEW.
    pub confirmation_token: Option<String>,
    pub error: Option<String>,
}

impl GqlEventResult {
    /// Named `pending` rather than `preview` because `SimpleObject`
    /// already generates a `preview` field accessor.
    pub fn pending(text: String, token: String) -> Self {
        Self {
            success: true,
            event: None,
            preview: Some(text),
            confirmation_token: Some(token),
            error: None,
        }
    }

    pub fn done(event: Event) -> Self {
        Self {
            success: true,
            event: Some(event.into()),
            preview: None,
            confirmation_token: None,
            error: None,
        }
    }

    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            event: None,
            preview: None,
            confirmation_token: None,
            error: Some(msg.into()),
        }
    }
}

// ============ Confirmation nonces ============

pub struct Nonce {
    fingerprint: String,
    issued_at: std::time::Instant,
}

/// Process-shared store of outstanding preview tokens. Schema-level rather
/// than request-level because a preview and its confirm are separate requests.
pub type NonceStore = tokio::sync::Mutex<std::collections::HashMap<String, Nonce>>;

/// Hard cap on outstanding nonces. A preview without a confirm is user intent —
/// capacity for hundreds of pending edits is plenty for a single session.
const NONCE_CAP: usize = 256;

/// How long a PREVIEW'd nonce stays valid. Long enough for a human to read and
/// approve, short enough to expire well before the process restarts.
const NONCE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Drop entries older than TTL, then if still over cap drop the oldest.
fn evict(map: &mut std::collections::HashMap<String, Nonce>) {
    let now = std::time::Instant::now();
    map.retain(|_, n| now.duration_since(n.issued_at) < NONCE_TTL);
    while map.len() >= NONCE_CAP {
        let oldest = map
            .iter()
            .min_by_key(|(_, n)| n.issued_at)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
}

/// Fingerprint the params so we can detect tampering between PREVIEW and
/// CONFIRM. Non-cryptographic — it only needs to catch accidental drift, not
/// defeat an attacker who already controls the process.
pub fn params_fingerprint(parts: &[&str]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Issue a new one-shot confirmation nonce for the given params.
pub async fn issue_nonce(store: &NonceStore, parts: &[&str]) -> String {
    let nonce = uuid::Uuid::new_v4().to_string();
    let entry = Nonce {
        fingerprint: params_fingerprint(parts),
        issued_at: std::time::Instant::now(),
    };
    let mut map = store.lock().await;
    evict(&mut map);
    map.insert(nonce.clone(), entry);
    nonce
}

/// Consume a nonce, returning Ok(()) if it was issued for the given params.
/// The nonce is always removed on consumption, even on mismatch or expiry, so
/// a bad CONFIRM forces the caller back to PREVIEW.
pub async fn consume_nonce(
    store: &NonceStore,
    nonce: Option<&str>,
    parts: &[&str],
) -> std::result::Result<(), &'static str> {
    let nonce =
        nonce.ok_or("Missing confirmationToken. Use action=PREVIEW first to obtain one.")?;
    let entry = store
        .lock()
        .await
        .remove(nonce)
        .ok_or("Invalid or already-used confirmationToken. Re-run PREVIEW.")?;
    if std::time::Instant::now().duration_since(entry.issued_at) >= NONCE_TTL {
        return Err("confirmationToken expired. Re-run PREVIEW.");
    }
    if entry.fingerprint != params_fingerprint(parts) {
        return Err("Params changed between PREVIEW and CONFIRM. Re-run PREVIEW.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nonce_round_trips_for_matching_params() {
        let store = NonceStore::default();
        let params = ["Standup", "2026-07-24T09:00:00Z"];
        let nonce = issue_nonce(&store, &params).await;
        assert!(consume_nonce(&store, Some(&nonce), &params).await.is_ok());
    }

    #[tokio::test]
    async fn nonce_is_single_use() {
        let store = NonceStore::default();
        let params = ["Standup"];
        let nonce = issue_nonce(&store, &params).await;
        assert!(consume_nonce(&store, Some(&nonce), &params).await.is_ok());
        assert!(consume_nonce(&store, Some(&nonce), &params).await.is_err());
    }

    #[tokio::test]
    async fn changed_params_are_rejected() {
        let store = NonceStore::default();
        let nonce = issue_nonce(&store, &["Standup", "09:00"]).await;
        let err = consume_nonce(&store, Some(&nonce), &["Standup", "17:00"])
            .await
            .unwrap_err();
        assert!(err.contains("Params changed"));
    }

    #[tokio::test]
    async fn missing_and_unknown_tokens_are_rejected() {
        let store = NonceStore::default();
        assert!(consume_nonce(&store, None, &["x"]).await.is_err());
        assert!(
            consume_nonce(&store, Some("made-up"), &["x"])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn store_stays_bounded() {
        let store = NonceStore::default();
        for i in 0..(NONCE_CAP + 50) {
            issue_nonce(&store, &[&i.to_string()]).await;
        }
        assert!(store.lock().await.len() <= NONCE_CAP);
    }

    #[test]
    fn fingerprint_is_order_sensitive() {
        assert_ne!(
            params_fingerprint(&["a", "b"]),
            params_fingerprint(&["b", "a"])
        );
    }
}
