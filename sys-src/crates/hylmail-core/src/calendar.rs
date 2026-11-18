//! `text/calendar` handling for invitations.
//!
//! A meeting request arrives as a `text/calendar` part next to the mail body.
//! The reading pane has to summarise it without a full calendar
//! implementation: who invites to what, when, where, and whether the message is
//! a new invitation, an answer or a cancellation.
//!
//! [`parse`] turns the ICS text into [`CalendarSummary`]; everything else in
//! this module is about presenting that summary in German.  Writing calendar
//! data back (accepting an invitation) is the composer's job in a later phase -
//! the original ICS is kept verbatim in
//! [`crate::models::MessageBody::calendar_ics`], so nothing is lost.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use icalendar::{Calendar, Component, EventLike};

use crate::error::{Error, Result};

/// The `METHOD` of a calendar object (RFC 5546).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarMethod {
    /// A new invitation.
    Request,
    /// An answer to an invitation.
    Reply,
    /// A cancellation of a previously sent invitation.
    Cancel,
    /// A counter proposal.
    Counter,
    /// An informational publication.
    Publish,
    /// A request to refresh a stale copy.
    Refresh,
    /// Anything else; the raw value is kept.
    Unknown(String),
}

impl CalendarMethod {
    /// Parses the `METHOD` property.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "REQUEST" => Self::Request,
            "REPLY" => Self::Reply,
            "CANCEL" => Self::Cancel,
            "COUNTER" => Self::Counter,
            "PUBLISH" => Self::Publish,
            "REFRESH" => Self::Refresh,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// The wire value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Request => "REQUEST",
            Self::Reply => "REPLY",
            Self::Cancel => "CANCEL",
            Self::Counter => "COUNTER",
            Self::Publish => "PUBLISH",
            Self::Refresh => "REFRESH",
            Self::Unknown(raw) => raw,
        }
    }

    /// German label for the invitation banner.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Request => "Einladung",
            Self::Reply => "Antwort",
            Self::Cancel => "Absage",
            Self::Counter => "Gegenvorschlag",
            Self::Publish => "Termin",
            Self::Refresh => "Terminaktualisierung",
            Self::Unknown(_) => "Kalendereintrag",
        }
    }
}

/// A date-time as it appeared in the calendar.
///
/// iCalendar distinguishes three forms and they must not be silently converted
/// into one another: a floating time belongs to the reader's zone, a `Z` time
/// is UTC, and a `TZID` time needs the zone definition the sender should have
/// shipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarInstant {
    /// A `VALUE=DATE` value: a whole day, no time of day.
    Date(NaiveDate),
    /// A date-time without a zone: interpreted in the reader's zone (UTC here).
    Floating(NaiveDateTime),
    /// A date-time ending in `Z`: unambiguous.
    Utc(DateTime<Utc>),
    /// A date-time with a `TZID` reference.
    Zoned {
        /// The wall clock time in that zone.
        local: NaiveDateTime,
        /// The zone identifier.
        tzid: String,
    },
}

impl CalendarInstant {
    /// Whether the value covers a whole day rather than an instant.
    #[must_use]
    pub const fn is_date_only(&self) -> bool {
        matches!(self, Self::Date(_))
    }

    /// The instant in UTC, when the message states one unambiguously.
    ///
    /// Floating times are *not* converted: assuming a zone here is exactly the
    /// bug that makes an invitation show the wrong hour.  A `TZID` value needs
    /// the zone database, which this client does not carry.
    #[must_use]
    pub fn to_utc(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Utc(value) => Some(*value),
            Self::Date(date) => date.and_hms_opt(0, 0, 0).map(|naive| naive.and_utc()),
            Self::Floating(value) => Some(value.and_utc()),
            Self::Zoned { .. } => None,
        }
    }

    /// German rendering for the invitation banner.
    #[must_use]
    pub fn format_german(&self) -> String {
        match self {
            Self::Date(date) => format!("{} (ganztägig)", date.format("%d.%m.%Y")),
            Self::Floating(value) => value.format("%d.%m.%Y %H:%M").to_string(),
            Self::Utc(value) => format!("{} UTC", value.format("%d.%m.%Y %H:%M")),
            Self::Zoned { local, tzid } => {
                format!("{} ({tzid})", local.format("%d.%m.%Y %H:%M"))
            }
        }
    }
}

impl From<icalendar::DatePerhapsTime> for CalendarInstant {
    fn from(value: icalendar::DatePerhapsTime) -> Self {
        match value {
            icalendar::DatePerhapsTime::Date(date) => Self::Date(date),
            icalendar::DatePerhapsTime::DateTime(calendar) => match calendar {
                icalendar::CalendarDateTime::Floating(local) => Self::Floating(local),
                icalendar::CalendarDateTime::Utc(value) => Self::Utc(value),
                icalendar::CalendarDateTime::WithTimezone { date_time, tzid } => Self::Zoned {
                    local: date_time,
                    tzid,
                },
            },
        }
    }
}

/// One attendee of an invitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarAttendee {
    /// Address without the `mailto:` prefix, lower-cased.
    pub email: String,
    /// `CN` parameter, if the sender provided one.
    pub name: Option<String>,
    /// Participation status, e.g. `ACCEPTED`.
    pub status: Option<String>,
    /// Whether a reply is expected.
    pub rsvp: bool,
    /// Whether this attendee is the organiser (role `CHAIR`).
    pub chair: bool,
}

impl CalendarAttendee {
    /// Label for the invitation view.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.email)
    }
}

/// The parts of an invitation the reading pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarSummary {
    /// `METHOD` of the calendar object.
    pub method: CalendarMethod,
    /// `UID`, the identity of the appointment.
    pub uid: Option<String>,
    /// `SUMMARY`.
    pub summary: Option<String>,
    /// `DESCRIPTION`.
    pub description: Option<String>,
    /// `LOCATION`.
    pub location: Option<String>,
    /// `ORGANIZER`, without the `mailto:` prefix.
    pub organizer: Option<String>,
    /// `ATTENDEE` entries.
    pub attendees: Vec<CalendarAttendee>,
    /// `DTSTART`.
    pub start: Option<CalendarInstant>,
    /// `DTEND`.
    pub end: Option<CalendarInstant>,
    /// `SEQUENCE`, which increases on every update.
    pub sequence: u32,
    /// `STATUS`, e.g. `CANCELLED`.
    pub status: Option<String>,
    /// `true` when the appointment repeats (`RRULE` present).
    pub recurring: bool,
}

impl CalendarSummary {
    /// `true` for a new or updated invitation.
    #[must_use]
    pub fn is_invitation(&self) -> bool {
        matches!(
            self.method,
            CalendarMethod::Request | CalendarMethod::Publish | CalendarMethod::Refresh
        )
    }

    /// `true` when the appointment was cancelled.
    #[must_use]
    pub fn is_cancellation(&self) -> bool {
        self.method == CalendarMethod::Cancel
            || self
                .status
                .as_deref()
                .is_some_and(|status| status.eq_ignore_ascii_case("CANCELLED"))
    }

    /// `true` when this is an answer from an attendee.
    #[must_use]
    pub fn is_reply(&self) -> bool {
        self.method == CalendarMethod::Reply
    }

    /// Subject of the invitation, never empty.
    #[must_use]
    pub fn title(&self) -> &str {
        self.summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or("(kein Titel)")
    }

    /// When the appointment starts, and ends if the sender provided it.
    #[must_use]
    pub fn when(&self) -> Option<String> {
        let start = self.start.as_ref()?;
        let mut text = start.format_german();
        if let Some(end) = &self.end {
            if start.is_date_only() && end.is_date_only() {
                // A one-day event needs no range at all.
                if start != end {
                    text = format!("{text} - {}", end.format_german());
                }
                return Some(text);
            }
            // For a meeting that ends on the same day only the time of the end
            // matters - but only when both values share a zone form, so that a
            // floating start and a UTC end never get silently merged.
            if let Some(time) = same_day_end(start, end) {
                return Some(format!("{text} - {}", time.format("%H:%M")));
            }
            text = format!("{text} - {}", end.format_german());
        }
        Some(text)
    }

    /// One line for the banner above the message.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = format!("{}: {}", self.method.label(), self.title());
        if let Some(when) = self.when() {
            text.push_str(" – ");
            text.push_str(&when);
        }
        if let Some(location) = self
            .location
            .as_deref()
            .filter(|location| !location.trim().is_empty())
        {
            text.push_str(", ");
            text.push_str(location);
        }
        if self.recurring {
            text.push_str(" (wiederkehrend)");
        }
        text
    }

    /// The organiser line, if the message names one.
    #[must_use]
    pub fn organizer_line(&self) -> Option<String> {
        match (&self.organizer, self.method.as_str()) {
            (Some(organizer), "REQUEST") => Some(format!("Einladung von {organizer}")),
            (Some(organizer), _) => Some(format!("Organisator: {organizer}")),
            (None, _) => None,
        }
    }
}

/// Parses a `text/calendar` payload.
///
/// A message may carry several calendar objects; the first one wins, which is
/// what other mail clients do for a reply with a single `VEVENT`.
///
/// # Errors
///
/// Returns [`Error::MalformedResponse`] when the payload is not a calendar at
/// all.  A calendar without events is *not* an error: the summary simply has no
/// `UID`, no start and no attendees.
pub fn parse(ics: &str) -> Result<CalendarSummary> {
    if !ics.contains("BEGIN:VCALENDAR") {
        return Err(Error::MalformedResponse {
            protocol: "iCalendar",
            message: "the part does not contain a BEGIN:VCALENDAR block".to_string(),
        });
    }
    let calendar: Calendar = ics
        .parse()
        .map_err(|message: String| Error::MalformedResponse {
            protocol: "iCalendar",
            message,
        })?;
    let method = calendar
        .property_value("METHOD")
        .map_or(CalendarMethod::Request, CalendarMethod::parse);

    let event = calendar.events().next();
    let Some(event) = event else {
        return Ok(CalendarSummary {
            method,
            uid: None,
            summary: None,
            description: None,
            location: None,
            organizer: None,
            attendees: Vec::new(),
            start: None,
            end: None,
            sequence: 0,
            status: None,
            recurring: false,
        });
    };

    let attendees = event
        .get_attendees()
        .into_iter()
        .map(|attendee| CalendarAttendee {
            email: strip_mailto(&attendee.cal_address),
            name: attendee.cn.clone(),
            status: attendee
                .part_stat
                .as_ref()
                .map(|status| format!("{status:?}").to_ascii_uppercase()),
            rsvp: attendee.rsvp.unwrap_or(false),
            chair: attendee
                .role
                .as_ref()
                .is_some_and(|role| matches!(role, icalendar::Role::Chair)),
        })
        .collect();

    Ok(CalendarSummary {
        method,
        uid: event.get_uid().map(str::to_string),
        summary: event.get_summary().map(str::to_string),
        description: event.get_description().map(str::to_string),
        location: event.get_location().map(str::to_string),
        organizer: event.property_value("ORGANIZER").map(strip_mailto),
        attendees,
        start: event.get_start().map(CalendarInstant::from),
        end: event.get_end().map(CalendarInstant::from),
        sequence: event.get_sequence().unwrap_or(0),
        status: event
            .get_status()
            .map(|status| format!("{status:?}").to_ascii_uppercase()),
        recurring: event.property_value("RRULE").is_some(),
    })
}

/// The end time of an appointment that ends on the same day it starts.
///
/// Returns `None` when the two values are not comparable without a zone
/// database: a `TZID` start with a UTC end, or two different zones.
fn same_day_end(start: &CalendarInstant, end: &CalendarInstant) -> Option<NaiveTime> {
    match (start, end) {
        (CalendarInstant::Floating(from), CalendarInstant::Floating(to))
            if from.date() == to.date() =>
        {
            Some(to.time())
        }
        (CalendarInstant::Utc(from), CalendarInstant::Utc(to))
            if from.date_naive() == to.date_naive() =>
        {
            Some(to.time())
        }
        (
            CalendarInstant::Zoned {
                local: from,
                tzid: from_zone,
            },
            CalendarInstant::Zoned {
                local: to,
                tzid: to_zone,
            },
        ) if from.date() == to.date() && from_zone == to_zone => Some(to.time()),
        _ => None,
    }
}

/// Removes the `mailto:` prefix and normalises the address.
fn strip_mailto(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = trimmed
        .strip_prefix("mailto:")
        .or_else(|| trimmed.strip_prefix("MAILTO:"))
        .unwrap_or(trimmed);
    without_prefix.trim().to_ascii_lowercase()
}

/// Participation status of an iTIP reply (RFC 5546 `PARTSTAT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartStat {
    /// The user will attend.
    Accepted,
    /// The user will not attend.
    Declined,
    /// The user may attend.
    Tentative,
}

impl PartStat {
    /// The wire value of the `PARTSTAT` parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "ACCEPTED",
            Self::Declined => "DECLINED",
            Self::Tentative => "TENTATIVE",
        }
    }

    /// German label of the button that produces this status.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Accepted => "Zusagen",
            Self::Declined => "Absagen",
            Self::Tentative => "Mit Vorbehalt",
        }
    }
}

impl CalendarSummary {
    /// `true` when `address` is one of the attendees of this invitation.
    ///
    /// Used to decide whether an accept/decline card is offered: an organiser
    /// answers nobody, and a message that merely mentions an organiser is not an
    /// invitation to that user.
    #[must_use]
    pub fn is_attendee(&self, address: &str) -> bool {
        let needle = strip_mailto(address);
        self.attendees
            .iter()
            .any(|attendee| attendee.email.eq_ignore_ascii_case(&needle))
    }

    /// `true` when this account may answer the invitation (`FR-CAL-2`).
    ///
    /// A request whose organiser is the account itself is not a candidate; an
    /// invitation without attendees is still answerable, because some servers
    /// omit the `ATTENDEE` list from the copy they deliver.
    #[must_use]
    pub fn can_reply(&self, address: &str) -> bool {
        let needle = strip_mailto(address);
        self.is_invitation()
            && !self.is_cancellation()
            && self
                .organizer
                .as_deref()
                .is_some_and(|organizer| !organizer.eq_ignore_ascii_case(&needle))
            && (self.attendees.is_empty() || self.is_attendee(&needle))
    }
}

/// Builds the `METHOD:REPLY` calendar object one attendee sends back.
///
/// The original invitation is reused verbatim except for the two things a reply
/// must change: `METHOD` becomes `REPLY`, and the attendee's line carries the
/// new `PARTSTAT` and no `RSVP`.  Keeping the original means the `UID`, the
/// `DTSTART`/`DTEND` and every property this client does not understand travel
/// back untouched - the reply refers to the very same appointment.
///
/// # Errors
///
/// [`Error::MalformedResponse`] when the payload is not a calendar at all.
pub fn build_reply(ics: &str, attendee: &str, status: PartStat) -> Result<String> {
    if !ics.contains("BEGIN:VCALENDAR") {
        return Err(Error::MalformedResponse {
            protocol: "iCalendar",
            message: "the part does not contain a BEGIN:VCALENDAR block".to_string(),
        });
    }
    let attendee = strip_mailto(attendee);
    let lines: Vec<&str> = ics
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let has_method = lines
        .iter()
        .any(|line| line.to_ascii_uppercase().starts_with("METHOD"));

    let mut out = String::with_capacity(ics.len() + 128);
    let mut attendee_done = false;
    for line in lines {
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("METHOD") {
            out.push_str("METHOD:REPLY\r\n");
            continue;
        }
        if upper.starts_with("ATTENDEE") {
            if attendee_matches(line, &attendee) {
                out.push_str(&rewrite_attendee(line, status));
                out.push_str("\r\n");
                attendee_done = true;
            } else {
                out.push_str(line);
                out.push_str("\r\n");
            }
            continue;
        }
        if upper.starts_with("BEGIN:VCALENDAR") {
            out.push_str(line);
            out.push_str("\r\n");
            if !has_method {
                out.push_str("METHOD:REPLY\r\n");
            }
            continue;
        }
        if upper.starts_with("END:VEVENT") && !attendee_done {
            // No attendee line for this user: add a minimal one.
            out.push_str(&format!(
                "ATTENDEE;PARTSTAT={}:mailto:{attendee}\r\n",
                status.as_str()
            ));
            attendee_done = true;
            out.push_str(line);
            out.push_str("\r\n");
            continue;
        }
        if line.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    Ok(out)
}

/// `true` when an `ATTENDEE` line names `attendee`.
fn attendee_matches(line: &str, attendee: &str) -> bool {
    let value = line
        .split_once(':')
        .map(|(_, value)| value)
        .unwrap_or_default();
    strip_mailto(value) == attendee
}

/// Rewrites the `PARTSTAT` of an attendee line (`RSVP` is dropped).
///
/// The parameter block ends at the first `:` that is not inside a quoted
/// parameter value, because the property value itself is a URI that contains a
/// colon (`mailto:`).
fn rewrite_attendee(line: &str, status: PartStat) -> String {
    let split = property_value_split(line);
    let (params, value) = match split {
        Some((params, value)) => (params, value),
        None => (line, ""),
    };
    let name_and_params: Vec<&str> = params.split(';').collect();
    let mut rebuilt = String::from(name_and_params.first().copied().unwrap_or("ATTENDEE"));
    for param in name_and_params.iter().skip(1) {
        let key = param
            .split('=')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_uppercase();
        if key == "PARTSTAT" || key == "RSVP" {
            continue;
        }
        rebuilt.push(';');
        rebuilt.push_str(param);
    }
    rebuilt.push_str(";PARTSTAT=");
    rebuilt.push_str(status.as_str());
    rebuilt.push(':');
    rebuilt.push_str(value);
    rebuilt
}

/// Splits an iCalendar content line at the `:` that separates the name and
/// parameters from the value, ignoring colons inside a quoted parameter.
fn property_value_split(line: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    for (index, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some((&line[..index], &line[index + 1..])),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITATION: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Example//Exchange//DE\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:040000008200E00074C5B7101A82E00800000000\r\n\
SEQUENCE:2\r\n\
DTSTAMP:20261112T120000Z\r\n\
DTSTART;TZID=Europe/Berlin:20261120T140000\r\n\
DTEND;TZID=Europe/Berlin:20261120T150000\r\n\
SUMMARY:Quartalsbesprechung\r\n\
LOCATION:Raum 4.12\r\n\
DESCRIPTION:Agenda siehe Anhang\\nBitte pünktlich\r\n\
ORGANIZER;CN=Ada Lovelace:mailto:ada@example.org\r\n\
ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:bob@example.org\r\n\
ATTENDEE;ROLE=CHAIR;PARTSTAT=NEEDS-ACTION:mailto:chair@example.org\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn invitation_fields_are_extracted() {
        let summary = parse(INVITATION).unwrap();
        assert_eq!(summary.method, CalendarMethod::Request);
        assert_eq!(
            summary.uid.as_deref(),
            Some("040000008200E00074C5B7101A82E00800000000")
        );
        assert_eq!(summary.title(), "Quartalsbesprechung");
        assert_eq!(summary.location.as_deref(), Some("Raum 4.12"));
        assert_eq!(summary.organizer.as_deref(), Some("ada@example.org"));
        assert_eq!(summary.sequence, 2);
        assert!(summary.is_invitation());
        assert!(!summary.is_cancellation());
        assert!(!summary.recurring);
        assert_eq!(summary.attendees.len(), 2);
        assert_eq!(summary.attendees[0].email, "bob@example.org");
        assert_eq!(summary.attendees[0].name.as_deref(), Some("Bob"));
        assert!(summary.attendees[0].rsvp);
        assert!(summary.attendees[1].chair);
        assert!(summary.description.is_some_and(|d| d.contains("pünktlich")));
    }

    #[test]
    fn zoned_times_are_not_silently_converted() {
        let summary = parse(INVITATION).unwrap();
        let start = summary.start.clone().unwrap();
        assert!(matches!(start, CalendarInstant::Zoned { .. }));
        assert!(start.to_utc().is_none());
        assert!(start.format_german().contains("Europe/Berlin"));
        assert!(!start.format_german().contains("UTC"));
    }

    #[test]
    fn the_when_line_shows_a_range_on_the_same_day() {
        let summary = parse(INVITATION).unwrap();
        let when = summary.when().unwrap();
        assert!(when.contains("20.11.2026 14:00"));
        assert!(when.ends_with("- 15:00"), "{when}");
    }

    #[test]
    fn utc_times_are_converted_and_labelled() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
                   DTSTART:20261120T140000Z\r\nSUMMARY:Call\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let summary = parse(ics).unwrap();
        let start = summary.start.clone().unwrap();
        assert_eq!(
            start.to_utc().unwrap().to_rfc3339(),
            "2026-11-20T14:00:00+00:00"
        );
        assert!(start.format_german().ends_with("UTC"));
    }

    #[test]
    fn all_day_events_are_labelled() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
                   DTSTART;VALUE=DATE:20261120\r\nSUMMARY:Feiertag\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let summary = parse(ics).unwrap();
        let start = summary.start.clone().unwrap();
        assert!(start.is_date_only());
        assert_eq!(start.format_german(), "20.11.2026 (ganztägig)");
        assert!(summary.describe().contains("ganztägig"));
    }

    #[test]
    fn floating_times_are_kept_as_written() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20261120T090000\r\n\
                   SUMMARY:Frühstück\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let summary = parse(ics).unwrap();
        assert_eq!(summary.start.unwrap().format_german(), "20.11.2026 09:00");
    }

    #[test]
    fn cancellations_are_recognised() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:x\r\n\
                   STATUS:CANCELLED\r\nSUMMARY:Quartalsbesprechung\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let summary = parse(ics).unwrap();
        assert!(summary.is_cancellation());
        assert!(!summary.is_invitation());
        assert!(summary.describe().starts_with("Absage:"));
    }

    #[test]
    fn a_cancelled_status_alone_is_enough() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSTATUS:CANCELLED\r\nSUMMARY:x\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(parse(ics).unwrap().is_cancellation());
    }

    #[test]
    fn replies_are_recognised() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:x\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let summary = parse(ics).unwrap();
        assert!(summary.is_reply());
        assert_eq!(summary.method.label(), "Antwort");
    }

    #[test]
    fn recurring_events_are_flagged() {
        let ics =
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:Standup\r\n\
                   RRULE:FREQ=DAILY;COUNT=5\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let summary = parse(ics).unwrap();
        assert!(summary.recurring);
        assert!(summary.describe().contains("wiederkehrend"));
    }

    #[test]
    fn an_empty_calendar_is_not_an_error() {
        let summary = parse("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n").unwrap();
        assert_eq!(summary.title(), "(kein Titel)");
        assert!(summary.when().is_none());
        assert!(summary.uid.is_none());
    }

    #[test]
    fn something_that_is_not_a_calendar_is_an_error() {
        let error = parse("nur Text").unwrap_err();
        assert!(matches!(error, Error::MalformedResponse { .. }));
        assert!(parse("BEGIN:VEVENT\r\nEND:VEVENT\r\n").is_err());
    }

    #[test]
    fn organiser_line_depends_on_the_method() {
        let summary = parse(INVITATION).unwrap();
        assert_eq!(
            summary.organizer_line().as_deref(),
            Some("Einladung von ada@example.org")
        );
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\n\
                   ORGANIZER:mailto:ada@example.org\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            parse(ics).unwrap().organizer_line().as_deref(),
            Some("Organisator: ada@example.org")
        );
    }

    #[test]
    fn method_parsing_round_trips() {
        for method in [
            CalendarMethod::Request,
            CalendarMethod::Reply,
            CalendarMethod::Cancel,
            CalendarMethod::Counter,
            CalendarMethod::Publish,
            CalendarMethod::Refresh,
        ] {
            assert_eq!(CalendarMethod::parse(method.as_str()), method);
        }
        assert_eq!(
            CalendarMethod::parse("weird"),
            CalendarMethod::Unknown("WEIRD".to_string())
        );
    }

    #[test]
    fn attendee_labels_prefer_the_name() {
        let attendee = CalendarAttendee {
            email: "bob@example.org".to_string(),
            name: Some("Bob".to_string()),
            status: None,
            rsvp: false,
            chair: false,
        };
        assert_eq!(attendee.display_name(), "Bob");
        let anonymous = CalendarAttendee {
            name: Some("  ".to_string()),
            ..attendee
        };
        assert_eq!(anonymous.display_name(), "bob@example.org");
    }

    #[test]
    fn mailto_prefixes_are_stripped_case_insensitively() {
        assert_eq!(strip_mailto("mailto:A@B.de"), "a@b.de");
        assert_eq!(strip_mailto("MAILTO:a@b.de"), "a@b.de");
        assert_eq!(strip_mailto(" a@b.de "), "a@b.de");
    }

    const REQUEST: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//DE\r\n\
        METHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:4711@example.org\r\n\
        DTSTART:20261120T140000Z\r\nDTEND:20261120T150000Z\r\nSUMMARY:Standup\r\n\
        ORGANIZER;CN=Ada:mailto:ada@example.org\r\n\
        ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.org\r\n\
        END:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn a_reply_flips_the_method_and_sets_the_partstat() {
        let reply = build_reply(REQUEST, "bob@example.org", PartStat::Accepted).unwrap();
        assert!(reply.contains("METHOD:REPLY"), "{reply}");
        assert!(!reply.to_ascii_uppercase().contains("METHOD:REQUEST"));
        assert!(reply.contains("PARTSTAT=ACCEPTED"), "{reply}");
        assert!(!reply.to_ascii_uppercase().contains("RSVP=TRUE"));
        // The identity of the appointment survives untouched.
        assert!(reply.contains("UID:4711@example.org"));
        assert!(reply.contains("DTSTART:20261120T140000Z"));
        assert!(reply.contains("mailto:bob@example.org"));
        assert!(reply.contains("mailto:ada@example.org"));
    }

    #[test]
    fn a_declined_reply_keeps_a_single_partstat() {
        let reply = build_reply(REQUEST, "BOB@example.org", PartStat::Declined).unwrap();
        assert!(reply.contains("PARTSTAT=DECLINED"), "{reply}");
        assert_eq!(reply.matches("PARTSTAT=").count(), 1, "{reply}");
        // The organiser's line has no PARTSTAT and is left alone.
        assert!(reply.contains("ORGANIZER;CN=Ada:mailto:ada@example.org"));
    }

    #[test]
    fn an_attendee_missing_from_the_invitation_is_added() {
        let ics = REQUEST.replace(
            "ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.org\r\n",
            "",
        );
        let reply = build_reply(&ics, "cara@example.org", PartStat::Tentative).unwrap();
        assert!(reply.contains("PARTSTAT=TENTATIVE"), "{reply}");
        assert!(reply.contains("mailto:cara@example.org"), "{reply}");
    }

    #[test]
    fn a_missing_method_is_added_to_the_reply() {
        let ics = REQUEST.replace("METHOD:REQUEST\r\n", "");
        let reply = build_reply(&ics, "bob@example.org", PartStat::Accepted).unwrap();
        assert!(reply.contains("METHOD:REPLY"), "{reply}");
    }

    #[test]
    fn a_broken_calendar_cannot_be_answered() {
        assert!(build_reply("not a calendar", "bob@example.org", PartStat::Accepted).is_err());
    }

    #[test]
    fn can_reply_only_for_an_invitee() {
        let summary = parse(REQUEST).unwrap();
        assert!(summary.can_reply("bob@example.org"));
        // The organiser answers nobody.
        assert!(!summary.can_reply("ada@example.org"));
        // An attendee of a cancellation is not invited any more.
        let cancelled = REQUEST.replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert!(!parse(&cancelled).unwrap().can_reply("bob@example.org"));
        // And somebody who is not on the list is not answering.
        assert!(!summary.can_reply("cara@example.org"));
    }

    #[test]
    fn a_quoted_parameter_with_a_colon_is_not_the_value_separator() {
        let line = "ATTENDEE;CN=\"Ada: Boss\";RSVP=TRUE:mailto:ada@example.org";
        let (params, value) = property_value_split(line).unwrap();
        assert_eq!(params, "ATTENDEE;CN=\"Ada: Boss\";RSVP=TRUE");
        assert_eq!(value, "mailto:ada@example.org");
    }
}
