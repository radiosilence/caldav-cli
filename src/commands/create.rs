use crate::models::{Attendee, EventFields, Output};

/// The event-shaped flags shared by `create` and `update`.
///
/// Every field is optional: on `create` the required ones are checked by the
/// client, and on `update` an absent field means "leave it alone".
#[derive(Debug, Clone, Default)]
pub struct EventArgs {
    pub summary: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    /// Length in minutes, used when `end` is absent.
    pub duration: Option<i64>,
    pub all_day: bool,
    /// IANA zone for naive start/end values, e.g. `Europe/London`.
    pub tz: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    /// `CONFIRMED`, `TENTATIVE`, or `CANCELLED`.
    pub status: Option<String>,
    /// Raw RRULE, e.g. `FREQ=WEEKLY;BYDAY=MO`.
    pub recurrence: Option<String>,
    /// `jane@x.test` or `Jane Doe <jane@x.test>`. Empty means "leave as-is".
    pub attendees: Vec<String>,
    /// Empty means "leave as-is".
    pub categories: Vec<String>,
}

impl EventArgs {
    /// Parsed attendees, or `None` when the flag wasn't given at all — the
    /// distinction the merge in `update_event` relies on.
    pub fn parsed_attendees(&self) -> Option<Vec<Attendee>> {
        if self.attendees.is_empty() {
            return None;
        }
        Some(
            self.attendees
                .iter()
                .map(|s| parse_attendee_spec(s))
                .collect(),
        )
    }

    pub fn parsed_categories(&self) -> Option<Vec<String>> {
        if self.categories.is_empty() {
            return None;
        }
        Some(self.categories.clone())
    }

    /// Borrowed view for the client. `attendees`/`categories` are passed in
    /// because [`EventFields`] borrows them and they must outlive the call.
    pub fn fields<'a>(
        &'a self,
        attendees: Option<&'a [Attendee]>,
        categories: Option<&'a [String]>,
    ) -> EventFields<'a> {
        EventFields {
            summary: self.summary.as_deref(),
            description: self.description.as_deref(),
            location: self.location.as_deref(),
            url: self.url.as_deref(),
            status: self.status.as_deref(),
            start: self.start.as_deref(),
            end: self.end.as_deref(),
            duration_minutes: self.duration,
            // Only assert all-day when the flag was actually passed; otherwise
            // let the start value decide (a bare date is all-day).
            all_day: self.all_day.then_some(true),
            tzid: self.tz.as_deref(),
            recurrence: self.recurrence.as_deref(),
            attendees,
            categories,
        }
    }
}

/// `Jane Doe <jane@x.test>` or a bare address. Shared with the GraphQL layer
/// so both front ends accept attendees written the same way.
pub fn parse_attendee_spec(spec: &str) -> Attendee {
    let spec = spec.trim();
    match (spec.find('<'), spec.strip_suffix('>')) {
        (Some(open), Some(_)) => Attendee {
            email: spec[open + 1..spec.len() - 1].trim().to_string(),
            name: Some(spec[..open].trim().to_string()).filter(|s| !s.is_empty()),
            ..Default::default()
        },
        _ => Attendee {
            email: spec.to_string(),
            ..Default::default()
        },
    }
}

/// Create an event. `--summary` and `--start` are required.
pub async fn create_event(calendar: Option<&str>, args: &EventArgs) -> anyhow::Result<()> {
    let client = super::make_client()?;
    let calendar = super::default_calendar(calendar);
    let attendees = args.parsed_attendees();
    let categories = args.parsed_categories();
    let fields = args.fields(attendees.as_deref(), categories.as_deref());

    let event = client.create_event(calendar.as_deref(), &fields).await?;
    Output::success(event).print();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_address() {
        let a = parse_attendee_spec("jane@x.test");
        assert_eq!(a.email, "jane@x.test");
        assert!(a.name.is_none());
    }

    #[test]
    fn parses_name_and_address() {
        let a = parse_attendee_spec("Jane Doe <jane@x.test>");
        assert_eq!(a.email, "jane@x.test");
        assert_eq!(a.name.as_deref(), Some("Jane Doe"));
    }

    #[test]
    fn parses_angle_brackets_without_a_name() {
        let a = parse_attendee_spec("<jane@x.test>");
        assert_eq!(a.email, "jane@x.test");
        assert!(a.name.is_none());
    }

    #[test]
    fn absent_lists_stay_none_so_updates_dont_wipe_them() {
        let args = EventArgs::default();
        assert!(args.parsed_attendees().is_none());
        assert!(args.parsed_categories().is_none());
    }

    #[test]
    fn all_day_is_only_asserted_when_the_flag_is_set() {
        assert_eq!(EventArgs::default().fields(None, None).all_day, None);
        let args = EventArgs {
            all_day: true,
            ..Default::default()
        };
        assert_eq!(args.fields(None, None).all_day, Some(true));
    }
}
