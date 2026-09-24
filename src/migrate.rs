//! Moves a calendar [`tum_google_sync`](crate::tum_google_sync) kept in Google Calendar over to an
//! Apple (iCloud) calendar, which [`tum_apple_sync`](crate::tum_apple_sync) keeps in sync from
//! then on.
//!
//! - Events the Google sync created are recognized by the `uid:` marker at the end of their
//!   description. They're stored under the same uid the Apple sync uses, so it keeps updating
//!   them just like the Google sync did.
//! - Whatever was changed by hand in Google Calendar comes along: notes in the description, a seat
//!   number in the location, reminders and colors. Events deleted in Google stay deleted.
//! - Events added to the calendar by hand are copied as well, recurring ones including their
//!   moved and cancelled occurrences.
//! - Nothing is ever deleted, neither in Google nor in the Apple Calendar. Events that already
//!   exist in the Apple Calendar are skipped unless [`MigrationOptions::overwrite`] is set.
//!
//! See [migrate_google_to_apple] to run it.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Berlin;
use futures::{stream, StreamExt};
use google_calendar3::api::{Event, EventDateTime};
use ical::parser::{ical::component::IcalEvent, Component};
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use reqwest::Url;

use crate::{
    apple::{
        escape_text, format_utc, is_exam, location_hint, object_name, parse_stored_event,
        push_line, push_property, AppleCalendar, PRODID,
    },
    convert_to_digits, google_hub, read_backup, GoogleCalendarHub,
};

const BERLIN: &str = "Europe/Berlin";

/// Written into every calendar object with local times. Recurring events need them, otherwise
/// they would move by an hour whenever daylight saving time starts or ends.
const BERLIN_TIMEZONE: &str = "BEGIN:VTIMEZONE\r\n\
    TZID:Europe/Berlin\r\n\
    BEGIN:DAYLIGHT\r\n\
    TZOFFSETFROM:+0100\r\n\
    TZOFFSETTO:+0200\r\n\
    TZNAME:CEST\r\n\
    DTSTART:19700329T020000\r\n\
    RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\n\
    END:DAYLIGHT\r\n\
    BEGIN:STANDARD\r\n\
    TZOFFSETFROM:+0200\r\n\
    TZOFFSETTO:+0100\r\n\
    TZNAME:CET\r\n\
    DTSTART:19701025T030000\r\n\
    RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\n\
    END:STANDARD\r\n\
    END:VTIMEZONE\r\n";

/// `uid:…` at the end of the description of every event the Google sync created, with the digits
/// written as bold math digits (see [`convert_to_non_digits`](crate::convert_to_non_digits))
static MARKER_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"uid:([^\s<]+)").unwrap());
static MARKER_TAG_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<small>\s*uid:[^<]*</small>").unwrap());
static RULE_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<hr[^>]*>").unwrap());
static ANCHOR_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<a\s([^>]*)>(.*?)</a>").unwrap());
/// Attributes Google Calendar adds to links whenever a description gets edited
static EXTRA_ATTRIBUTE_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\s+(?:target|rel|class|style|data-[\w-]+)="[^"]*""#).unwrap());
static TAG_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(/?)([a-zA-Z][a-zA-Z0-9]*)([^>]*)>").unwrap());
static HREF_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)href="([^"]*)""#).unwrap());
static ENTITY_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"&(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z]+);").unwrap());
/// Older versions of the Google sync stripped the backslash of `\n` line breaks and left the
/// `n` behind, e.g. `InfoeventnAnsprechpartner: nName,nName`
static LOST_BREAK_BEFORE_CONTACT_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"n(Ansprechpartner)").unwrap());
static LOST_BREAK_AFTER_PUNCTUATION_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)(^|[,;:]\s?)n(\p{Lu})").unwrap());
/// Same for locations, e.g. `Online: VideokonferenznZoom etc.`
static LOST_BREAK_IN_WORD_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\p{Ll})n(\p{Lu})").unwrap());
/// Exactly what the Google sync writes, to tell whether a description was edited by hand
static UNTOUCHED_DESCRIPTION_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?s)^.*?(?:<a href="https://nav\.tum\.de/search\?q=.*?">Wo ist das\?</a><br>|Online<br>|<a href="https://www\.moodle\.tum\.de/my/">Online auf Moodle</a><br>)<br><hr><small>uid:[^<]+</small>$"#,
    )
    .unwrap()
});

/// Controls what [migrate_google_to_apple] does.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    /// Without it, nothing is written - the migration only reports what it would do
    pub apply: bool,
    /// Replace events which already exist in the Apple Calendar instead of skipping them
    pub overwrite: bool,
    /// Additionally save every calendar object as an `.ics` file into this folder
    pub dump_directory: Option<PathBuf>,
    /// The backup of the [ICSWatcher](crate::ICSWatcher), which tells which events are still in
    /// the ICS calendar
    pub backup_name: String,
}

impl Default for MigrationOptions {
    fn default() -> Self {
        MigrationOptions {
            apply: false,
            overwrite: false,
            dump_directory: None,
            backup_name: String::from("TUM Calendar"),
        }
    }
}

// ---------------------------------------------------------------------------
// Calendar objects
// ---------------------------------------------------------------------------

/// How a point in time gets written
#[derive(Debug, Clone, Copy, PartialEq)]
enum Time {
    Utc(DateTime<Utc>),
    /// Written in Europe/Berlin local time
    Local(DateTime<Utc>),
    Date(NaiveDate),
}

impl Time {
    fn from_google(value: &EventDateTime, local: bool) -> Option<Self> {
        if let Some(date) = value.date {
            return Some(Time::Date(date));
        }

        let date_time = value.date_time?;
        Some(if local {
            Time::Local(date_time)
        } else {
            Time::Utc(date_time)
        })
    }

    fn push_to(&self, calendar_object: &mut String, name: &str) {
        match self {
            Time::Utc(date_time) => {
                push_property(calendar_object, name, &[], &format_utc(*date_time))
            }
            Time::Local(date_time) => push_property(
                calendar_object,
                name,
                &[(String::from("TZID"), vec![String::from(BERLIN)])],
                &date_time
                    .with_timezone(&Berlin)
                    .format("%Y%m%dT%H%M%S")
                    .to_string(),
            ),
            Time::Date(date) => push_property(
                calendar_object,
                name,
                &[(String::from("VALUE"), vec![String::from("DATE")])],
                &date.format("%Y%m%d").to_string(),
            ),
        }
    }
}

/// A single `VEVENT`. Text values are plain text, they get escaped while writing.
#[derive(Debug)]
struct VEvent {
    uid: String,
    recurrence_id: Option<Time>,
    start: Time,
    end: Time,
    /// Complete `RRULE` / `EXDATE` / `RDATE` lines, the way Google hands them out
    recurrence: Vec<String>,
    exdates: Vec<Time>,
    summary: Option<String>,
    location: Option<String>,
    description: Option<String>,
    url: Option<String>,
    status: Option<String>,
    categories: Option<String>,
    color: Option<&'static str>,
    transparent: bool,
    created: Option<DateTime<Utc>>,
    last_modified: Option<DateTime<Utc>>,
    /// Minutes before the start
    alarms: Vec<i32>,
}

impl VEvent {
    /// Takes over everything that doesn't need any conversion. The description is left to the
    /// caller.
    fn from_google(uid: &str, event: &Event, local: bool) -> Option<Self> {
        let start = Time::from_google(event.start.as_ref()?, local)?;

        Some(VEvent {
            uid: uid.to_string(),
            recurrence_id: None,
            start,
            end: event
                .end
                .as_ref()
                .and_then(|end| Time::from_google(end, local))
                .unwrap_or(start),
            recurrence: event.recurrence.clone().unwrap_or_default(),
            exdates: Vec::new(),
            summary: event
                .summary
                .clone()
                .filter(|summary| !summary.trim().is_empty()),
            location: event
                .location
                .clone()
                .filter(|location| !location.trim().is_empty()),
            description: None,
            url: event.source.as_ref().and_then(|source| source.url.clone()),
            status: event.status.as_deref().map(str::to_uppercase),
            categories: None,
            color: event.color_id.as_deref().and_then(color_name),
            transparent: event.transparency.as_deref() == Some("transparent"),
            created: event.created,
            last_modified: event.updated,
            alarms: event
                .reminders
                .as_ref()
                .and_then(|reminders| reminders.overrides.as_ref())
                .map(|overrides| {
                    overrides
                        .iter()
                        .filter_map(|reminder| reminder.minutes)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn uses_local_time(&self) -> bool {
        [self.start, self.end]
            .into_iter()
            .chain(self.recurrence_id)
            .chain(self.exdates.iter().copied())
            .any(|time| matches!(time, Time::Local(_)))
            || self.recurrence.iter().any(|line| line.contains("TZID="))
    }

    fn push_to(&self, calendar_object: &mut String) {
        calendar_object.push_str("BEGIN:VEVENT\r\n");
        push_property(calendar_object, "UID", &[], &escape_text(&self.uid));
        push_property(calendar_object, "DTSTAMP", &[], &format_utc(Utc::now()));
        if let Some(recurrence_id) = self.recurrence_id {
            recurrence_id.push_to(calendar_object, "RECURRENCE-ID");
        }
        self.start.push_to(calendar_object, "DTSTART");
        self.end.push_to(calendar_object, "DTEND");
        for line in &self.recurrence {
            push_line(calendar_object, line);
        }
        for exdate in &self.exdates {
            exdate.push_to(calendar_object, "EXDATE");
        }

        let text_properties = [
            ("SUMMARY", &self.summary),
            ("LOCATION", &self.location),
            ("DESCRIPTION", &self.description),
            ("URL", &self.url),
            ("STATUS", &self.status),
            ("CATEGORIES", &self.categories),
        ];
        for (name, value) in text_properties {
            if let Some(value) = value {
                push_property(calendar_object, name, &[], &escape_text(value));
            }
        }

        if let Some(color) = self.color {
            // Apple Calendar shows RFC 7986 colors on supported clients
            push_property(calendar_object, "COLOR", &[], color);
        }
        if self.transparent {
            push_property(calendar_object, "TRANSP", &[], "TRANSPARENT");
        }
        if let Some(created) = self.created {
            push_property(calendar_object, "CREATED", &[], &format_utc(created));
        }
        push_property(calendar_object, "SEQUENCE", &[], "0");
        push_property(
            calendar_object,
            "LAST-MODIFIED",
            &[],
            &format_utc(self.last_modified.unwrap_or_else(Utc::now)),
        );

        for minutes in &self.alarms {
            calendar_object.push_str("BEGIN:VALARM\r\n");
            push_property(calendar_object, "ACTION", &[], "DISPLAY");
            push_property(
                calendar_object,
                "DESCRIPTION",
                &[],
                &escape_text(self.summary.as_deref().unwrap_or("Erinnerung")),
            );
            push_property(calendar_object, "TRIGGER", &[], &format!("-PT{minutes}M"));
            calendar_object.push_str("END:VALARM\r\n");
        }

        calendar_object.push_str("END:VEVENT\r\n");
    }
}

fn calendar_object(events: &[VEvent]) -> String {
    let mut calendar_object = String::new();

    calendar_object.push_str("BEGIN:VCALENDAR\r\n");
    push_property(&mut calendar_object, "VERSION", &[], "2.0");
    push_property(&mut calendar_object, "PRODID", &[], PRODID);
    push_property(&mut calendar_object, "CALSCALE", &[], "GREGORIAN");
    if events.iter().any(VEvent::uses_local_time) {
        calendar_object.push_str(BERLIN_TIMEZONE);
    }
    for event in events {
        event.push_to(&mut calendar_object);
    }
    calendar_object.push_str("END:VCALENDAR\r\n");

    calendar_object
}

/// Google's event colors as the closest CSS color, which is what RFC 7986 `COLOR` takes
fn color_name(color_id: &str) -> Option<&'static str> {
    Some(match color_id {
        "1" => "mediumslateblue", // Lavender
        "2" => "mediumseagreen",  // Sage
        "3" => "darkorchid",      // Grape
        "4" => "salmon",          // Flamingo
        "5" => "gold",            // Banana
        "6" => "orangered",       // Tangerine
        "7" => "deepskyblue",     // Peacock
        "8" => "dimgray",         // Graphite
        "9" => "royalblue",       // Blueberry
        "10" => "seagreen",       // Basil
        "11" => "tomato",         // Tomato
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// HTML descriptions
// ---------------------------------------------------------------------------

fn decode_entities(text: &str) -> String {
    ENTITY_REGEX
        .replace_all(text, |captures: &Captures| {
            let entity = &captures[1];
            let decoded = match entity {
                "nbsp" => Some(' '),
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix('#')
                    .and_then(|number| match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse().ok(),
                    })
                    .and_then(char::from_u32),
            };
            decoded
                .map(String::from)
                .unwrap_or_else(|| captures[0].to_string())
        })
        .replace('\u{a0}', " ")
}

/// Trims all lines, drops empty ones at the start and the end and collapses runs of empty lines
fn tidy_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Starts a new line, unless the current one is still empty (or just a bullet)
fn soft_line_break(text: &mut String) {
    let current_line = text.rsplit('\n').next().unwrap_or_default().trim();
    if !current_line.is_empty() && current_line != "•" {
        text.push('\n');
    }
}

/// Turns the HTML Google Calendar keeps descriptions in into plain text. Plain text stays as
/// it is.
fn html_to_text(html: &str) -> String {
    let html = html.replace("\r\n", "\n");
    let mut text = String::new();
    // Where the text of each open link starts, along with where the link points to
    let mut links: Vec<(usize, Option<String>)> = Vec::new();
    let mut position = 0;

    for tag in TAG_REGEX.captures_iter(&html) {
        let whole = tag.get(0).expect("the whole match always exists");
        text.push_str(&decode_entities(&html[position..whole.start()]));
        position = whole.end();

        let closing = !tag[1].is_empty();
        match tag[2].to_ascii_lowercase().as_str() {
            "br" | "hr" => text.push('\n'),
            "li" => {
                soft_line_break(&mut text);
                if !closing {
                    text.push_str("• ");
                }
            }
            "p" | "div" | "ul" | "ol" | "table" | "tr" | "blockquote" | "h1" | "h2" | "h3"
            | "h4" | "h5" | "h6" => soft_line_break(&mut text),
            "a" if !closing => links.push((
                text.len(),
                HREF_REGEX
                    .captures(&tag[3])
                    .map(|href| decode_entities(&href[1])),
            )),
            "a" => {
                if let Some((start, Some(href))) = links.pop() {
                    let label = text[start..].trim();
                    if !label.is_empty() && !href.is_empty() && label != href {
                        text.push_str(&format!(" ({href})"));
                    }
                }
            }
            _ => (),
        }
    }
    text.push_str(&decode_entities(&html[position..]));

    tidy_lines(&text)
}

/// The room a "Wo ist das?" link searches for. When a description gets edited, Google Calendar
/// cuts links off at the first quote - what's left is still good enough for a search.
fn room_from_link(attributes: &str) -> Option<String> {
    let attributes = EXTRA_ATTRIBUTE_REGEX.replace_all(attributes, "");
    let href = attributes
        .trim()
        .strip_prefix("href=\"")?
        .strip_suffix('"')?;

    let link = Url::parse(&decode_entities(href)).ok()?;
    let room = link.query_pairs().find(|(key, _)| key == "q")?.1;
    let room = convert_to_digits(room.trim());

    (!room.is_empty()).then_some(room)
}

// ---------------------------------------------------------------------------
// Converting events
// ---------------------------------------------------------------------------

/// The uid the Google sync noted at the end of the description, if the event is one of its own.
fn synced_uid(event: &Event) -> Option<String> {
    let marker = MARKER_REGEX.captures(event.description.as_deref()?)?;

    Some(convert_to_digits(&marker[1]).replace('|', "@tum.de"))
}

/// The Google sync wrote the digits of rooms as bold math digits. Locations are a single line,
/// so line breaks typed into them become commas.
fn convert_synced_location(location: &str) -> String {
    let location = convert_to_digits(location)
        .replace("\r\n", ", ")
        .replace(['\n', '\r'], ", ")
        .replace(r"\n", ", ");

    // Only online events had line breaks in their location, which older versions of the
    // Google sync turned into a stray `n`
    let location = if location.to_lowercase().starts_with("online") {
        LOST_BREAK_IN_WORD_REGEX
            .replace_all(&location, "${1}, ${2}")
            .into_owned()
    } else {
        location
    };

    location.trim().to_string()
}

/// Turns a description the Google sync wrote into the one the Apple sync writes: plain text,
/// without the uid marker and with the "Wo ist das?" link as a line of its own at the end.
/// Everything that was added by hand stays where it is.
fn convert_synced_description(
    description: &str,
    location: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let html = MARKER_TAG_REGEX.replace_all(description, "");
    let html = MARKER_REGEX.replace_all(&html, "");
    let html = RULE_REGEX.replace_all(&html, "");

    let mut linked_room = None;
    let mut on_moodle = false;
    let html = ANCHOR_REGEX.replace_all(&html, |captures: &Captures| {
        match decode_entities(&captures[2]).trim() {
            "Wo ist das?" => {
                linked_room = room_from_link(&captures[1]);
                String::new()
            }
            "Online auf Moodle" => {
                on_moodle = true;
                String::new()
            }
            _ => captures[0].to_string(),
        }
    });

    let online = on_moodle || location.to_lowercase().contains("online");
    let mut lines: Vec<String> = html_to_text(&html).lines().map(String::from).collect();

    // Descriptions which lost their HTML still have the text of the link
    let unlinked = match lines.iter().rposition(|line| line == "Wo ist das?") {
        Some(index) => {
            lines.remove(index);
            true
        }
        None => false,
    };
    if online {
        if let Some(index) = lines
            .iter()
            .rposition(|line| line == "Online" || line == "Online auf Moodle")
        {
            lines.remove(index);
        }
    }

    let mut text = lines.join("\n");
    if text.contains("nAnsprechpartner") {
        text = LOST_BREAK_BEFORE_CONTACT_REGEX
            .replace_all(&text, "\n${1}")
            .into_owned();
        text = LOST_BREAK_AFTER_PUNCTUATION_REGEX
            .replace_all(&text, "${1}\n${2}")
            .into_owned();
    }
    let text = tidy_lines(&text);

    let hint = match linked_room {
        // Cut off at the quote in the room name - the location still has all of it
        Some(room) if room.ends_with(',') && location.starts_with(&room) => {
            Some(location_hint(location)?)
        }
        Some(room) => Some(location_hint(&room)?),
        None if online || unlinked => Some(location_hint(location)?),
        None => None,
    };

    let description = [Some(text), hint]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<String>>()
        .join("\n");

    Ok((!description.is_empty()).then_some(description))
}

/// Converts an event the Google sync created into what the Apple sync would have written,
/// including everything that was changed by hand.
fn synced_event(
    uid: &str,
    event: &Event,
) -> Result<Option<VEvent>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(mut vevent) = VEvent::from_google(uid, event, false) else {
        return Ok(None);
    };

    let location = event
        .location
        .as_deref()
        .map(convert_synced_location)
        .unwrap_or_default();
    vevent.description = match &event.description {
        Some(description) => convert_synced_description(description, &location)?,
        None => None,
    };
    vevent.location = (!location.is_empty()).then_some(location);

    if vevent
        .summary
        .as_deref()
        .is_some_and(|summary| summary.contains("Prüfung"))
    {
        // Same as the Apple sync marks exams
        vevent.categories = Some(String::from("Prüfung"));
        vevent.color = Some("tomato");
    }

    Ok(Some(vevent))
}

/// What was changed by hand in an event the Google sync created, for the report.
fn hand_made_changes(event: &Event) -> Vec<&'static str> {
    let mut changes = Vec::new();

    if event
        .description
        .as_deref()
        .is_some_and(|description| !UNTOUCHED_DESCRIPTION_REGEX.is_match(description))
    {
        changes.push("description");
    }
    // The Google sync only ever wrote bold digits
    if event.location.as_deref().is_some_and(|location| {
        location.chars().any(|character| character.is_ascii_digit())
            && !location.contains("Kein Ort")
    }) {
        changes.push("location");
    }
    if event
        .reminders
        .as_ref()
        .and_then(|reminders| reminders.overrides.as_ref())
        .is_some_and(|overrides| !overrides.is_empty())
    {
        changes.push("reminders");
    }
    // Exams are colored by the Google sync itself
    let is_exam = event
        .summary
        .as_deref()
        .is_some_and(|summary| summary.contains("Prüfung"));
    if event
        .color_id
        .as_deref()
        .is_some_and(|color| color != "11" || !is_exam)
    {
        changes.push("color");
    }

    changes
}

/// An event that was added by hand: a single one, or a recurring one along with its moved and
/// cancelled occurrences.
#[derive(Debug, Default)]
struct Series {
    event: Option<Event>,
    exceptions: Vec<Event>,
}

/// Converts an event that was added by hand. The first `VEvent` is the event itself, the others
/// are its moved occurrences.
fn series_events(series: &Series) -> Result<Vec<VEvent>, String> {
    let event = series
        .event
        .as_ref()
        .ok_or("its recurring event is missing")?;
    let uid = event
        .i_cal_uid
        .clone()
        .or_else(|| event.id.clone())
        .ok_or("it has no uid")?;
    let start = event.start.as_ref().ok_or("it has no start")?;

    // Recurring events are written in local time, so that they don't move by an hour whenever
    // daylight saving time starts or ends
    let repeats_at_a_time = event.recurrence.is_some() && start.date.is_none();
    let local = repeats_at_a_time && start.time_zone.as_deref() == Some(BERLIN);
    if repeats_at_a_time && !local {
        return Err(format!(
            "it repeats in the time zone {:?}, which isn't supported",
            start.time_zone
        ));
    }

    let mut main = VEvent::from_google(&uid, event, local).ok_or("it has no start")?;
    main.description = event
        .description
        .as_deref()
        .map(html_to_text)
        .filter(|description| !description.is_empty());

    let mut occurrences = Vec::new();
    for exception in &series.exceptions {
        let original_start = exception
            .original_start_time
            .as_ref()
            .and_then(|original_start| Time::from_google(original_start, local))
            .ok_or("an occurrence has no original start")?;

        if exception.status.as_deref() == Some("cancelled") {
            main.exdates.push(original_start);
            continue;
        }

        let mut occurrence =
            VEvent::from_google(&uid, exception, local).ok_or("an occurrence has no start")?;
        occurrence.description = exception
            .description
            .as_deref()
            .map(html_to_text)
            .filter(|description| !description.is_empty());
        // Only the event itself carries the rule
        occurrence.recurrence.clear();
        occurrence.recurrence_id = Some(original_start);
        occurrences.push(occurrence);
    }

    occurrences.insert(0, main);
    Ok(occurrences)
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

async fn fetch_events(
    hub: &GoogleCalendarHub,
    calendar_id: &str,
) -> Result<Vec<Event>, Box<dyn std::error::Error + Send + Sync>> {
    let mut events = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        // Recurring events come as one event with its exceptions instead of every occurrence
        let mut call = hub
            .events()
            .list(calendar_id)
            .single_events(false)
            .show_deleted(false)
            .max_results(2500);
        if let Some(page_token) = &page_token {
            call = call.page_token(page_token);
        }

        let (_, page) = call.doit().await?;
        events.extend(page.items.unwrap_or_default());

        page_token = page.next_page_token;
        if page_token.is_none() {
            return Ok(events);
        }
    }
}

/// Date and title of an event, for the report
fn label(event: &Event) -> String {
    let date = event
        .start
        .as_ref()
        .or(event.original_start_time.as_ref())
        .and_then(|start| {
            start.date.or_else(|| {
                start
                    .date_time
                    .map(|date_time| date_time.with_timezone(&Berlin).date_naive())
            })
        })
        .map(|date| date.to_string())
        .unwrap_or_else(|| String::from("????-??-??"));

    format!(
        "{date} {}",
        event.summary.as_deref().unwrap_or("(no title)")
    )
}

fn feed_label(event: &IcalEvent) -> String {
    let property = |name: &str| {
        event
            .get_property(name)
            .and_then(|property| property.value.clone())
            .unwrap_or_default()
    };
    let start = property("DTSTART");
    let date = start
        .get(0..8)
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y%m%d").ok())
        .map(|date| date.to_string())
        .unwrap_or(start);

    format!("{date} {}", property("SUMMARY").replace('\\', ""))
}

/// A calendar object ready to be uploaded
struct Planned {
    uid: String,
    name: String,
    label: String,
    calendar_object: String,
}

/// Uploads a calendar object. Returns whether it was written - without `overwrite`, existing
/// events are left alone.
async fn write(
    apple_calendar: &AppleCalendar,
    planned: &Planned,
    overwrite: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let url = apple_calendar.object_url(&planned.uid)?;

    if overwrite {
        apple_calendar
            .put_object(&url, &planned.calendar_object, None)
            .await
            .map(|()| true)
    } else {
        apple_calendar
            .create_object(&url, &planned.calendar_object)
            .await
    }
}

/// Copies the Google calendar that [`tum_google_sync`](crate::tum_google_sync) kept over to an
/// Apple Calendar. See the [module documentation](self) for what it does exactly.
///
/// Run it from the folder the [ICSWatcher](crate::ICSWatcher) runs in, while the watcher is
/// stopped: it needs the Google credentials in `.secrets` and the backup in `.backups`.
/// Without an `apple_calendar` or without [`MigrationOptions::apply`], it only reports what it
/// would do.
pub async fn migrate_google_to_apple(
    google_calendar_id: &str,
    apple_calendar: Option<&AppleCalendar>,
    options: &MigrationOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hub = google_hub().await?;
    let events = fetch_events(&hub, google_calendar_id).await?;
    let event_count = events.len();

    // Sort the events into the ones the Google sync created and the ones added by hand
    let mut synced: HashMap<String, Event> = HashMap::new();
    let mut duplicates = 0;
    let mut series: HashMap<String, Series> = HashMap::new();
    for event in events {
        if let Some(recurring_event_id) = event.recurring_event_id.clone() {
            series
                .entry(recurring_event_id)
                .or_default()
                .exceptions
                .push(event);
        } else if let Some(uid) = synced_uid(&event) {
            // Should the Google sync ever have created an event twice, the newer one wins
            if let Some(existing) = synced.get(&uid) {
                duplicates += 1;
                if existing.updated >= event.updated {
                    continue;
                }
            }
            synced.insert(uid, event);
        } else if let Some(id) = event.id.clone() {
            series.entry(id).or_default().event = Some(event);
        }
    }

    let feed = read_backup(&options.backup_name).ok();
    let mut planned = Vec::new();
    let mut problems = Vec::new();
    let mut changes = Vec::new();

    for (uid, event) in &synced {
        let label = label(event);
        let changed = hand_made_changes(event);
        if !changed.is_empty() {
            changes.push(format!("{label}: {}", changed.join(", ")));
        }

        match synced_event(uid, event) {
            Ok(Some(vevent)) => planned.push(Planned {
                uid: uid.clone(),
                name: object_name(uid),
                label,
                calendar_object: calendar_object(&[vevent]),
            }),
            Ok(None) => problems.push(format!("{label}: it has no start")),
            Err(error) => problems.push(format!("{label}: {error}")),
        }
    }

    let mut recurring = 0;
    let mut occurrences = 0;
    for series in series.values() {
        let label = series
            .event
            .as_ref()
            .or(series.exceptions.first())
            .map(label)
            .unwrap_or_default();

        match series_events(series) {
            Ok(vevents) => {
                if !vevents[0].recurrence.is_empty() {
                    recurring += 1;
                    occurrences += series.exceptions.len();
                }
                planned.push(Planned {
                    uid: vevents[0].uid.clone(),
                    name: object_name(&vevents[0].uid),
                    label,
                    calendar_object: calendar_object(&vevents),
                });
            }
            Err(reason) => problems.push(format!("{label}: skipped, {reason}")),
        }
    }

    planned.sort_by(|a, b| a.label.cmp(&b.label));
    changes.sort();
    problems.sort();

    let mut names = HashSet::new();
    if let Some(clash) = planned.iter().find(|planned| !names.insert(&planned.name)) {
        return Err(format!("Two events would be stored as {}", clash.name).into());
    }

    // Report
    println!("Google calendar: {event_count} events");
    println!(
        "  {} created by the Google sync{}",
        synced.len(),
        match &feed {
            Some(feed) => format!(
                ", {} of them still in the ICS calendar",
                synced.keys().filter(|uid| feed.contains_key(*uid)).count()
            ),
            None => String::new(),
        }
    );
    if duplicates > 0 {
        println!("  {duplicates} of them created twice, only the newer copy is migrated");
    }
    println!(
        "  {} added by hand, {recurring} of them recurring with {occurrences} moved or cancelled occurrences",
        series.len()
    );

    if let Some(feed) = &feed {
        let mut deleted: Vec<String> = feed
            .iter()
            .filter(|(uid, _)| !synced.contains_key(*uid))
            .map(|(_, event)| {
                let never_synced = event
                    .get_property("DESCRIPTION")
                    .and_then(|property| property.value.as_deref())
                    .is_some_and(|description| description.contains("Videoübertragung aus"));
                if never_synced {
                    format!("{} (video transmission, never synced)", feed_label(event))
                } else {
                    feed_label(event)
                }
            })
            .collect();
        deleted.sort();

        println!(
            "\nIn the ICS calendar, but not in Google - these won't be created ({}):",
            deleted.len()
        );
        for label in &deleted {
            println!("  {label}");
        }
    }

    println!(
        "\nChanges made in Google Calendar, which are carried over ({}):",
        changes.len()
    );
    for change in &changes {
        println!("  {change}");
    }

    if !problems.is_empty() {
        println!("\nProblems ({}):", problems.len());
        for problem in &problems {
            println!("  {problem}");
        }
    }

    if let Some(directory) = &options.dump_directory {
        fs::create_dir_all(directory)?;
        for planned in &planned {
            fs::write(directory.join(&planned.name), &planned.calendar_object)?;
        }
        println!(
            "\nSaved {} calendar objects to {}",
            planned.len(),
            directory.display()
        );
    }

    let Some(apple_calendar) = apple_calendar else {
        println!(
            "\n{} calendar objects are ready - configure the Apple Calendar to write them.",
            planned.len()
        );
        return Ok(());
    };

    let existing = apple_calendar.object_names().await?;
    let already_there = planned
        .iter()
        .filter(|planned| existing.contains(&planned.name))
        .count();
    let to_write: Vec<&Planned> = planned
        .iter()
        .filter(|planned| options.overwrite || !existing.contains(&planned.name))
        .collect();

    println!(
        "\nApple Calendar: {} events in there already, {already_there} of them from this migration",
        existing.len()
    );
    println!(
        "{} calendar objects to {}, {} skipped as they exist already",
        to_write.len(),
        if options.overwrite {
            "write or replace"
        } else {
            "create"
        },
        planned.len() - to_write.len()
    );

    if !options.apply {
        println!("\nNothing was written - run again with --apply to migrate.");
        return Ok(());
    }

    // If the first event already gets rejected, all others would be as well
    let Some((first, rest)) = to_write.split_first() else {
        println!("\nNothing to write.");
        return Ok(());
    };
    write(apple_calendar, first, options.overwrite)
        .await
        .map_err(|error| format!("{}: {error}", first.label))?;

    let results: Vec<_> = stream::iter(rest.iter().copied())
        .map(|planned| async move {
            (
                planned.label.as_str(),
                write(apple_calendar, planned, options.overwrite).await,
            )
        })
        .buffer_unordered(4)
        .collect()
        .await;

    let mut written = 1;
    let mut skipped = 0;
    let mut failed = Vec::new();
    for (label, result) in results {
        match result {
            Ok(true) => written += 1,
            Ok(false) => skipped += 1,
            Err(error) => failed.push(format!("{label}: {error}")),
        }
    }
    failed.sort();

    println!(
        "\nWrote {written} calendar objects, {skipped} skipped as they appeared in the meantime, {} failed",
        failed.len()
    );
    for failure in &failed {
        println!("  {failure}");
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} events couldn't be written - run the migration again to retry them",
            failed.len()
        )
        .into())
    }
}

/// An exam waiting to be moved into the exam calendar
struct Exam {
    name: String,
    url: Url,
    etag: Option<String>,
    calendar_object: String,
    label: String,
}

/// Moves the exams that are in the main calendar over into the exam calendar (see
/// [`AppleCalendar::with_exam_calendar`]) - the ones [migrate_google_to_apple] put there, or
/// the ones created before there was an exam calendar.
///
/// Every exam is copied first and only deleted from the main calendar if it didn't change in
/// the meantime, so nothing gets lost. Without `apply`, it only reports what it would do.
pub async fn move_exams_to_exam_calendar(
    calendar: &AppleCalendar,
    apply: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let calendar_urls = calendar.calendar_urls(false);
    let [calendar_url, exam_calendar_url] = calendar_urls.as_slice() else {
        return Err("No exam calendar configured".into());
    };

    let names = calendar.object_names().await?;
    let fetched: Vec<_> = stream::iter(names)
        .map(|name| async move {
            let url = calendar_url.join(&name)?;
            let stored = calendar.fetch_object(&url).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((name, url, stored))
        })
        .buffer_unordered(4)
        .collect()
        .await;

    let mut exams = Vec::new();
    let mut problems = Vec::new();
    let mut event_count = 0;
    for result in fetched {
        match result {
            Ok((name, url, Some((calendar_object, etag)))) => {
                event_count += 1;
                let Some(stored) = parse_stored_event(&calendar_object) else {
                    problems.push(format!("{name}: couldn't be read"));
                    continue;
                };
                if !is_exam(&stored.event) {
                    continue;
                }

                let property = |name: &str| {
                    stored
                        .event
                        .get_property(name)
                        .and_then(|property| property.value.clone())
                        .unwrap_or_default()
                };
                let start = property("DTSTART");
                let date = start
                    .get(0..8)
                    .and_then(|date| NaiveDate::parse_from_str(date, "%Y%m%d").ok())
                    .map(|date| date.to_string())
                    .unwrap_or(start);

                exams.push(Exam {
                    label: format!("{date} {}", property("SUMMARY").replace('\\', "")),
                    name,
                    url,
                    etag,
                    calendar_object,
                });
            }
            // Deleted in the meantime
            Ok((_, _, None)) => (),
            Err(error) => problems.push(error.to_string()),
        }
    }
    exams.sort_by(|a, b| a.label.cmp(&b.label));

    println!(
        "Main calendar: {event_count} events, {} of them exams",
        exams.len()
    );
    for exam in &exams {
        println!("  {}", exam.label);
    }
    if !problems.is_empty() {
        println!("\nProblems ({}):", problems.len());
        for problem in &problems {
            println!("  {problem}");
        }
    }

    if !apply {
        println!("\nNothing was moved - run again with --apply to move the exams.");
        return Ok(());
    }

    let results: Vec<_> = stream::iter(&exams)
        .map(|exam| async move {
            let result = async {
                // Overwrites a copy an earlier, interrupted run might have left behind
                calendar
                    .put_object(
                        &exam_calendar_url.join(&exam.name)?,
                        &exam.calendar_object,
                        None,
                    )
                    .await?;
                calendar
                    .delete_object(&exam.url, exam.etag.as_deref())
                    .await
            }
            .await;
            (exam.label.as_str(), result)
        })
        .buffer_unordered(4)
        .collect()
        .await;

    let mut failed: Vec<String> = results
        .into_iter()
        .filter_map(|(label, result)| result.err().map(|error| format!("{label}: {error}")))
        .collect();
    failed.sort();

    println!(
        "\nMoved {} exams, {} failed",
        exams.len() - failed.len(),
        failed.len()
    );
    for failure in &failed {
        println!("  {failure}");
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} exams couldn't be moved - run it again to retry them",
            failed.len()
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use google_calendar3::api::{EventReminder, EventReminders, EventSource};

    fn at(value: &str) -> Option<EventDateTime> {
        Some(EventDateTime {
            date_time: Some(value.parse().expect("valid timestamp")),
            time_zone: Some(String::from(BERLIN)),
            date: None,
        })
    }

    fn on(year: i32, month: u32, day: u32) -> Option<EventDateTime> {
        Some(EventDateTime {
            date: NaiveDate::from_ymd_opt(year, month, day),
            ..Default::default()
        })
    }

    fn reminders(minutes: &[i32]) -> Option<EventReminders> {
        Some(EventReminders {
            use_default: Some(false),
            overrides: Some(
                minutes
                    .iter()
                    .map(|minutes| EventReminder {
                        method: Some(String::from("popup")),
                        minutes: Some(*minutes),
                    })
                    .collect(),
            ),
        })
    }

    /// Undoes the line folding, so that assertions don't depend on where lines are split
    fn unfolded(calendar_object: &str) -> String {
        calendar_object.replace("\r\n ", "")
    }

    const MARKER: &str = "<br><hr><small>uid:𝟖𝟗𝟏𝟗𝟓𝟔𝟎𝟎𝟓|XR</small>";

    #[test]
    fn reads_the_uid_marker() {
        let event = Event {
            description: Some(format!("Vorlesung<br>Online<br>{MARKER}")),
            ..Default::default()
        };
        assert_eq!(synced_uid(&event).as_deref(), Some("891956005@tum.deXR"));

        let event = Event {
            description: Some(String::from("<small>uid:𝟖𝟗𝟏𝟐𝟗𝟐𝟓𝟗𝟎|𝟔𝟎𝟔𝟓𝟒𝟒</small>")),
            ..Default::default()
        };
        assert_eq!(
            synced_uid(&event).as_deref(),
            Some("891292590@tum.de606544")
        );

        let event = Event {
            description: Some(String::from("Mittagessen")),
            ..Default::default()
        };
        assert_eq!(synced_uid(&event), None);
    }

    #[test]
    fn turns_html_into_text() {
        let text = html_to_text(
            r#"<h4></h4><ul><li><h4><span><strong>Row:</strong>&nbsp;<span style="font-weight: normal;">A5</span></span></h4></li><li><span><strong>Seat:&nbsp;</strong>15</span>&nbsp;</li></ul><br>&nbsp;<br>Mehr auf <a href="https://example.com/info">der Info-Seite</a> &amp; so"#,
        );

        assert_eq!(
            text,
            "• Row: A5\n• Seat: 15\n\nMehr auf der Info-Seite (https://example.com/info) & so"
        );
    }

    #[test]
    fn keeps_plain_text() {
        assert_eq!(
            html_to_text("Zug um 11:43\nGleis 12 & 13"),
            "Zug um 11:43\nGleis 12 & 13"
        );
    }

    #[test]
    fn keeps_a_note_before_the_link() {
        let room = "MW 0001, Hörsaal (5510.EG.001)";
        let description = convert_synced_description(
            "<b>Sitzplatz</b>: H / 5\u{a0}<br>\u{a0}<br><a href=\"https://nav.tum.de/search?q=MW+𝟎𝟎𝟎𝟏,+Hörsaal+(𝟓𝟓𝟏𝟎.EG.𝟎𝟎𝟏)\" target=\"_blank\">Wo ist das?</a><br><hr /><small>uid:𝟖𝟗𝟐𝟒𝟓𝟓𝟕𝟖𝟐|XR</small>",
            room,
        )
        .unwrap();

        assert_eq!(
            description,
            Some(format!(
                "Sitzplatz: H / 5\n{}",
                location_hint(room).unwrap()
            ))
        );
    }

    #[test]
    fn keeps_a_note_after_the_link() {
        // The quotes in the room name end the link early in the HTML
        let room = r#"003, Hörsaal 2, "Interims II" (5416.01.003)"#;
        let description = convert_synced_description(
            &format!("<a href=\"https://nav.tum.de/search?q={room}\">Wo ist das?</a><br>\u{a0}<br><b>Seat 96</b><br><hr /><small>uid:𝟖𝟗𝟏𝟗𝟓𝟓𝟑𝟕𝟔|XR</small>"),
            room,
        )
        .unwrap();

        assert_eq!(
            description,
            Some(format!("Seat 96\n{}", location_hint(room).unwrap()))
        );
    }

    #[test]
    fn repairs_line_breaks_older_versions_lost() {
        let description = convert_synced_description(
            "nAnsprechpartner: nJank Georg,nThoma Tobias<br>Online<br>(verschoben)\u{a0}<br><hr /><small>uid:𝟖𝟗𝟐𝟐𝟗𝟐𝟕𝟑𝟗|𝟔𝟓𝟎𝟎𝟓𝟑</small>",
            "Online",
        )
        .unwrap();

        assert_eq!(
            description.as_deref(),
            Some("Ansprechpartner:\nJank Georg,\nThoma Tobias\n(verschoben)\nOnline")
        );
    }

    #[test]
    fn keeps_notes_in_the_location() {
        assert_eq!(
            convert_synced_location(
                "MW 𝟐𝟎𝟎𝟏 Empore Rudolf-Diesel-Hörsaal (𝟓𝟓𝟏𝟎.𝟎𝟐.𝟎𝟎𝟏) Row: B\nSeat: 29"
            ),
            "MW 2001 Empore Rudolf-Diesel-Hörsaal (5510.02.001) Row: B, Seat: 29"
        );
        assert_eq!(
            convert_synced_location(r"Hörsaal 1 Row: B\nSeat: 29"),
            "Hörsaal 1 Row: B, Seat: 29"
        );
    }

    #[test]
    fn repairs_line_breaks_in_online_locations() {
        assert_eq!(
            convert_synced_location("Online: VideokonferenznZoom etc."),
            "Online: Videokonferenz, Zoom etc."
        );
        // Rooms are left alone
        assert_eq!(convert_synced_location("SeminarnRaum"), "SeminarnRaum");
    }

    #[test]
    fn completes_links_cut_off_at_a_quote() {
        let location = r#"0.001A, Hörsaal 1A, "Zelt" (5539.EG.001A)"#;
        let description = convert_synced_description(
            "Seat 15<br><a href=\"https://nav.tum.de/search?q=0.001A,+Hörsaal+1A,\" target=\"_blank\">Wo ist das?</a><br><hr /><small>uid:𝟏|XR</small>",
            location,
        )
        .unwrap();

        assert_eq!(
            description,
            Some(format!("Seat 15\n{}", location_hint(location).unwrap()))
        );
    }

    #[test]
    fn tells_hand_made_changes_apart() {
        let untouched = Event {
            summary: Some(String::from("Analysis Prüfung")),
            location: Some(String::from("𝟓𝟔𝟎𝟖.EG.𝟎𝟏𝟏")),
            color_id: Some(String::from("11")),
            description: Some(format!(
                "<a href=\"https://nav.tum.de/search?q=102, Hörsaal 2, \"Interims I\" (5620.01.102)\">Wo ist das?</a><br>{MARKER}"
            )),
            ..Default::default()
        };
        assert!(hand_made_changes(&untouched).is_empty());

        let edited = Event {
            location: Some(String::from("𝟓𝟔𝟎𝟖.EG.𝟎𝟏𝟏 Platz 12")),
            color_id: Some(String::from("8")),
            description: Some(String::from(
                "Reihe 7<br><a href=\"https://nav.tum.de/search?q=x\" target=\"_blank\">Wo ist das?</a><br><hr /><small>uid:𝟏|XR</small>",
            )),
            ..Default::default()
        };
        assert_eq!(
            hand_made_changes(&edited),
            vec!["description", "location", "color"]
        );
    }

    #[test]
    fn builds_a_synced_exam() {
        let event = Event {
            summary: Some(String::from("GRNVS Prüfung")),
            location: Some(String::from(
                "𝟐𝟓𝟎𝟏, Rudolf-Mößbauer-Hörsaal (𝟓𝟏𝟎𝟏.EG.𝟓𝟎𝟏) Platz F 7",
            )),
            description: Some(format!(
                "<a href=\"https://nav.tum.de/search?q=𝟐𝟓𝟎𝟏, Rudolf-Mößbauer-Hörsaal (𝟓𝟏𝟎𝟏.EG.𝟓𝟎𝟏)\">Wo ist das?</a><br>{MARKER}"
            )),
            start: at("2025-08-04T06:00:00Z"),
            end: at("2025-08-04T08:00:00Z"),
            color_id: Some(String::from("11")),
            reminders: reminders(&[1440]),
            source: Some(EventSource {
                title: Some(String::from("Link zur Lernveranstaltung")),
                url: Some(String::from("https://campus.tum.de/x")),
            }),
            status: Some(String::from("confirmed")),
            ..Default::default()
        };

        let vevent = synced_event("891956005@tum.deXR", &event)
            .unwrap()
            .expect("the event has a start");
        let calendar_object = unfolded(&calendar_object(&[vevent]));

        assert!(calendar_object.contains("UID:891956005@tum.deXR\r\n"));
        assert!(calendar_object.contains("DTSTART:20250804T060000Z\r\n"));
        assert!(calendar_object.contains("DTEND:20250804T080000Z\r\n"));
        assert!(calendar_object.contains("SUMMARY:GRNVS Prüfung\r\n"));
        assert!(calendar_object
            .contains(r"LOCATION:2501\, Rudolf-Mößbauer-Hörsaal (5101.EG.501) Platz F 7"));
        assert!(
            calendar_object.contains("DESCRIPTION:Wo ist das? https://nav.tum.de/search?q=2501")
        );
        assert!(calendar_object.contains("URL:https://campus.tum.de/x\r\n"));
        assert!(calendar_object.contains("STATUS:CONFIRMED\r\n"));
        assert!(calendar_object.contains("CATEGORIES:Prüfung\r\n"));
        assert!(calendar_object.contains("COLOR:tomato\r\n"));
        assert!(calendar_object.contains(
            "BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:GRNVS Prüfung\r\nTRIGGER:-PT1440M\r\nEND:VALARM\r\n"
        ));
        assert!(!calendar_object.contains("VTIMEZONE"));
    }

    #[test]
    fn builds_a_recurring_event_with_its_exceptions() {
        let series = Series {
            event: Some(Event {
                id: Some(String::from("2bomka0k")),
                i_cal_uid: Some(String::from("2bomka0k@google.com")),
                summary: Some(String::from("MA HA Gruppe")),
                start: at("2025-01-14T17:00:00Z"),
                end: at("2025-01-14T19:00:00Z"),
                recurrence: Some(vec![String::from(
                    "RRULE:FREQ=WEEKLY;UNTIL=20250210T225959Z;BYDAY=TU",
                )]),
                ..Default::default()
            }),
            exceptions: vec![
                Event {
                    recurring_event_id: Some(String::from("2bomka0k")),
                    status: Some(String::from("cancelled")),
                    original_start_time: at("2025-01-28T17:00:00Z"),
                    ..Default::default()
                },
                Event {
                    recurring_event_id: Some(String::from("2bomka0k")),
                    summary: Some(String::from("MA HA Gruppe (später)")),
                    status: Some(String::from("confirmed")),
                    original_start_time: at("2025-02-04T17:00:00Z"),
                    start: at("2025-02-04T18:30:00Z"),
                    end: at("2025-02-04T20:00:00Z"),
                    ..Default::default()
                },
            ],
        };

        let calendar_object = unfolded(&calendar_object(&series_events(&series).unwrap()));

        assert!(calendar_object.contains("BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\n"));
        assert!(calendar_object.contains("DTSTART;TZID=Europe/Berlin:20250114T180000\r\n"));
        assert!(calendar_object.contains("RRULE:FREQ=WEEKLY;UNTIL=20250210T225959Z;BYDAY=TU\r\n"));
        assert_eq!(calendar_object.matches("RRULE:FREQ=WEEKLY").count(), 1);
        assert!(calendar_object.contains("EXDATE;TZID=Europe/Berlin:20250128T180000\r\n"));
        assert!(calendar_object.contains("RECURRENCE-ID;TZID=Europe/Berlin:20250204T180000\r\n"));
        assert!(calendar_object.contains("DTSTART;TZID=Europe/Berlin:20250204T193000\r\n"));
        assert!(calendar_object.contains("SUMMARY:MA HA Gruppe (später)\r\n"));
        assert_eq!(calendar_object.matches("BEGIN:VEVENT").count(), 2);
        assert_eq!(
            calendar_object.matches("UID:2bomka0k@google.com").count(),
            2
        );
    }

    #[test]
    fn builds_all_day_events() {
        let series = Series {
            event: Some(Event {
                id: Some(String::from("c8qjed1p")),
                i_cal_uid: Some(String::from("c8qjed1p@google.com")),
                summary: Some(String::from("Klassentreffen")),
                start: on(2025, 12, 28),
                end: on(2025, 12, 29),
                color_id: Some(String::from("11")),
                reminders: reminders(&[9540]),
                ..Default::default()
            }),
            exceptions: Vec::new(),
        };

        let calendar_object = unfolded(&calendar_object(&series_events(&series).unwrap()));

        assert!(calendar_object.contains("DTSTART;VALUE=DATE:20251228\r\n"));
        assert!(calendar_object.contains("DTEND;VALUE=DATE:20251229\r\n"));
        assert!(calendar_object.contains("COLOR:tomato\r\n"));
        assert!(calendar_object.contains("TRIGGER:-PT9540M\r\n"));
        assert!(!calendar_object.contains("CATEGORIES"));
        assert!(!calendar_object.contains("VTIMEZONE"));
    }

    #[test]
    fn refuses_recurring_events_in_other_time_zones() {
        let series = Series {
            event: Some(Event {
                id: Some(String::from("abc")),
                start: Some(EventDateTime {
                    date_time: Some("2025-01-14T17:00:00Z".parse().unwrap()),
                    time_zone: Some(String::from("America/New_York")),
                    date: None,
                }),
                recurrence: Some(vec![String::from("RRULE:FREQ=DAILY;COUNT=3")]),
                ..Default::default()
            }),
            exceptions: Vec::new(),
        };

        assert!(series_events(&series).is_err());
    }
}
