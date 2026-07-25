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

/// Case-insensitive substring test that treats an absent haystack as no match.
fn contains(haystack: Option<&str>, needle: &str) -> bool {
    haystack.is_some_and(|h| h.to_lowercase().contains(needle))
}

fn person_matches(person: &crate::models::Attendee, needle: &str) -> bool {
    person.email.to_lowercase().contains(needle) || contains(person.name.as_deref(), needle)
}

impl EventFilter {
    /// Does `event` satisfy this filter?
    pub fn matches(&self, event: &Event) -> bool {
        self.scalars_match(event)
            && self.and.iter().flatten().all(|f| f.matches(event))
            // An empty `or` list constrains nothing — it names no alternative to
            // fail, so treating it as "nothing matches" would be a trap.
            && self
                .or
                .as_ref()
                .is_none_or(|fs| fs.is_empty() || fs.iter().any(|f| f.matches(event)))
            && !self.not.iter().flatten().any(|f| f.matches(event))
    }

    fn scalars_match(&self, event: &Event) -> bool {
        if let Some(needle) = &self.text
            && !self.text_matches(event, &needle.to_lowercase())
        {
            return false;
        }
        if let Some(needle) = &self.summary
            && !contains(event.summary.as_deref(), &needle.to_lowercase())
        {
            return false;
        }
        if let Some(needle) = &self.description
            && !contains(event.description.as_deref(), &needle.to_lowercase())
        {
            return false;
        }
        if let Some(needle) = &self.location
            && !contains(event.location.as_deref(), &needle.to_lowercase())
        {
            return false;
        }
        if let Some(needle) = &self.category {
            let needle = needle.to_lowercase();
            if !event.categories.iter().any(|c| c.to_lowercase() == needle) {
                return false;
            }
        }
        if let Some(needle) = &self.attendee {
            let needle = needle.to_lowercase();
            if !event.attendees.iter().any(|a| person_matches(a, &needle)) {
                return false;
            }
        }
        if let Some(needle) = &self.organizer {
            let needle = needle.to_lowercase();
            if !event
                .organizer
                .as_ref()
                .is_some_and(|o| person_matches(o, &needle))
            {
                return false;
            }
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

    /// The broad sweep behind `text` and `searchEvents`.
    fn text_matches(&self, event: &Event, needle: &str) -> bool {
        contains(event.summary.as_deref(), needle)
            || contains(event.description.as_deref(), needle)
            || contains(event.location.as_deref(), needle)
            || event
                .categories
                .iter()
                .any(|c| c.to_lowercase().contains(needle))
            || event.attendees.iter().any(|a| person_matches(a, needle))
            || event
                .organizer
                .as_ref()
                .is_some_and(|o| person_matches(o, needle))
    }
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
pub fn sort_events(events: &mut [Event], sort: Option<&[EventSort]>) {
    let Some(sort) = sort.filter(|s| !s.is_empty()) else {
        events.sort_by_key(|e| e.start.sort_key());
        return;
    };

    events.sort_by(|a, b| {
        for level in sort {
            let ordering = match level.property {
                SortProperty::Start => a.start.sort_key().cmp(&b.start.sort_key()),
                SortProperty::End => a.end.sort_key().cmp(&b.end.sort_key()),
                SortProperty::Summary => a
                    .summary
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase()
                    .cmp(&b.summary.as_deref().unwrap_or_default().to_lowercase()),
                SortProperty::Created => a.created.cmp(&b.created),
                SortProperty::LastModified => a.last_modified.cmp(&b.last_modified),
            };
            let ordering = if level.ascending.unwrap_or(true) {
                ordering
            } else {
                ordering.reverse()
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
}

#[cfg(test)]
mod tests {
    use super::*;
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
