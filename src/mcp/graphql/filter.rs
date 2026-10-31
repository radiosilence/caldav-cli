//! Composable event filters and sorts.
//!
//! Every predicate here runs **client-side**, against events already fetched.
//! That is not a shortcut: CalDAV's server-side filtering is a `time-range` plus
//! per-property `text-match`, its sibling filters are all AND-ed (RFC 4791
//! §9.7 — there is no OR), and `text-match` support varies enough between
//! iCloud, Fastmail, Google and Nextcloud that pushing predicates down would
//! give different answers on different servers.
//!
//! So the time range is what goes on the wire — it is the one filter every
//! server implements and the one that bounds how much comes back — and
//! everything else is decided here, where `and`/`or`/`not` can nest freely.

use std::borrow::Borrow;
use std::cell::OnceCell;

use async_graphql::{Enum, InputObject};

use crate::models::Event;

/// An event's `STATUS` property.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
#[graphql(name = "EventStatus")]
pub enum GqlEventStatus {
    Confirmed,
    Tentative,
    Cancelled,
}

impl GqlEventStatus {
    fn as_ical(self) -> &'static str {
        match self {
            Self::Confirmed => "CONFIRMED",
            Self::Tentative => "TENTATIVE",
            Self::Cancelled => "CANCELLED",
        }
    }
}

/// Which events to keep.
///
/// Scalar fields on one filter object are AND-ed together; `and` / `or` / `not`
/// take further filters and nest arbitrarily, so "anything with Alice on it that
/// isn't a declined standup" is one filter rather than three queries.
///
/// All text matching is case-insensitive substring matching.
#[derive(InputObject, Default, Clone, Debug)]
#[graphql(name = "EventFilter")]
pub struct EventFilter {
    /// Matches title, notes, location, categories, organizer and attendees —
    /// the same sweep `searchEvents` does.
    pub text: Option<String>,
    /// Matches the title only.
    pub summary: Option<String>,
    /// Matches the notes only.
    pub description: Option<String>,
    pub location: Option<String>,
    /// Matches any one of the event's categories.
    pub category: Option<String>,
    /// Matches any attendee's address or display name.
    pub attendee: Option<String>,
    /// Matches the organizer's address or display name.
    pub organizer: Option<String>,
    pub status: Option<GqlEventStatus>,
    pub all_day: Option<bool>,
    /// True keeps only events carrying an `RRULE`; false keeps only one-offs.
    /// On expanded results every occurrence inherits the series' rule.
    pub recurring: Option<bool>,
    /// True keeps only events with at least one attendee — a rough proxy for
    /// "a meeting rather than a block of time".
    pub has_attendees: Option<bool>,
    /// Every nested filter must match.
    pub and: Option<Vec<EventFilter>>,
    /// At least one nested filter must match.
    pub or: Option<Vec<EventFilter>>,
    /// No nested filter may match.
    pub not: Option<Vec<EventFilter>>,
}

/// Most nodes one filter may have, counting every nested `and`/`or`/`not`
/// entry. Matching costs a pass over every event per node, so this bounds a
/// request's filtering work; real filters have a handful.
pub const MAX_FILTER_NODES: usize = 1000;

/// An [`EventFilter`] ready to run: needles lowercased once, and its size
/// checked against [`MAX_FILTER_NODES`].
pub struct PreparedFilter {
    text: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    category: Option<String>,
    attendee: Option<String>,
    organizer: Option<String>,
    status: Option<GqlEventStatus>,
    all_day: Option<bool>,
    recurring: Option<bool>,
    has_attendees: Option<bool>,
    and: Vec<PreparedFilter>,
    or: Vec<PreparedFilter>,
    not: Vec<PreparedFilter>,
}

impl EventFilter {
    /// Lower every needle once and refuse a filter past [`MAX_FILTER_NODES`].
    pub fn prepare(&self) -> async_graphql::Result<PreparedFilter> {
        let mut left = MAX_FILTER_NODES;
        self.prepare_within(&mut left).ok_or_else(|| {
            async_graphql::Error::new(format!(
                "filter has more than {MAX_FILTER_NODES} parts; combine conditions or split the query"
            ))
        })
    }

    fn prepare_within(&self, left: &mut usize) -> Option<PreparedFilter> {
        *left = left.checked_sub(1)?;
        let lower = |s: &Option<String>| s.as_deref().map(str::to_lowercase);
        let nested = |list: &Option<Vec<EventFilter>>, left: &mut usize| {
            list.iter()
                .flatten()
                .map(|f| f.prepare_within(left))
                .collect::<Option<Vec<_>>>()
        };
        Some(PreparedFilter {
            text: lower(&self.text),
            summary: lower(&self.summary),
            description: lower(&self.description),
            location: lower(&self.location),
            category: lower(&self.category),
            attendee: lower(&self.attendee),
            organizer: lower(&self.organizer),
            status: self.status,
            all_day: self.all_day,
            recurring: self.recurring,
            has_attendees: self.has_attendees,
            and: nested(&self.and, left)?,
            or: nested(&self.or, left)?,
            not: nested(&self.not, left)?,
        })
    }

    #[cfg(test)]
    fn matches(&self, event: &Event) -> bool {
        self.prepare().unwrap().matches(event)
    }
}

/// An event's text fields, lowercased the first time a filter node asks for
/// each, so a filter with many nodes lowers every field at most once.
struct Lowered<'a> {
    event: &'a Event,
    summary: OnceCell<Option<String>>,
    description: OnceCell<Option<String>>,
    location: OnceCell<Option<String>>,
    categories: OnceCell<Vec<String>>,
    attendees: OnceCell<Vec<(String, Option<String>)>>,
    organizer: OnceCell<Option<(String, Option<String>)>>,
}

fn lower_person(person: &crate::models::Attendee) -> (String, Option<String>) {
    (
        person.email.to_lowercase(),
        person.name.as_deref().map(str::to_lowercase),
    )
}

fn person_matches((email, name): &(String, Option<String>), needle: &str) -> bool {
    email.contains(needle) || name.as_deref().is_some_and(|n| n.contains(needle))
}

impl<'a> Lowered<'a> {
    fn new(event: &'a Event) -> Self {
        Self {
            event,
            summary: OnceCell::new(),
            description: OnceCell::new(),
            location: OnceCell::new(),
            categories: OnceCell::new(),
            attendees: OnceCell::new(),
            organizer: OnceCell::new(),
        }
    }

    fn summary(&self) -> Option<&str> {
        self.summary
            .get_or_init(|| self.event.summary.as_deref().map(str::to_lowercase))
            .as_deref()
    }

    fn description(&self) -> Option<&str> {
        self.description
            .get_or_init(|| self.event.description.as_deref().map(str::to_lowercase))
            .as_deref()
    }

    fn location(&self) -> Option<&str> {
        self.location
            .get_or_init(|| self.event.location.as_deref().map(str::to_lowercase))
            .as_deref()
    }

    fn categories(&self) -> &[String] {
        self.categories.get_or_init(|| {
            self.event
                .categories
                .iter()
                .map(|c| c.to_lowercase())
                .collect()
        })
    }

    fn attendees(&self) -> &[(String, Option<String>)] {
        self.attendees
            .get_or_init(|| self.event.attendees.iter().map(lower_person).collect())
    }

    fn organizer(&self) -> Option<&(String, Option<String>)> {
        self.organizer
            .get_or_init(|| self.event.organizer.as_ref().map(lower_person))
            .as_ref()
    }
}

/// Substring test that treats an absent haystack as no match.
fn contains(haystack: Option<&str>, needle: &str) -> bool {
    haystack.is_some_and(|h| h.contains(needle))
}

impl PreparedFilter {
    /// Does `event` satisfy this filter?
    pub fn matches(&self, event: &Event) -> bool {
        self.matches_lowered(&Lowered::new(event))
    }

    fn matches_lowered(&self, event: &Lowered<'_>) -> bool {
        self.scalars_match(event)
            && self.and.iter().all(|f| f.matches_lowered(event))
            // An empty `or` list constrains nothing — it names no alternative to
            // fail, so treating it as "nothing matches" would be a trap.
            && (self.or.is_empty() || self.or.iter().any(|f| f.matches_lowered(event)))
            && !self.not.iter().any(|f| f.matches_lowered(event))
    }

    fn scalars_match(&self, lowered: &Lowered<'_>) -> bool {
        let event = lowered.event;
        if let Some(needle) = &self.text
            && !text_matches(lowered, needle)
        {
            return false;
        }
        if let Some(needle) = &self.summary
            && !contains(lowered.summary(), needle)
        {
            return false;
        }
        if let Some(needle) = &self.description
            && !contains(lowered.description(), needle)
        {
            return false;
        }
        if let Some(needle) = &self.location
            && !contains(lowered.location(), needle)
        {
            return false;
        }
        if let Some(needle) = &self.category
            && !lowered.categories().iter().any(|c| c == needle)
        {
            return false;
        }
        if let Some(needle) = &self.attendee
            && !lowered
                .attendees()
                .iter()
                .any(|a| person_matches(a, needle))
        {
            return false;
        }
        if let Some(needle) = &self.organizer
            && !lowered
                .organizer()
                .is_some_and(|o| person_matches(o, needle))
        {
            return false;
        }
        if let Some(status) = self.status
            && event.status.as_deref() != Some(status.as_ical())
        {
            return false;
        }
        if let Some(all_day) = self.all_day
            && event.all_day != all_day
        {
            return false;
        }
        if let Some(recurring) = self.recurring
            && event.recurrence.is_some() != recurring
        {
            return false;
        }
        if let Some(has) = self.has_attendees
            && !event.attendees.is_empty() != has
        {
            return false;
        }
        true
    }
}

/// The broad sweep behind `text` and `searchEvents`.
fn text_matches(event: &Lowered<'_>, needle: &str) -> bool {
    contains(event.summary(), needle)
        || contains(event.description(), needle)
        || contains(event.location(), needle)
        || event.categories().iter().any(|c| c.contains(needle))
        || event.attendees().iter().any(|a| person_matches(a, needle))
        || event.organizer().is_some_and(|o| person_matches(o, needle))
}

/// What to order events by.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
#[graphql(name = "EventSortProperty")]
pub enum SortProperty {
    Start,
    End,
    Summary,
    Created,
    LastModified,
}

/// One level of ordering. Pass several, most significant first.
#[derive(InputObject, Clone, Debug)]
#[graphql(name = "EventSort")]
pub struct EventSort {
    pub property: SortProperty,
    /// Default true. Ascending means earliest-first for times, A–Z for text.
    pub ascending: Option<bool>,
}

/// Order `events` in place. With no sort given, earliest start first — the order
/// an agenda wants and the one every read has always returned.
pub fn sort_events<E: Borrow<Event>>(events: &mut [E], sort: Option<&[EventSort]>) {
    let Some(sort) = sort.filter(|s| !s.is_empty()) else {
        events.sort_by_key(|e| e.borrow().start.sort_key());
        return;
    };

    // Each event's key is built once, so a text sort lowers each title once
    // rather than twice per comparison.
    events.sort_by_cached_key(|e| {
        let e = e.borrow();
        sort.iter()
            .map(|level| {
                let key = match level.property {
                    SortProperty::Start => SortKey::Time(e.start.sort_key()),
                    SortProperty::End => SortKey::Time(e.end.sort_key()),
                    SortProperty::Summary => {
                        SortKey::Text(e.summary.as_deref().unwrap_or_default().to_lowercase())
                    }
                    SortProperty::Created => SortKey::Stamp(e.created.clone()),
                    SortProperty::LastModified => SortKey::Stamp(e.last_modified.clone()),
                };
                match level.ascending.unwrap_or(true) {
                    true => Directed::Ascending(key),
                    false => Directed::Descending(std::cmp::Reverse(key)),
                }
            })
            .collect::<Vec<_>>()
    });
}

/// One level of a sort key. Levels line up across events, so the order between
/// variants never decides anything.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Time(chrono::DateTime<chrono::Utc>),
    Text(String),
    Stamp(Option<String>),
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Directed {
    Ascending(SortKey),
    Descending(std::cmp::Reverse<SortKey>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_past_the_node_cap_is_refused() {
        let leaf = || EventFilter {
            summary: Some("x".into()),
            ..Default::default()
        };
        let wide = |n: usize| EventFilter {
            or: Some((0..n).map(|_| leaf()).collect()),
            ..Default::default()
        };
        // The root counts as one node. Depth is bounded by the parser's
        // recursion limit, so breadth is what the cap is for.
        assert!(wide(MAX_FILTER_NODES - 1).prepare().is_ok());
        assert!(wide(MAX_FILTER_NODES).prepare().is_err());
    }
    use crate::models::{Attendee, EventTime};

    fn event(summary: &str) -> Event {
        Event {
            id: format!("uid-{summary}"),
            calendar: "Home".into(),
            calendar_href: "/cal/home/".into(),
            href: "/cal/home/e.ics".into(),
            resource_url: "https://dav.test/cal/home/e.ics".into(),
            etag: None,
            summary: Some(summary.into()),
            description: None,
            location: None,
            url: None,
            status: None,
            start: EventTime::default(),
            end: EventTime::default(),
            all_day: false,
            recurrence: None,
            recur_source: None,
            recurrence_id: None,
            organizer: None,
            attendees: vec![],
            categories: vec![],
            created: None,
            last_modified: None,
            sequence: 0,
        }
    }

    fn attendee(email: &str) -> Attendee {
        Attendee {
            email: email.into(),
            ..Default::default()
        }
    }

    #[test]
    fn scalars_on_one_filter_are_anded() {
        let mut e = event("Standup");
        e.location = Some("Room 4".into());
        let f = EventFilter {
            summary: Some("stand".into()),
            location: Some("room 4".into()),
            ..Default::default()
        };
        assert!(f.matches(&e));

        let f = EventFilter {
            summary: Some("stand".into()),
            location: Some("room 5".into()),
            ..Default::default()
        };
        assert!(!f.matches(&e));
    }

    #[test]
    fn or_branches_are_alternatives() {
        let f = EventFilter {
            or: Some(vec![
                EventFilter {
                    summary: Some("standup".into()),
                    ..Default::default()
                },
                EventFilter {
                    summary: Some("retro".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        assert!(f.matches(&event("Standup")));
        assert!(f.matches(&event("Retro")));
        assert!(!f.matches(&event("Lunch")));
    }

    #[test]
    fn not_excludes_without_constraining_otherwise() {
        let f = EventFilter {
            not: Some(vec![EventFilter {
                summary: Some("cancelled".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(f.matches(&event("Standup")));
        assert!(!f.matches(&event("Cancelled sync")));
    }

    #[test]
    fn branches_nest() {
        // (attendee alice OR attendee bob) AND NOT all-day
        let f = EventFilter {
            or: Some(vec![
                EventFilter {
                    attendee: Some("alice@".into()),
                    ..Default::default()
                },
                EventFilter {
                    attendee: Some("bob@".into()),
                    ..Default::default()
                },
            ]),
            not: Some(vec![EventFilter {
                all_day: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut with_alice = event("Sync");
        with_alice.attendees = vec![attendee("alice@example.com")];
        assert!(f.matches(&with_alice));

        with_alice.all_day = true;
        assert!(!f.matches(&with_alice));

        let mut with_carol = event("Sync");
        with_carol.attendees = vec![attendee("carol@example.com")];
        assert!(!f.matches(&with_carol));
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        assert!(EventFilter::default().matches(&event("Anything")));
    }

    #[test]
    fn an_empty_or_list_constrains_nothing() {
        let f = EventFilter {
            or: Some(vec![]),
            ..Default::default()
        };
        assert!(f.matches(&event("Standup")));
    }

    #[test]
    fn text_sweeps_across_fields() {
        let mut e = event("Sync");
        e.attendees = vec![attendee("alice@example.com")];
        e.categories = vec!["Personal".into()];

        for needle in ["sync", "alice@example", "personal"] {
            let f = EventFilter {
                text: Some(needle.into()),
                ..Default::default()
            };
            assert!(f.matches(&e), "{needle} should match");
        }
        let f = EventFilter {
            text: Some("nowhere".into()),
            ..Default::default()
        };
        assert!(!f.matches(&e));
    }

    #[test]
    fn status_matches_the_ical_value() {
        let mut e = event("Sync");
        e.status = Some("CANCELLED".into());
        let f = EventFilter {
            status: Some(GqlEventStatus::Cancelled),
            ..Default::default()
        };
        assert!(f.matches(&e));

        let f = EventFilter {
            status: Some(GqlEventStatus::Confirmed),
            ..Default::default()
        };
        assert!(!f.matches(&e));
    }

    #[test]
    fn recurring_distinguishes_series_from_one_offs() {
        let mut series = event("Standup");
        series.recurrence = Some("FREQ=DAILY".into());
        let one_off = event("Lunch");

        let f = EventFilter {
            recurring: Some(true),
            ..Default::default()
        };
        assert!(f.matches(&series));
        assert!(!f.matches(&one_off));
    }

    #[test]
    fn sorting_defaults_to_earliest_start() {
        let mut events = vec![event("b"), event("a")];
        events[0].start.instant = Some(chrono::DateTime::UNIX_EPOCH + chrono::Duration::hours(2));
        events[1].start.instant = Some(chrono::DateTime::UNIX_EPOCH);
        sort_events(&mut events, None);
        assert_eq!(events[0].summary.as_deref(), Some("a"));
    }

    #[test]
    fn sort_levels_break_ties_in_order() {
        let mut events = vec![event("b"), event("a"), event("c")];
        // Every start is equal, so the second level decides.
        sort_events(
            &mut events,
            Some(&[
                EventSort {
                    property: SortProperty::Start,
                    ascending: Some(true),
                },
                EventSort {
                    property: SortProperty::Summary,
                    ascending: Some(false),
                },
            ]),
        );
        let order: Vec<_> = events.iter().map(|e| e.summary.clone().unwrap()).collect();
        assert_eq!(order, ["c", "b", "a"]);
    }
}
