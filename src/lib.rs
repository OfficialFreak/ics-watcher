//! A lightweight crate for monitoring iCalendar (ICS) files or links and detecting changes, additions, and removals.
//! Provides an API to watch calendars and receive notifications through customizable callbacks.
//!
//! See [ICSWatcher] to get started.

use std::{
    collections::HashMap,
    fs::{self, File},
    future::Future,
    io::BufReader,
    path::Path,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use chrono::{NaiveDateTime, Utc};

use ical::{
    parser::{
        ical::component::{IcalCalendar, IcalEvent},
        Component,
    },
    property::Property,
    IcalParser,
};

use google_calendar3::{
    api::{Event, EventDateTime},
    hyper_rustls::{self, HttpsConnector},
    hyper_util::{self, client::legacy::connect::HttpConnector},
    yup_oauth2::{self, read_application_secret},
    CalendarHub,
};

use once_cell::sync::Lazy;
use regex::Regex;
use sanitize_filename::sanitize;
use tokio::time::sleep;

fn rfc5545_to_std_duration(rfc_duration: &str) -> Duration {
    let duration_str = rfc_duration.trim_start_matches('P').replace('T', "");

    let mut total_secs = 0u64;
    let mut number = String::new();

    for c in duration_str.chars() {
        match c {
            'W' => {
                let weeks = number.parse::<u64>().unwrap_or(0);
                total_secs += weeks * 7 * 24 * 60 * 60;
                number.clear();
            }
            'D' => {
                let days = number.parse::<u64>().unwrap_or(0);
                total_secs += days * 24 * 60 * 60;
                number.clear();
            }
            'H' => {
                let hours = number.parse::<u64>().unwrap_or(0);
                total_secs += hours * 60 * 60;
                number.clear();
            }
            'M' => {
                let minutes = number.parse::<u64>().unwrap_or(0);
                total_secs += minutes * 60;
                number.clear();
            }
            'S' => {
                let seconds = number.parse::<u64>().unwrap_or(0);
                total_secs += seconds;
                number.clear();
            }
            digit if digit.is_ascii_digit() => {
                number.push(digit);
            }
            '-' => {}
            _ => {}
        }
    }

    Duration::from_secs(total_secs)
}

fn changed_properties(event1: &IcalEvent, event2: &IcalEvent) -> Option<Vec<String>> {
    let props1 = &event1.properties;
    let props2 = &event2.properties;

    let mut changed_props = Vec::new();

    // Check for modified and removed properties
    for prop1 in props1.iter().filter(|p| p.name != "DTSTAMP") {
        let matching_prop = props2
            .iter()
            .filter(|p| p.name != "DTSTAMP")
            .find(|prop2| prop1.name == prop2.name);

        match matching_prop {
            Some(prop2) => {
                // Check if existing property changed
                if prop1.value != prop2.value
                    || match (&prop1.params, &prop2.params) {
                        (Some(params1), Some(params2)) => params1 != params2,
                        (None, None) => false,
                        _ => true,
                    }
                {
                    changed_props.push(prop1.name.clone());
                }
            }
            None => {
                // Property was removed in event2
                changed_props.push(prop1.name.clone());
            }
        }
    }

    // Check for new properties in event2
    for prop2 in props2.iter().filter(|p| p.name != "DTSTAMP") {
        if !props1.iter().any(|p| p.name == prop2.name) {
            changed_props.push(prop2.name.clone());
        }
    }

    if changed_props.is_empty() {
        None
    } else {
        Some(changed_props)
    }
}

/// A helper struct to save an [IcalEvent] with its uid
#[derive(Debug, Clone)]
pub struct EventData {
    pub uid: String,
    pub ical_data: IcalEvent,
}

/// A struct denoting a Property Change of a [key](`PropertyChange::key`) with both states in [from](`PropertyChange::from`) and [to](`PropertyChange::to`).
///
/// # Examples
///
/// ```
/// // A description has been added with the contents "New Description"
/// PropertyChange {
///     key: "DESCRIPTION".to_string(),
///     from: None,
///     to: Some(Property {
///         name: "DESCRIPTION".to_string(),
///         params: None,
///         value: Some("New Description".to_string())
///     })
/// }
/// ```
#[derive(Debug, Clone)]
pub struct PropertyChange {
    pub key: String,
    pub from: Option<Property>,
    pub to: Option<Property>,
}

/// Used to pass the events to the callbacks.
///
/// The types:
/// - [`CalendarEvent::Setup`]: If the ICS Watcher is being initialized for the first time, all events that are found will be passed as [`CalendarEvent::Setup`]
/// - [`CalendarEvent::Created`]: If the ICS Watcher has been running, any new events found will be passed as [`CalendarEvent::Created`]
/// - [`CalendarEvent::Updated`]: Any events with different properties. The changed properties, along with both the before and after state will be passed in [`CalendarEvent::Updated::changed_properties`]
/// - [`CalendarEvent::Deleted`]: If an event is not found anymore, it is being passed as [`CalendarEvent::Deleted`]
#[derive(Debug, Clone)]
pub enum CalendarEvent {
    Setup(EventData),
    Created(EventData),
    Updated {
        event: EventData,
        changed_properties: Vec<PropertyChange>,
    },
    Deleted(EventData),
}

/// Handling change detection of a single calendar (as one ics file can contain multiple calendars)
/// For usage details, see [ICSWatcher]
#[derive(Debug)]
pub struct CalendarChangeDetector {
    pub name: Option<String>,
    pub description: Option<String>,
    pub ttl: Duration,
    previous: HashMap<String, IcalEvent>,
    initialized: bool,
}

impl CalendarChangeDetector {
    pub fn new() -> Self {
        CalendarChangeDetector {
            name: None,
            description: None,
            ttl: rfc5545_to_std_duration("PT1H"),
            previous: HashMap::new(),
            initialized: false,
        }
    }

    pub fn set_state(&mut self, state: HashMap<String, IcalEvent>) {
        self.previous = state;
        self.initialized = true;
    }

    pub fn compare(&mut self, calendar: IcalCalendar) -> Vec<CalendarEvent> {
        self.name = calendar
            .get_property("X-WR-CALNAME")
            .and_then(|prop| prop.value.clone());

        self.description = calendar
            .get_property("X-WR-CALDESC")
            .and_then(|prop| prop.value.clone());

        self.ttl = calendar
            .get_property("X-PUBLISHED-TTL")
            .and_then(|prop| prop.value.as_ref())
            .map(|value| value.as_str())
            .and_then(|s| Some(rfc5545_to_std_duration(s)))
            .unwrap_or_else(|| rfc5545_to_std_duration("PT1H"));

        let mut new_previous = HashMap::new();
        let mut result = Vec::with_capacity(calendar.events.len());

        for event in calendar.events {
            let event_uid_property = match event
                .get_property("UID")
                .and_then(|prop| prop.value.clone())
            {
                Some(uid) => uid,
                None => {
                    println!("Warning: An event is missing a UID, skipping");
                    continue;
                }
            };
            let event_uid = event_uid_property
                + &event
                    .get_property("RECURRENCE-ID")
                    .map(|prop| match prop.value.clone() {
                        Some(v) => v,
                        None => String::from("R"),
                    })
                    .unwrap_or(String::from(""))
                + &event
                    .get_property("X-CO-RECURRINGID")
                    .map(|prop| match prop.value.clone() {
                        Some(v) => v,
                        None => String::from("XR"),
                    })
                    .unwrap_or(String::from(""));

            new_previous.insert(event_uid.clone(), event.clone());
            if self.initialized {
                if let Some(prev_event) = self.previous.get(&event_uid) {
                    match changed_properties(prev_event, &event) {
                        Some(properties) => {
                            result.push(CalendarEvent::Updated {
                                changed_properties: properties
                                    .iter()
                                    .map(|property| PropertyChange {
                                        key: property.clone(),
                                        from: self.previous[&event_uid]
                                            .get_property(property)
                                            .cloned(),
                                        to: new_previous[&event_uid]
                                            .get_property(property)
                                            .cloned(),
                                    })
                                    .collect(),
                                event: EventData {
                                    uid: event_uid,
                                    ical_data: event,
                                },
                            });
                        }
                        None => (),
                    }
                } else {
                    result.push(CalendarEvent::Created(EventData {
                        uid: event_uid,
                        ical_data: event,
                    }));
                }
            } else {
                result.push(CalendarEvent::Setup(EventData {
                    uid: event_uid,
                    ical_data: event,
                }));
            }
        }

        for (uid, ical_data) in self.previous.drain() {
            if !new_previous.contains_key(&uid) {
                result.push(CalendarEvent::Deleted(EventData { uid, ical_data }));
            }
        }

        self.previous = new_previous;
        self.initialized = true;

        result
    }
}

pub type CalendarCallback = Box<
    dyn Fn(
        Option<String>,
        Option<String>,
        Vec<CalendarEvent>,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>,
    >,
>;
/// Instantiate an [ICSWatcher] using [ICSWatcher::new] to watch for changes of an ics link.
///
/// Using this, you can also [create](`ICSWatcher::create_backup`) and [load](`ICSWatcher::load_backup`) backups.
/// If you want to handle when the watcher updates, you can manually call the [`ICSWatcher::update`] method.
///
/// # Examples
///
/// ```
/// let mut ics_watcher = ICSWatcher::new(
///     "some url",
///     vec![
///         Box::new(|a, b, e| Box::pin(async move { log_events(a, b, e).await })),
///     ],
/// );

/// // Try to load backup
/// let _ = ics_watcher.load_backup("Your Calendar");
/// // Run ics watcher infinitely and save backups as "Your Calendar"
/// ics_watcher
///     .run(Option::from("Your Calendar"))
///     .await
///     .expect("ICS Watcher crashed");
/// ```
pub struct ICSWatcher<'a> {
    ics_link: &'a str,
    pub callbacks: Vec<CalendarCallback>,
    change_detector: CalendarChangeDetector,
}

impl<'a> ICSWatcher<'a> {
    pub fn new(ics_link: &'a str, callbacks: Vec<CalendarCallback>) -> Self {
        ICSWatcher {
            ics_link,
            callbacks,
            change_detector: CalendarChangeDetector::new(),
        }
    }

    pub fn restore_state(&mut self, state: HashMap<String, IcalEvent>) {
        self.change_detector.set_state(state);
    }

    pub fn get_state(&self) -> &HashMap<String, IcalEvent> {
        &self.change_detector.previous
    }

    pub fn get_calendar_name(&self) -> Option<String> {
        self.change_detector.name.clone()
    }

    pub fn create_backup(&self, name: &str) {
        let backup_file_path = Path::new(".backups").join(sanitize(name) + ".cbor");

        fs::create_dir_all(".backups").expect("Failed to create .backups folder");
        let backup_file = File::create(backup_file_path).expect("Failed to create backup file");
        ciborium::ser::into_writer(self.get_state(), backup_file)
            .expect("Failed to create and write backup");
    }

    pub fn load_backup(&mut self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let backup_file_path = File::open(Path::new(".backups").join(sanitize(name) + ".cbor"))?;

        let state = ciborium::de::from_reader(backup_file_path)?;
        self.restore_state(state);

        Ok(())
    }

    pub async fn update(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let res = reqwest::get(self.ics_link).await?;
        // If server doesn't return 200, return with error
        if let Err(error) = res.error_for_status_ref() {
            return Err(error.into());
        }
        let res_text = res.text().await?;
        let buf = BufReader::new(res_text.as_bytes());
        let calendar = IcalParser::new(buf).next().ok_or("No Calendar present")??;

        let events = self.change_detector.compare(calendar);

        if !events.is_empty() {
            let futures: Vec<_> = self
                .callbacks
                .iter()
                .map(|callback| {
                    callback(
                        self.change_detector.name.clone(),
                        self.change_detector.description.clone(),
                        events.clone(),
                    )
                })
                .collect();

            for future in futures {
                match future.await {
                    Ok(()) => (),
                    Err(err) => eprintln!("Error in callback: {err:?}"),
                }
            }
        }

        Ok(())
    }

    pub async fn run(&mut self, backup: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            self.update().await?;
            if let Some(path) = backup {
                self.create_backup(path);
            }
            println!("Refreshing in {:?}", self.change_detector.ttl);
            sleep(self.change_detector.ttl).await;
        }
    }
}

/// This is a callback which logs all events.
///
/// This can come in useful during debugging or when deploying to check the logs later on.
///
/// # Examples
///
/// ```
/// let mut ics_watcher = ICSWatcher::new(
///     "some url",
///     vec![
///         Box::new(|a, b, e| Box::pin(async move { log_events(a, b, e).await })),
///     ],
/// );
///
/// // Try to load backup
/// let _ = ics_watcher.load_backup("Your Calendar");
/// ics_watcher
///     .run(Option::from("Your Calendar"))
///     .await
///     .expect("ICS Watcher crashed");
/// ```
pub async fn log_events(
    name: Option<String>,
    description: Option<String>,
    events: Vec<CalendarEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!(
        "Captured changes of {}{}:",
        name.as_deref().unwrap_or("Unnamed Calendar"),
        description
            .as_deref()
            .and_then(|desc| Some(format!(" ({desc})")))
            .unwrap_or(String::from(""))
    );
    for event in events {
        match event {
            CalendarEvent::Setup(EventData { uid, ical_data }) => {
                println!("Setup {uid}: {ical_data:?}\n")
            }
            CalendarEvent::Created(EventData { uid, ical_data }) => {
                println!("Created {uid}: {ical_data:?}\n")
            }
            CalendarEvent::Updated {
                event,
                changed_properties,
            } => {
                println!("Updated {}: {:?}\n", event.uid, changed_properties)
            }
            CalendarEvent::Deleted(EventData { uid, ical_data }) => {
                println!("Deleted {uid}: {ical_data:?}\n")
            }
        }
    }

    Ok(())
}

static REPLACEMENTS: Lazy<Arc<Vec<(String, String)>>> = Lazy::new(|| {
    let courses_json = match fs::read_to_string("replacements.json") {
        Ok(content) => content,
        Err(_) => return Arc::new(Vec::new()),
    };

    let raw_replacements: HashMap<String, String> = match serde_json::from_str(&courses_json) {
        Ok(parsed) => parsed,
        Err(_) => return Arc::new(Vec::new()),
    };

    let mut replacements: Vec<(String, String)> = raw_replacements.into_iter().collect();
    replacements.sort_by(|(a_key, _), (b_key, _)| {
        if a_key.len() != b_key.len() {
            b_key.len().cmp(&a_key.len())
        } else {
            a_key.cmp(b_key)
        }
    });

    Arc::new(replacements)
});

static LV_ID_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[(([A-Z]{2})(\d{4}))\]|\((([A-Z]{2})(\d{4}))\)").unwrap());

fn remove_lv_id(text: &str) -> String {
    // Matches [AA1234] or (AA1234) where A is any uppercase letter and 1234 is any four digits
    LV_ID_REGEX.replace_all(text, "").to_string()
}

fn replace_courses(input: &str) -> String {
    let mut result = input.to_string();
    for (from, to) in REPLACEMENTS.iter() {
        result = result.replace(from, to);
    }
    remove_lv_id(result.as_str())
}

fn convert_to_non_digits(str: String) -> String {
    str.chars()
        .map(|c| match c {
            '0' => '𝟎',
            '1' => '𝟏',
            '2' => '𝟐',
            '3' => '𝟑',
            '4' => '𝟒',
            '5' => '𝟓',
            '6' => '𝟔',
            '7' => '𝟕',
            '8' => '𝟖',
            '9' => '𝟗',
            other => other,
        })
        .collect::<String>()
}

// TODO: Refactor create and update event
async fn create_event(
    hub: &CalendarHub<HttpsConnector<HttpConnector>>,
    uid: String,
    event: IcalEvent,
    calendar_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut google_event = Event::default();

    let start = NaiveDateTime::parse_from_str(
        &event
            .get_property("DTSTART")
            .ok_or("Required property DTSTART missing")?
            .value
            .clone()
            .ok_or("Required property value DTSTART missing")?[0..15],
        "%Y%m%dT%H%M%S",
    )?
    .and_utc();
    let end = NaiveDateTime::parse_from_str(
        &event
            .get_property("DTEND")
            .ok_or("Required property DTEND missing")?
            .value
            .clone()
            .ok_or("Required property value DTEND missing")?[0..15],
        "%Y%m%dT%H%M%S",
    )?
    .and_utc();

    google_event.start = Some(EventDateTime {
        date_time: Some(start),
        date: None,
        time_zone: None,
    });
    google_event.end = Some(EventDateTime {
        date_time: Some(end),
        date: None,
        time_zone: None,
    });

    let room = event
        .get_property("LOCATION")
        .and_then(|loc| loc.value.clone())
        .map(|s| s.replace(r"\", ""))
        .unwrap_or_else(|| "Kein Ort angegeben".to_string());

    let i_cal_uid = convert_to_non_digits(uid.replace("@tum.de", "|").to_string());

    // google_event.reminders would be useful for exams
    if let Some(url) = event.get_property("URL").and_then(|url| url.value.clone()) {
        google_event.source = Some(google_calendar3::api::EventSource {
            title: Some("Link zur Lernveranstaltung".to_string()),
            url: Some(url),
        });
    }

    if let Some(status) = event
        .get_property("STATUS")
        .and_then(|status| status.value.clone())
    {
        google_event.status = Some(status.to_lowercase());
    }

    match event
        .get_property("SUMMARY")
        .and_then(|summary| summary.value.clone())
    {
        Some(summary) => {
            google_event.summary = Some(replace_courses(summary.replace(r"\", "").as_str()));
            if summary.contains("Prüfung") {
                // Big important :o
                google_event.color_id = Some(String::from("11"));
            }
        }
        None => {
            google_event.summary = Some("Kein Titel angegeben".to_string());
        }
    }

    google_event.location = Some(convert_to_non_digits(room.clone()));

    let link = format!("https://nav.tum.de/search?q={}", room.clone());
    let description = event
        .get_property("DESCRIPTION")
        .and_then(|prop| prop.value.clone())
        .map(|desc| desc.split(r"\;").skip(2).collect())
        .unwrap_or(String::new())
        .as_str()
        .replace(r"\", "")
        .trim()
        .to_string();

    let original_description = if !description.is_empty() {
        format!("{}<br>", description)
    } else {
        description
    };

    let location_link = format!("<a href=\"{}\">Wo ist das?</a><br>", link);
    let online_only = room.to_lowercase().contains("online");
    let on_moodle = room.to_lowercase().contains("moodle");

    google_event.description = Some(format!(
        "{}{}<br><hr><small>uid:{}</small>",
        original_description,
        if online_only {
            if on_moodle {
                "<a href=\"https://www.moodle.tum.de/my/\">Online auf Moodle</a><br>".into()
            } else {
                "Online<br>".into()
            }
        } else {
            location_link
        },
        i_cal_uid
    ));

    let results = hub
        .events()
        .list(calendar_id)
        .q(&format!("uid:{}", i_cal_uid))
        .doit()
        .await?;

    if let Some(event_id) = results
        .1
        .items
        .and_then(|items| items.first().cloned())
        .and_then(|event| event.id)
    {
        hub.events()
            .update(google_event, calendar_id, &event_id)
            .doit()
            .await?
            .0
    } else {
        hub.events()
            .insert(google_event, calendar_id)
            .doit()
            .await?
            .0
    };

    Ok(())
}

async fn update_event(
    hub: &CalendarHub<HttpsConnector<HttpConnector>>,
    uid: String,
    event: IcalEvent,
    property_changes: Vec<PropertyChange>,
    calendar_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Updating event {uid}: {property_changes:?}");

    let i_cal_uid = convert_to_non_digits(uid.replace("@tum.de", "|").to_string());
    let results = hub
        .events()
        .list(calendar_id)
        .q(&format!("uid:{}", i_cal_uid))
        .doit()
        .await?;

    let oringinal_event = results
        .1
        .items
        .and_then(|items| items.first().cloned())
        .ok_or("Updating not possible as event is not present anymore")?;
    let event_id = oringinal_event
        .id
        .ok_or("Google didn't provide the event with an ID")?;
    let mut google_event = Event::default();

    if property_changes
        .iter()
        .any(|property_change| property_change.key == "DTSTART" || property_change.key == "DTEND")
    {
        let start = NaiveDateTime::parse_from_str(
            &event
                .get_property("DTSTART")
                .ok_or("Required property DTSTART missing")?
                .value
                .clone()
                .ok_or("Required property value DTSTART missing")?[0..15],
            "%Y%m%dT%H%M%S",
        )?
        .and_utc();
        let end = NaiveDateTime::parse_from_str(
            &event
                .get_property("DTEND")
                .ok_or("Required property DTEND missing")?
                .value
                .clone()
                .ok_or("Required property DTEND value missing")?[0..15],
            "%Y%m%dT%H%M%S",
        )?
        .and_utc();

        google_event.start = Some(EventDateTime {
            date_time: Some(start),
            date: None,
            time_zone: None,
        });
        google_event.end = Some(EventDateTime {
            date_time: Some(end),
            date: None,
            time_zone: None,
        });
    } else {
        google_event.start = oringinal_event.start;
        google_event.end = oringinal_event.end;
    }

    // google_event.reminders would be useful for exams
    if let Some(url) = event.get_property("URL").and_then(|url| url.value.clone()) {
        google_event.source = Some(google_calendar3::api::EventSource {
            title: Some("Link zur Lernveranstaltung".to_string()),
            url: Some(url),
        });
    }

    if property_changes
        .iter()
        .any(|property_change| property_change.key == "STATUS")
    {
        if let Some(status) = event
            .get_property("STATUS")
            .and_then(|status| status.value.clone())
        {
            google_event.status = Some(status.to_lowercase());
        }
    } else {
        google_event.status = oringinal_event.status;
    }

    if property_changes
        .iter()
        .any(|property_change| property_change.key == "SUMMARY")
    {
        match event
            .get_property("SUMMARY")
            .and_then(|summary| summary.value.clone())
        {
            Some(summary) => {
                google_event.summary = Some(replace_courses(summary.replace(r"\", "").as_str()));
                if summary.contains("Prüfung") {
                    // 11 = Tomato (Google Calendar's Red)
                    google_event.color_id = Some(String::from("11"));
                }
            }
            None => {
                google_event.summary = Some("Kein Titel angegeben".to_string());
            }
        }
    } else {
        if oringinal_event
            .summary
            .clone()
            .is_some_and(|summary| summary.contains("Prüfung"))
        {
            // 11 = Tomato (Google Calendar's Red)
            google_event.color_id = Some(String::from("11"));
        }
        google_event.summary = oringinal_event.summary;
    }

    // If room has changed, update all properties associated with the room
    let room = event
        .get_property("LOCATION")
        .and_then(|loc| loc.value.clone())
        .map(|s| s.replace(r"\", ""))
        .unwrap_or_else(|| "Kein Ort angegeben".to_string());
    if property_changes
        .iter()
        .any(|property_change| property_change.key == "LOCATION")
    {
        google_event.location = Some(convert_to_non_digits(room.clone()));
    } else {
        google_event.location = oringinal_event.location;
    }

    let link = format!(
        "https://nav.tum.de/search?q={}",
        google_event.location.clone().unwrap_or_default()
    );

    let description = event
        .get_property("DESCRIPTION")
        .and_then(|prop| prop.value.clone())
        .map(|desc| desc.split(r"\;").skip(2).collect())
        .unwrap_or(String::new())
        .as_str()
        .replace(r"\", "")
        .trim()
        .to_string();

    let original_description = if !description.is_empty() {
        format!("{}<br>", description)
    } else {
        description
    };

    if property_changes.iter().any(|property_change| {
        property_change.key == "DESCRIPTION" || property_change.key == "LOCATION"
    }) {
        let location_link = format!("<a href=\"{}\">Wo ist das?</a><br>", link);
        let online_only = google_event
            .location
            .clone()
            .unwrap_or_default()
            .to_lowercase()
            .contains("online");
        let on_moodle = google_event
            .location
            .clone()
            .unwrap_or_default()
            .to_lowercase()
            .contains("moodle");
        google_event.description = Some(format!(
            "{}{}<br><hr><small>uid:{}</small>",
            original_description,
            if online_only {
                if on_moodle {
                    "<a href=\"https://www.moodle.tum.de/my/\">Online auf Moodle</a><br>".into()
                } else {
                    "Online<br>".into()
                }
            } else {
                location_link
            },
            i_cal_uid
        ));
    } else {
        google_event.description = oringinal_event.description;
    }

    hub.events()
        .update(google_event, calendar_id, &event_id)
        .doit()
        .await?
        .0;

    Ok(())
}

async fn delete_event(
    hub: &CalendarHub<HttpsConnector<HttpConnector>>,
    uid: String,
    calendar_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let i_cal_uid = convert_to_non_digits(uid.replace("@tum.de", "|").to_string());
    let results = hub
        .events()
        .list(calendar_id)
        .q(&format!("uid:{}", i_cal_uid))
        .doit()
        .await?;

    if let Some(event_id) = results
        .1
        .items
        .and_then(|items| items.first().cloned())
        .and_then(|event| event.id)
    {
        hub.events().delete(calendar_id, &event_id).doit().await?;
    }

    Ok(())
}

/// This is a callback which synchronizes your TUM Calendar to your Google Calender.
///
/// The event summaries will be shortened and the events themselves modifieable. As soon as you delete an event, it won't come back.
/// If you modify an event, your changes will only be overwritten if they're changed in the TUM Calendar.
///
/// # Examples
///
/// ```
/// let mut ics_watcher = ICSWatcher::new(
///     tum_url.as_str(),
///     vec![
///         Box::new(move |a, b, e| {
///             let calendar_id = google_calendar_id.clone();
///             Box::pin(async move { tum_google_sync(&calendar_id, a, b, e).await })
///         }),
///     ],
/// );

/// // Try to load backup
/// let _ = ics_watcher.load_backup("TUM Calendar");
/// ics_watcher
///     .run(Option::from("TUM Calendar"))
///     .await
///     .expect("ICS Watcher crashed");
/// ```
pub async fn tum_google_sync(
    calendar_id: &str,
    _: Option<String>,
    _: Option<String>,
    events: Vec<CalendarEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let secret: yup_oauth2::ApplicationSecret =
        read_application_secret(Path::new(".secrets/client_secret.json"))
            .await
            .expect("Failed to read client secret");

    let auth = yup_oauth2::InstalledFlowAuthenticator::builder(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::HTTPRedirect,
    )
    .persist_tokens_to_disk(".secrets/token_cache.json")
    .build()
    .await?;

    auth.token(&["https://www.googleapis.com/auth/calendar"])
        .await
        .expect("Unable to get scope for calendar");

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()?
                .https_or_http()
                .enable_http1()
                .build(),
        );
    let hub = CalendarHub::new(client, auth);

    for event in events {
        let calendar_id = calendar_id;

        let result = match event {
            CalendarEvent::Setup(EventData { uid, ical_data }) => {
                // Don't sync if event is a video transmission
                if ical_data
                    .get_property("DESCRIPTION")
                    .and_then(|prop| prop.value.clone())
                    .map(|desc| desc.contains("Videoübertragung aus"))
                    .unwrap_or(false)
                {
                    Err(format!(
                        "Skipping video transmission event {:?}",
                        ical_data.get_property("SUMMARY"),
                    )
                    .into())
                } else {
                    println!("Setting up event {uid}");
                    create_event(&hub, uid, ical_data, calendar_id).await
                }
            }
            CalendarEvent::Created(EventData { uid, ical_data }) => {
                // Don't sync if event is a video transmission
                if ical_data
                    .get_property("DESCRIPTION")
                    .and_then(|prop| prop.value.clone())
                    .map(|desc| desc.contains("Videoübertragung aus"))
                    .unwrap_or(false)
                {
                    // Skipping video transmission event
                    Ok(())
                } else {
                    println!("Creating event {uid}");
                    create_event(&hub, uid, ical_data, calendar_id).await
                }
            }
            CalendarEvent::Updated {
                event: EventData { uid, ical_data },
                changed_properties,
            } => {
                // The TUM Calendar seems to randomly serve english / german descriptions
                // This looks for differences other than the first two words in english / german
                if changed_properties.len() == 1
                    && changed_properties[0].key == "DESCRIPTION"
                    && changed_properties[0]
                        .from
                        .as_ref()
                        .and_then(|from| from.value.as_ref())
                        .zip(
                            changed_properties[0]
                                .to
                                .as_ref()
                                .and_then(|to| to.value.as_ref()),
                        )
                        .map_or(false, |(from, to)| {
                            from.split(";").skip(2).collect::<String>()
                                == to.split(";").skip(2).collect::<String>()
                        })
                {
                    // Update is a language-only update
                    Ok(())
                } else {
                    update_event(&hub, uid, ical_data, changed_properties, calendar_id).await
                }
            }
            CalendarEvent::Deleted(EventData { uid, ical_data }) => {
                println!("Deleting event {uid}");
                // If the event is in the far past, we assume it's just the calendar updating
                // for the next semester, which means we don't actually need to delete it
                let end_date = ical_data
                    .get_property("DTEND")
                    .and_then(|prop| prop.value.clone())
                    .and_then(|value| {
                        if value.len() >= 15 {
                            NaiveDateTime::parse_from_str(&value[0..15], "%Y%m%dT%H%M%S")
                                .map(|dt| dt.and_utc())
                                .ok()
                        } else {
                            None
                        }
                    });

                match end_date {
                    Some(end) if end < Utc::now() - Duration::from_secs(60 * 24 * 7) => {
                        // Not deleting event as it is far back in the past
                        Ok(())
                    }
                    _ => delete_event(&hub, uid, calendar_id).await,
                }
            }
        };

        match result {
            Ok(_) => (),
            Err(error) => eprintln!("Error on syncing event: {error:?}"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmp_no_properties() {
        let event1 = IcalEvent {
            properties: vec![],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2);

        assert_eq!(keys, None);
    }

    #[test]
    fn cmp_same_properties() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: Some(String::from("prop1 value")),
            params: None,
        };

        let prop2 = Property {
            name: String::from("prop2"),
            value: Some(String::from("prop2 value")),
            params: None,
        };

        let event1 = IcalEvent {
            properties: vec![prop1.clone(), prop2.clone()],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2.clone(), prop1.clone()],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2);

        assert_eq!(keys, None);
    }

    #[test]
    fn cmp_different_properties() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: Some(String::from("prop1 value")),
            params: None,
        };

        let prop2 = Property {
            name: String::from("prop2"),
            value: Some(String::from("prop2 value")),
            params: None,
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&String::from("prop1")) && keys.contains(&String::from("prop2")));
    }

    #[test]
    fn cmp_added_property() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: Some(String::from("prop1 value")),
            params: None,
        };

        let event1 = IcalEvent {
            properties: vec![],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&String::from("prop1")));
    }

    #[test]
    fn cmp_removed_property() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: Some(String::from("prop1 value")),
            params: None,
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&String::from("prop1")));
    }

    #[test]
    fn cmp_different_properties_no_value() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: None,
        };

        let prop2 = Property {
            name: String::from("prop2"),
            value: None,
            params: None,
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&String::from("prop1")) && keys.contains(&String::from("prop2")));
    }

    #[test]
    fn cmp_different_params() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value")])]),
        };

        let prop2 = Property {
            name: String::from("prop2"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value")])]),
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&String::from("prop1")) && keys.contains(&String::from("prop2")));
    }

    #[test]
    fn cmp_same_params() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value")])]),
        };

        let prop2 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value")])]),
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2);

        assert_eq!(keys, None);
    }

    #[test]
    fn cmp_different_param_keys() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value")])]),
        };

        let prop2 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key2"), vec![String::from("value")])]),
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&String::from("prop1")));
    }

    #[test]
    fn cmp_different_param_values() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value")])]),
        };

        let prop2 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value2")])]),
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&String::from("prop1")));
    }

    #[test]
    fn cmp_added_param() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: None,
        };

        let prop2 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value2")])]),
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&String::from("prop1")));
    }

    #[test]
    fn cmp_removed_param() {
        let prop1 = Property {
            name: String::from("prop1"),
            value: None,
            params: Some(vec![(String::from("key"), vec![String::from("value2")])]),
        };

        let prop2 = Property {
            name: String::from("prop1"),
            value: None,
            params: None,
        };

        let event1 = IcalEvent {
            properties: vec![prop1],
            alarms: vec![],
        };

        let event2 = IcalEvent {
            properties: vec![prop2],
            alarms: vec![],
        };

        let keys = changed_properties(&event1, &event2).expect("Keys should be Some");

        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&String::from("prop1")));
    }
}
