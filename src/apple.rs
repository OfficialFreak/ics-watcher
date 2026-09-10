//! Apple Calendar (iCloud) adapter.
//!
//! Apple doesn't offer a REST API for its calendars, so this adapter talks CalDAV
//! ([RFC 4791](https://datatracker.ietf.org/doc/html/rfc4791)) to `caldav.icloud.com`.
//! Authentication uses an Apple ID together with an
//! [app-specific password](https://support.apple.com/en-us/102654) - your regular
//! Apple ID password will not work.
//!
//! See [AppleCalendar] to connect to a calendar and [tum_apple_sync] for the
//! counterpart of [`tum_google_sync`](crate::tum_google_sync).

use std::{collections::HashMap, io::BufReader, time::Duration};

use chrono::{DateTime, NaiveDateTime, Utc};

use ical::{
    parser::{ical::component::IcalEvent, Component},
    property::Property,
    IcalParser,
};

use quick_xml::{escape::unescape, events::Event as XmlEvent, Reader};
use reqwest::{
    header::{CONTENT_TYPE, ETAG, IF_MATCH},
    Client, Method, StatusCode, Url,
};

use crate::{replace_courses, CalendarEvent, EventData, PropertyChange};

/// The CalDAV entry point of iCloud, used by [`AppleCalendar::connect`].
pub const ICLOUD_CALDAV_URL: &str = "https://caldav.icloud.com/";

const PRODID: &str = "-//ics-watcher//Apple Calendar Adapter//EN";

const CURRENT_USER_PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:current-user-principal/></d:prop>
</d:propfind>"#;

const CALENDAR_HOME_SET_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-home-set/></d:prop>
</d:propfind>"#;

const CALENDAR_LIST_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;

// ---------------------------------------------------------------------------
// CalDAV plumbing
// ---------------------------------------------------------------------------

/// A single `<response>` of a WebDAV `multistatus` document, reduced to the
/// properties this adapter cares about.
#[derive(Debug, Default)]
struct DavResource {
    href: String,
    display_name: Option<String>,
    is_calendar: bool,
    /// `None` if the server didn't report a component set at all
    components: Option<Vec<String>>,
    /// Hrefs nested inside a property, keyed by the property name
    /// (e.g. `current-user-principal`)
    property_hrefs: HashMap<String, String>,
}

fn local_name(name: quick_xml::name::QName) -> String {
    name.local_name().into_inner().to_lowercase()
}

/// Handles an opening (or self-closing) tag. `parents` does *not* contain `name` yet.
fn open_element(
    name: &str,
    element: &quick_xml::events::BytesStart,
    parents: &[String],
    resources: &mut Vec<DavResource>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match name {
        "response" => resources.push(DavResource::default()),
        "calendar" if parents.iter().any(|parent| parent == "resourcetype") => {
            if let Some(resource) = resources.last_mut() {
                resource.is_calendar = true;
            }
        }
        "comp"
            if parents
                .iter()
                .any(|parent| parent == "supported-calendar-component-set") =>
        {
            if let Some(resource) = resources.last_mut() {
                let components = resource.components.get_or_insert_with(Vec::new);
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    if local_name(attribute.key) == "name" {
                        components.push(unescape(&attribute.value)?.to_string());
                    }
                }
            }
        }
        _ => (),
    }

    Ok(())
}

/// Handles a closing tag. `parents` no longer contains `name`.
fn close_element(name: &str, text: &str, parents: &[String], resources: &mut [DavResource]) {
    let Some(resource) = resources.last_mut() else {
        return;
    };

    match name {
        "href" if text.is_empty() => (),
        "href" if parents.last().is_some_and(|parent| parent == "response") => {
            resource.href = text.to_string();
        }
        "href" => {
            // <prop><current-user-principal><href>…</href></…></prop>
            if parents.len() >= 2 && parents[parents.len() - 2] == "prop" {
                resource
                    .property_hrefs
                    .insert(parents[parents.len() - 1].clone(), text.to_string());
            }
        }
        "displayname"
            if !text.is_empty() && parents.last().is_some_and(|parent| parent == "prop") =>
        {
            resource.display_name = Some(text.to_string());
        }
        _ => (),
    }
}

fn parse_multistatus(
    xml: &str,
) -> Result<Vec<DavResource>, Box<dyn std::error::Error + Send + Sync>> {
    let mut reader = Reader::from_str(xml);
    let mut resources: Vec<DavResource> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();

    loop {
        match reader.read_event()? {
            XmlEvent::Start(element) => {
                let name = local_name(element.name());
                open_element(&name, &element, &stack, &mut resources)?;
                stack.push(name);
                text.clear();
            }
            XmlEvent::Empty(element) => {
                let name = local_name(element.name());
                open_element(&name, &element, &stack, &mut resources)?;
            }
            XmlEvent::Text(content) => text.push_str(&content.xml10_content()),
            // Entity references arrive as their own event and have to be resolved
            XmlEvent::GeneralRef(reference) => {
                text.push_str(&unescape(&format!("&{};", reference.as_ref()))?)
            }
            XmlEvent::End(_) => {
                if let Some(name) = stack.pop() {
                    close_element(&name, text.trim(), &stack, &mut resources);
                }
                text.clear();
            }
            XmlEvent::Eof => break,
            _ => (),
        }
    }

    Ok(resources)
}

#[derive(Clone)]
struct DavClient {
    http: Client,
    username: String,
    password: String,
}

impl DavClient {
    fn new(
        username: &str,
        password: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(DavClient {
            http: Client::builder()
                .user_agent(concat!("ics-watcher/", env!("CARGO_PKG_VERSION")))
                .build()?,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    fn request(&self, method: Method, url: &Url) -> reqwest::RequestBuilder {
        self.http
            .request(method, url.clone())
            .basic_auth(&self.username, Some(&self.password))
    }

    async fn propfind(
        &self,
        url: &Url,
        depth: &str,
        body: &'static str,
    ) -> Result<Vec<DavResource>, Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .request(Method::from_bytes(b"PROPFIND")?, url)
            .header(CONTENT_TYPE, "application/xml; charset=utf-8")
            .header("Depth", depth)
            .body(body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!("PROPFIND {url} failed with status {status}").into());
        }

        parse_multistatus(&response.text().await?)
    }
}

/// A calendar collection as advertised by the CalDAV server.
#[derive(Debug, Clone)]
pub struct CalendarInfo {
    /// The name as shown in the Apple Calendar app
    pub name: Option<String>,
    pub url: String,
    /// `false` for collections which only hold reminders (`VTODO`)
    pub supports_events: bool,
}

async fn discover_calendars(
    dav: &DavClient,
    server_url: &str,
) -> Result<Vec<CalendarInfo>, Box<dyn std::error::Error + Send + Sync>> {
    let server_url = Url::parse(server_url)?;

    let principal_href = dav
        .propfind(&server_url, "0", CURRENT_USER_PRINCIPAL_BODY)
        .await?
        .into_iter()
        .find_map(|resource| {
            resource
                .property_hrefs
                .get("current-user-principal")
                .cloned()
        })
        .ok_or("CalDAV server didn't return a current-user-principal")?;
    let principal_url = server_url.join(&principal_href)?;

    let home_href = dav
        .propfind(&principal_url, "0", CALENDAR_HOME_SET_BODY)
        .await?
        .into_iter()
        .find_map(|resource| resource.property_hrefs.get("calendar-home-set").cloned())
        .ok_or("CalDAV server didn't return a calendar-home-set")?;
    let home_url = ensure_collection_url(principal_url.join(&home_href)?)?;

    let mut calendars = Vec::new();
    for resource in dav.propfind(&home_url, "1", CALENDAR_LIST_BODY).await? {
        if !resource.is_calendar {
            continue;
        }

        calendars.push(CalendarInfo {
            name: resource.display_name,
            url: ensure_collection_url(home_url.join(&resource.href)?)?.to_string(),
            supports_events: resource
                .components
                .is_none_or(|components| components.iter().any(|component| component == "VEVENT")),
        });
    }

    Ok(calendars)
}

fn ensure_collection_url(url: Url) -> Result<Url, Box<dyn std::error::Error + Send + Sync>> {
    if url.path().ends_with('/') {
        Ok(url)
    } else {
        Ok(Url::parse(&format!("{url}/"))?)
    }
}

/// Lists all calendars of an iCloud account, which is handy to find out the exact
/// name to pass to [`AppleCalendar::connect`].
///
/// `app_password` has to be an [app-specific password](https://support.apple.com/en-us/102654).
pub async fn list_icloud_calendars(
    apple_id: &str,
    app_password: &str,
) -> Result<Vec<CalendarInfo>, Box<dyn std::error::Error + Send + Sync>> {
    let dav = DavClient::new(apple_id, app_password)?;
    discover_calendars(&dav, ICLOUD_CALDAV_URL).await
}

// ---------------------------------------------------------------------------
// Calendar handle
// ---------------------------------------------------------------------------

/// A connected Apple Calendar, used by [tum_apple_sync].
///
/// Cloning is cheap (the underlying connection pool is shared), so the handle can
/// be created once and moved into a callback.
///
/// # Examples
///
/// ```
/// let apple_calendar = AppleCalendar::connect("me@icloud.com", "abcd-efgh-ijkl-mnop", "TUM")
///     .await
///     .expect("Failed to connect to the Apple Calendar");
/// ```
#[derive(Clone)]
pub struct AppleCalendar {
    dav: DavClient,
    calendar_url: Url,
}

impl std::fmt::Debug for AppleCalendar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately without the credentials
        formatter
            .debug_struct("AppleCalendar")
            .field("username", &self.dav.username)
            .field("calendar_url", &self.calendar_url.as_str())
            .finish_non_exhaustive()
    }
}

impl AppleCalendar {
    /// Connects to the iCloud calendar named `calendar_name`.
    ///
    /// `app_password` has to be an [app-specific password](https://support.apple.com/en-us/102654),
    /// your regular Apple ID password won't be accepted.
    pub async fn connect(
        apple_id: &str,
        app_password: &str,
        calendar_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::connect_to(ICLOUD_CALDAV_URL, apple_id, app_password, calendar_name).await
    }

    /// Same as [`AppleCalendar::connect`], but for any other CalDAV server
    /// (Fastmail, Nextcloud, Radicale, …).
    pub async fn connect_to(
        server_url: &str,
        username: &str,
        password: &str,
        calendar_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let dav = DavClient::new(username, password)?;
        let calendars = discover_calendars(&dav, server_url).await?;

        let calendar = calendars
            .iter()
            .find(|calendar| {
                calendar.supports_events && calendar.name.as_deref() == Some(calendar_name)
            })
            .ok_or_else(|| {
                let available: Vec<&str> = calendars
                    .iter()
                    .filter(|calendar| calendar.supports_events)
                    .filter_map(|calendar| calendar.name.as_deref())
                    .collect();
                format!(
                    "No calendar named {calendar_name:?} found. Available calendars: {}",
                    available.join(", ")
                )
            })?;

        Ok(AppleCalendar {
            calendar_url: Url::parse(&calendar.url)?,
            dav,
        })
    }

    /// Skips the discovery and uses the calendar collection at `calendar_url` directly.
    pub fn from_calendar_url(
        username: &str,
        password: &str,
        calendar_url: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(AppleCalendar {
            dav: DavClient::new(username, password)?,
            calendar_url: ensure_collection_url(Url::parse(calendar_url)?)?,
        })
    }

    pub fn calendar_url(&self) -> &str {
        self.calendar_url.as_str()
    }

    /// The URL an event is stored at. Derived from the uid, so the same event always
    /// ends up at the same place - no lookup needed.
    fn object_url(&self, uid: &str) -> Result<Url, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.calendar_url.join(&object_name(uid))?)
    }

    /// Returns the stored calendar object and its etag, or `None` if it doesn't exist
    /// (anymore).
    async fn fetch_object(
        &self,
        url: &Url,
    ) -> Result<Option<(String, Option<String>)>, Box<dyn std::error::Error + Send + Sync>> {
        let response = self.dav.request(Method::GET, url).send().await?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            return Err(format!("GET {url} failed with status {status}").into());
        }

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|etag| etag.to_str().ok())
            .map(|etag| etag.to_string());

        Ok(Some((response.text().await?, etag)))
    }

    async fn put_object(
        &self,
        url: &Url,
        calendar_object: &str,
        etag: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut request = self
            .dav
            .request(Method::PUT, url)
            .header(CONTENT_TYPE, "text/calendar; charset=utf-8")
            .body(calendar_object.to_string());

        if let Some(etag) = etag {
            request = request.header(IF_MATCH, etag);
        }

        let response = request.send().await?;
        let status = response.status();

        if status == StatusCode::PRECONDITION_FAILED {
            return Err(format!(
                "PUT {url} was rejected because the event changed on the server in the meantime"
            )
            .into());
        }
        if !status.is_success() {
            return Err(format!("PUT {url} failed with status {status}").into());
        }

        Ok(())
    }

    async fn delete_object(
        &self,
        url: &Url,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = self.dav.request(Method::DELETE, url).send().await?;
        let status = response.status();

        // Already gone - nothing to do
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            return Err(format!("DELETE {url} failed with status {status}").into());
        }

        Ok(())
    }
}

/// Turns a uid into a stable, filesystem- and URL-safe resource name.
///
/// The hash keeps the name unique even when sanitizing or truncating maps two
/// different uids onto the same slug.
fn object_name(uid: &str) -> String {
    let mut slug: String = uid
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    // Only ASCII at this point, so truncating on a byte index is safe
    slug.truncate(100);

    format!("{slug}-{:016x}.ics", fnv1a(uid))
}

/// FNV-1a, because `DefaultHasher` isn't guaranteed to be stable across releases
/// and the resource name has to stay the same forever.
fn fnv1a(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// iCalendar serialization
// ---------------------------------------------------------------------------

/// Escapes a value for a text property as described in
/// [RFC 5545 3.3.11](https://datatracker.ietf.org/doc/html/rfc5545#section-3.3.11)
fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str(r"\\"),
            ';' => escaped.push_str(r"\;"),
            ',' => escaped.push_str(r"\,"),
            '\n' => escaped.push_str(r"\n"),
            '\r' => (),
            other => escaped.push(other),
        }
    }
    escaped
}

fn quote_parameter(value: &str) -> String {
    if value
        .chars()
        .any(|character| matches!(character, ':' | ';' | ',' | ' '))
    {
        format!("\"{}\"", value.replace('"', ""))
    } else {
        value.to_string()
    }
}

/// Appends a content line, folded to 75 octets as required by
/// [RFC 5545 3.1](https://datatracker.ietf.org/doc/html/rfc5545#section-3.1).
///
/// `value` is expected to be escaped already.
fn push_property(
    calendar_object: &mut String,
    name: &str,
    parameters: &[(String, Vec<String>)],
    value: &str,
) {
    let mut line = String::from(name);
    for (key, values) in parameters {
        line.push(';');
        line.push_str(key);
        line.push('=');
        line.push_str(
            &values
                .iter()
                .map(|value| quote_parameter(value))
                .collect::<Vec<String>>()
                .join(","),
        );
    }
    line.push(':');
    line.push_str(value);

    let mut octets = 0;
    for character in line.chars() {
        if octets + character.len_utf8() > 75 {
            // Continuation lines start with a single space
            calendar_object.push_str("\r\n ");
            octets = 1;
        }
        calendar_object.push(character);
        octets += character.len_utf8();
    }
    calendar_object.push_str("\r\n");
}

/// A property we either build ourselves or carry over from the calendar unchanged.
struct RawProperty {
    name: String,
    parameters: Vec<(String, Vec<String>)>,
    /// Already escaped
    value: String,
}

impl RawProperty {
    fn new(name: &str, value: &str) -> Self {
        RawProperty {
            name: name.to_string(),
            parameters: Vec::new(),
            value: value.to_string(),
        }
    }

    /// Takes a property over as-is - the ical parser hands out unescaped values,
    /// so nothing has to be escaped again.
    fn inherited(property: &Property) -> Self {
        RawProperty {
            name: property.name.clone(),
            parameters: property.params.clone().unwrap_or_default(),
            value: property.value.clone().unwrap_or_default(),
        }
    }

    fn push_to(&self, calendar_object: &mut String) {
        push_property(calendar_object, &self.name, &self.parameters, &self.value);
    }
}

/// Copies whole components (`VTIMEZONE`) out of a calendar object verbatim, so that
/// `TZID` parameters we carry over keep pointing at a definition.
fn extract_components(calendar_object: &str, component: &str) -> Vec<String> {
    let begin = format!("BEGIN:{component}");
    let end = format!("END:{component}");

    let mut components = Vec::new();
    let mut current: Option<String> = None;

    for line in calendar_object.lines() {
        let line = line.trim_end_matches('\r');
        if current.is_none() && line.eq_ignore_ascii_case(&begin) {
            current = Some(String::new());
        }

        let is_end = line.eq_ignore_ascii_case(&end);
        if let Some(buffer) = current.as_mut() {
            buffer.push_str(line);
            buffer.push_str("\r\n");
        }
        if is_end {
            if let Some(buffer) = current.take() {
                components.push(buffer);
            }
        }
    }

    components
}

/// The event as it currently sits in the Apple Calendar
struct StoredEvent {
    event: IcalEvent,
    timezones: Vec<String>,
}

fn parse_stored_event(calendar_object: &str) -> Option<StoredEvent> {
    let calendar = IcalParser::new(BufReader::new(calendar_object.as_bytes()))
        .next()?
        .ok()?;

    Some(StoredEvent {
        event: calendar.events.into_iter().next()?,
        timezones: extract_components(calendar_object, "VTIMEZONE"),
    })
}

// ---------------------------------------------------------------------------
// Building the event
// ---------------------------------------------------------------------------

fn format_utc(date_time: DateTime<Utc>) -> String {
    date_time.format("%Y%m%dT%H%M%SZ").to_string()
}

fn parse_ical_utc(
    event: &IcalEvent,
    name: &str,
) -> Result<DateTime<Utc>, Box<dyn std::error::Error + Send + Sync>> {
    let value = event
        .get_property(name)
        .and_then(|property| property.value.clone())
        .ok_or(format!("Required property {name} missing"))?;

    let date_time = value.get(0..15).ok_or(format!(
        "Property {name} has an unsupported value {value:?}"
    ))?;

    Ok(NaiveDateTime::parse_from_str(date_time, "%Y%m%dT%H%M%S")?.and_utc())
}

fn build_description(
    event: &IcalEvent,
    room: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let description = event
        .get_property("DESCRIPTION")
        .and_then(|property| property.value.clone())
        .map(|description| description.split(r"\;").skip(2).collect::<String>())
        .unwrap_or_default()
        .replace(r"\n", "\n")
        .replace(r"\", "")
        .trim()
        .to_string();

    let lowercase_room = room.to_lowercase();
    let location_hint = if lowercase_room.contains("online") {
        if lowercase_room.contains("moodle") {
            String::from("Online auf Moodle: https://www.moodle.tum.de/my/")
        } else {
            String::from("Online")
        }
    } else {
        let mut link = Url::parse("https://nav.tum.de/search")?;
        link.query_pairs_mut().append_pair("q", room);
        format!("Wo ist das? {link}")
    };

    Ok(if description.is_empty() {
        location_hint
    } else {
        format!("{description}\n{location_hint}")
    })
}

/// Builds the calendar object that gets uploaded.
///
/// `stored` and `property_changes` are `None` while creating an event, in which case
/// everything is taken from the ICS calendar. While updating, only the properties
/// which actually changed in the ICS calendar are overwritten - everything else is
/// carried over from the Apple Calendar so that manual edits survive.
fn build_calendar_object(
    uid: &str,
    event: &IcalEvent,
    stored: Option<&StoredEvent>,
    property_changes: Option<&[PropertyChange]>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let stored_event = stored.map(|stored| &stored.event);
    let unchanged = |name: &str| {
        property_changes.is_some_and(|changes| {
            !changes
                .iter()
                .any(|property_change| property_change.key == name)
        })
    };
    let inherited = |name: &str| {
        if unchanged(name) {
            stored_event.and_then(|event| event.get_property(name))
        } else {
            None
        }
    };

    // Times - both are carried over together, otherwise an event could end before it starts
    let stored_times = if unchanged("DTSTART") && unchanged("DTEND") {
        stored_event
            .and_then(|stored| stored.get_property("DTSTART"))
            .zip(stored_event.and_then(|stored| stored.get_property("DTEND")))
    } else {
        None
    };
    let (start, end) = match stored_times {
        Some((start, end)) => (RawProperty::inherited(start), RawProperty::inherited(end)),
        None => (
            RawProperty::new("DTSTART", &format_utc(parse_ical_utc(event, "DTSTART")?)),
            RawProperty::new("DTEND", &format_utc(parse_ical_utc(event, "DTEND")?)),
        ),
    };

    let (summary, is_exam) = match inherited("SUMMARY") {
        Some(summary) => {
            let summary = RawProperty::inherited(summary);
            let is_exam = summary.value.contains("Prüfung");
            (summary, is_exam)
        }
        None => match event
            .get_property("SUMMARY")
            .and_then(|summary| summary.value.clone())
        {
            Some(summary) => (
                RawProperty::new(
                    "SUMMARY",
                    &escape_text(&replace_courses(summary.replace(r"\", "").as_str())),
                ),
                summary.contains("Prüfung"),
            ),
            None => (
                RawProperty::new("SUMMARY", &escape_text("Kein Titel angegeben")),
                false,
            ),
        },
    };

    let room = event
        .get_property("LOCATION")
        .and_then(|location| location.value.clone())
        .map(|location| location.replace(r"\", ""))
        .unwrap_or_else(|| "Kein Ort angegeben".to_string());
    let location = match inherited("LOCATION") {
        Some(location) => RawProperty::inherited(location),
        None => RawProperty::new("LOCATION", &escape_text(&room)),
    };

    // The description embeds the room, so it also has to be rebuilt when the room changed
    let stored_description = if unchanged("DESCRIPTION") && unchanged("LOCATION") {
        stored_event.and_then(|stored| stored.get_property("DESCRIPTION"))
    } else {
        None
    };
    let description = match stored_description {
        Some(description) => RawProperty::inherited(description),
        None => RawProperty::new(
            "DESCRIPTION",
            &escape_text(&build_description(event, &room)?),
        ),
    };

    let status = match inherited("STATUS") {
        Some(status) => Some(RawProperty::inherited(status)),
        None => event
            .get_property("STATUS")
            .and_then(|status| status.value.clone())
            .map(|status| RawProperty::new("STATUS", &escape_text(&status.to_uppercase()))),
    };

    // Bumping the sequence tells the calendar clients that this is a newer revision
    let sequence = stored_event
        .and_then(|stored| stored.get_property("SEQUENCE"))
        .and_then(|sequence| sequence.value.clone())
        .and_then(|sequence| sequence.trim().parse::<u32>().ok())
        .map(|sequence| sequence.saturating_add(1))
        .unwrap_or(0);

    let now = format_utc(Utc::now());
    let mut calendar_object = String::new();

    calendar_object.push_str("BEGIN:VCALENDAR\r\n");
    push_property(&mut calendar_object, "VERSION", &[], "2.0");
    push_property(&mut calendar_object, "PRODID", &[], PRODID);
    push_property(&mut calendar_object, "CALSCALE", &[], "GREGORIAN");

    // Only needed if we carried over times which might reference a TZID
    if stored_times.is_some() {
        for timezone in stored
            .map(|stored| stored.timezones.as_slice())
            .unwrap_or(&[])
        {
            calendar_object.push_str(timezone);
        }
    }

    calendar_object.push_str("BEGIN:VEVENT\r\n");
    push_property(&mut calendar_object, "UID", &[], &escape_text(uid));
    push_property(&mut calendar_object, "DTSTAMP", &[], &now);
    start.push_to(&mut calendar_object);
    end.push_to(&mut calendar_object);
    summary.push_to(&mut calendar_object);
    location.push_to(&mut calendar_object);
    description.push_to(&mut calendar_object);

    if let Some(url) = event.get_property("URL").and_then(|url| url.value.clone()) {
        push_property(&mut calendar_object, "URL", &[], &escape_text(&url));
    }
    if let Some(status) = status {
        status.push_to(&mut calendar_object);
    }
    if is_exam {
        push_property(
            &mut calendar_object,
            "CATEGORIES",
            &[],
            &escape_text("Prüfung"),
        );
        // Apple Calendar shows RFC 7986 colors on supported clients
        push_property(&mut calendar_object, "COLOR", &[], "tomato");
    }

    push_property(&mut calendar_object, "SEQUENCE", &[], &sequence.to_string());
    push_property(&mut calendar_object, "LAST-MODIFIED", &[], &now);
    calendar_object.push_str("END:VEVENT\r\n");
    calendar_object.push_str("END:VCALENDAR\r\n");

    Ok(calendar_object)
}

// ---------------------------------------------------------------------------
// Syncing
// ---------------------------------------------------------------------------

async fn create_event(
    calendar: &AppleCalendar,
    uid: String,
    event: IcalEvent,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = calendar.object_url(&uid)?;
    let calendar_object = build_calendar_object(&uid, &event, None, None)?;

    calendar.put_object(&url, &calendar_object, None).await
}

async fn update_event(
    calendar: &AppleCalendar,
    uid: String,
    event: IcalEvent,
    property_changes: Vec<PropertyChange>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Updating event {uid}: {property_changes:?}");

    let url = calendar.object_url(&uid)?;
    let Some((stored_object, etag)) = calendar.fetch_object(&url).await? else {
        // The event was deleted in the Apple Calendar - don't bring it back
        println!("Event {uid} isn't in the Apple Calendar anymore, skipping update");
        return Ok(());
    };

    let stored = parse_stored_event(&stored_object);
    let calendar_object =
        build_calendar_object(&uid, &event, stored.as_ref(), Some(&property_changes))?;

    calendar
        .put_object(&url, &calendar_object, etag.as_deref())
        .await
}

async fn delete_event(
    calendar: &AppleCalendar,
    uid: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = calendar.object_url(&uid)?;

    calendar.delete_object(&url).await
}

fn is_video_transmission(event: &IcalEvent) -> bool {
    event
        .get_property("DESCRIPTION")
        .and_then(|property| property.value.clone())
        .is_some_and(|description| description.contains("Videoübertragung aus"))
}

/// The TUM Calendar seems to randomly serve english / german descriptions.
/// This looks for differences other than the first two words in english / german.
fn is_language_only_update(property_changes: &[PropertyChange]) -> bool {
    property_changes.len() == 1
        && property_changes[0].key == "DESCRIPTION"
        && property_changes[0]
            .from
            .as_ref()
            .and_then(|from| from.value.as_ref())
            .zip(
                property_changes[0]
                    .to
                    .as_ref()
                    .and_then(|to| to.value.as_ref()),
            )
            .is_some_and(|(from, to)| {
                from.split(";").skip(2).collect::<String>()
                    == to.split(";").skip(2).collect::<String>()
            })
}

/// If the event is in the far past, we assume it's just the calendar updating
/// for the next semester, which means we don't actually need to delete it
///
/// The cutoff is a week back, the same one [`tum_google_sync`](crate::tum_google_sync)
/// uses, so both adapters keep and delete the same events.
fn is_far_in_the_past(event: &IcalEvent) -> bool {
    event
        .get_property("DTEND")
        .and_then(|property| property.value.clone())
        .and_then(|value| {
            value
                .get(0..15)
                .and_then(|value| NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok())
                .map(|date_time| date_time.and_utc())
        })
        .is_some_and(|end| end < Utc::now() - Duration::from_secs(60 * 60 * 24 * 7))
}

/// This is a callback which synchronizes your TUM Calendar to your Apple (iCloud) Calendar.
///
/// It behaves like [`tum_google_sync`](crate::tum_google_sync): the event summaries will be
/// shortened and the events themselves modifieable. As soon as you delete an event, it won't
/// come back. If you modify an event, your changes will only be overwritten if they're changed
/// in the TUM Calendar.
///
/// # Examples
///
/// ```
/// let apple_calendar = AppleCalendar::connect(&apple_id, &app_password, "TUM")
///     .await
///     .expect("Failed to connect to the Apple Calendar");
///
/// let mut ics_watcher = ICSWatcher::new(
///     tum_url.as_str(),
///     vec![
///         Box::new(move |a, b, e| {
///             let calendar = apple_calendar.clone();
///             Box::pin(async move { tum_apple_sync(&calendar, a, b, e).await })
///         }),
///     ],
/// );
///
/// // Try to load backup
/// let _ = ics_watcher.load_backup("TUM Calendar");
/// ics_watcher
///     .run(Option::from("TUM Calendar"))
///     .await
///     .expect("ICS Watcher crashed");
/// ```
pub async fn tum_apple_sync(
    calendar: &AppleCalendar,
    _: Option<String>,
    _: Option<String>,
    events: Vec<CalendarEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for event in events {
        let result = match event {
            CalendarEvent::Setup(EventData { uid, ical_data }) => {
                // Don't sync if event is a video transmission
                if is_video_transmission(&ical_data) {
                    println!("Skipping video transmission event {uid}");
                    Ok(())
                } else {
                    println!("Setting up event {uid}");
                    create_event(calendar, uid, ical_data).await
                }
            }
            CalendarEvent::Created(EventData { uid, ical_data }) => {
                // Don't sync if event is a video transmission
                if is_video_transmission(&ical_data) {
                    Ok(())
                } else {
                    println!("Creating event {uid}");
                    create_event(calendar, uid, ical_data).await
                }
            }
            CalendarEvent::Updated {
                event: EventData { uid, ical_data },
                changed_properties,
            } => {
                if is_language_only_update(&changed_properties) {
                    // Update is a language-only update
                    Ok(())
                } else {
                    update_event(calendar, uid, ical_data, changed_properties).await
                }
            }
            CalendarEvent::Deleted(EventData { uid, ical_data }) => {
                println!("Deleting event {uid}");
                if is_far_in_the_past(&ical_data) {
                    // Not deleting event as it is far back in the past
                    Ok(())
                } else {
                    delete_event(calendar, uid).await
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

    fn property(name: &str, value: &str) -> Property {
        Property {
            name: String::from(name),
            params: None,
            value: Some(String::from(value)),
        }
    }

    /// Builds a calendar object and undoes the line folding, so that assertions
    /// don't depend on where a long line happens to be split
    fn build_unfolded(
        event: &IcalEvent,
        stored: Option<&StoredEvent>,
        property_changes: Option<&[PropertyChange]>,
    ) -> String {
        build_calendar_object("1234567@tum.de", event, stored, property_changes)
            .expect("calendar object should build")
            .replace("\r\n ", "")
    }

    fn tum_event() -> IcalEvent {
        IcalEvent {
            properties: vec![
                property("UID", "1234567@tum.de"),
                property("DTSTART", "20250114T100000Z"),
                property("DTEND", "20250114T120000Z"),
                property("SUMMARY", "Analysis 1"),
                property("LOCATION", r"5608.EG.011\, Hörsaal"),
                property("DESCRIPTION", r"fix\;me\;Vorlesung\nZweite Zeile"),
            ],
            alarms: vec![],
        }
    }

    #[test]
    fn escapes_text() {
        assert_eq!(escape_text("a,b;c\\d\ne"), String::from(r"a\,b\;c\\d\ne"));
    }

    #[test]
    fn folds_long_lines() {
        let mut calendar_object = String::new();
        push_property(&mut calendar_object, "DESCRIPTION", &[], &"x".repeat(200));

        for line in calendar_object.lines() {
            assert!(line.len() <= 75, "line too long: {}", line.len());
        }
        // Unfolding has to yield the original line again
        assert_eq!(
            calendar_object.replace("\r\n ", ""),
            format!("DESCRIPTION:{}\r\n", "x".repeat(200))
        );
    }

    #[test]
    fn folds_without_splitting_characters() {
        let mut calendar_object = String::new();
        push_property(&mut calendar_object, "SUMMARY", &[], &"ü".repeat(100));

        assert!(calendar_object.contains("\r\n "));
        assert_eq!(
            calendar_object.replace("\r\n ", ""),
            format!("SUMMARY:{}\r\n", "ü".repeat(100))
        );
    }

    #[test]
    fn serializes_parameters() {
        let mut calendar_object = String::new();
        push_property(
            &mut calendar_object,
            "DTSTART",
            &[(String::from("TZID"), vec![String::from("Europe/Berlin")])],
            "20250114T100000",
        );

        assert_eq!(
            calendar_object,
            "DTSTART;TZID=Europe/Berlin:20250114T100000\r\n"
        );
    }

    #[test]
    fn object_names_are_stable_and_safe() {
        let name = object_name("1234567@tum.de");

        assert_eq!(name, object_name("1234567@tum.de"));
        assert!(name.ends_with(".ics"));
        assert!(name
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.')));
        assert_ne!(name, object_name("1234567-tum.de"));
    }

    #[test]
    fn creates_calendar_object() {
        let calendar_object = build_unfolded(&tum_event(), None, None);

        assert!(calendar_object.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(calendar_object.ends_with("END:VCALENDAR\r\n"));
        assert!(calendar_object.contains("UID:1234567@tum.de\r\n"));
        assert!(calendar_object.contains("DTSTART:20250114T100000Z\r\n"));
        assert!(calendar_object.contains("DTEND:20250114T120000Z\r\n"));
        assert!(calendar_object.contains(r"LOCATION:5608.EG.011\, Hörsaal"));
        assert!(calendar_object.contains("SEQUENCE:0\r\n"));
        // The first two \; separated segments are dropped, linebreaks are kept
        assert!(calendar_object.contains(r"DESCRIPTION:Vorlesung\nZweite Zeile\n"));
        assert!(calendar_object.contains("nav.tum.de"));
    }

    #[test]
    fn marks_exams() {
        let mut event = tum_event();
        event
            .properties
            .push(property("SUMMARY", "Prüfung Analysis"));
        event
            .properties
            .retain(|p| p.value.as_deref() != Some("Analysis 1"));

        let calendar_object = build_unfolded(&event, None, None);

        assert!(calendar_object.contains(r"CATEGORIES:Prüfung"));
        assert!(calendar_object.contains("COLOR:tomato"));
    }

    #[test]
    fn keeps_online_events_without_a_room_link() {
        let mut event = tum_event();
        event.properties.retain(|p| p.name != "LOCATION");
        event
            .properties
            .push(property("LOCATION", "Online: Moodle"));

        let calendar_object = build_unfolded(&event, None, None);

        assert!(calendar_object.contains("moodle.tum.de"));
        assert!(!calendar_object.contains("nav.tum.de"));
    }

    #[test]
    fn update_keeps_unchanged_properties() {
        let stored = parse_stored_event(
            "BEGIN:VCALENDAR\r\n\
             VERSION:2.0\r\n\
             BEGIN:VTIMEZONE\r\n\
             TZID:Europe/Berlin\r\n\
             END:VTIMEZONE\r\n\
             BEGIN:VEVENT\r\n\
             UID:1234567@tum.de\r\n\
             DTSTART;TZID=Europe/Berlin:20250114T110000\r\n\
             DTEND;TZID=Europe/Berlin:20250114T130000\r\n\
             SUMMARY:Mein eigener Titel\r\n\
             LOCATION:Zuhause\r\n\
             DESCRIPTION:Meine Notiz\r\n\
             SEQUENCE:3\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR\r\n",
        )
        .expect("stored event should parse");

        let changes = vec![PropertyChange {
            key: String::from("SUMMARY"),
            from: Some(property("SUMMARY", "Analysis 1 alt")),
            to: Some(property("SUMMARY", "Analysis 1")),
        }];

        let calendar_object = build_unfolded(&tum_event(), Some(&stored), Some(&changes));

        // Changed in the ICS calendar -> overwritten
        assert!(calendar_object.contains("SUMMARY:Analysis 1\r\n"));
        // Edited by hand and untouched by the ICS calendar -> kept, including the timezone
        assert!(calendar_object.contains("DTSTART;TZID=Europe/Berlin:20250114T110000\r\n"));
        assert!(calendar_object.contains("DTEND;TZID=Europe/Berlin:20250114T130000\r\n"));
        assert!(calendar_object.contains("BEGIN:VTIMEZONE\r\n"));
        assert!(calendar_object.contains("LOCATION:Zuhause\r\n"));
        assert!(calendar_object.contains("DESCRIPTION:Meine Notiz\r\n"));
        assert!(calendar_object.contains("SEQUENCE:4\r\n"));
    }

    #[test]
    fn update_rebuilds_description_when_the_room_changes() {
        let stored = parse_stored_event(
            "BEGIN:VCALENDAR\r\n\
             BEGIN:VEVENT\r\n\
             UID:1234567@tum.de\r\n\
             DTSTART:20250114T100000Z\r\n\
             DTEND:20250114T120000Z\r\n\
             SUMMARY:Analysis 1\r\n\
             LOCATION:Alter Raum\r\n\
             DESCRIPTION:Alte Beschreibung\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR\r\n",
        )
        .expect("stored event should parse");

        let changes = vec![PropertyChange {
            key: String::from("LOCATION"),
            from: Some(property("LOCATION", "Alter Raum")),
            to: Some(property("LOCATION", r"5608.EG.011\, Hörsaal")),
        }];

        let calendar_object = build_unfolded(&tum_event(), Some(&stored), Some(&changes));

        assert!(calendar_object.contains(r"LOCATION:5608.EG.011\, Hörsaal"));
        assert!(!calendar_object.contains("Alte Beschreibung"));
        assert!(calendar_object.contains("nav.tum.de"));
    }

    #[test]
    fn parses_a_multistatus() {
        let resources = parse_multistatus(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
              <response>
                <href>/1234/calendars/home/</href>
                <propstat>
                  <prop><resourcetype><collection/></resourcetype></prop>
                  <status>HTTP/1.1 200 OK</status>
                </propstat>
              </response>
              <response>
                <href>/1234/calendars/tum/</href>
                <propstat>
                  <prop>
                    <resourcetype><collection/><cal:calendar/></resourcetype>
                    <displayname>TUM</displayname>
                    <cal:supported-calendar-component-set>
                      <cal:comp name="VEVENT"/>
                    </cal:supported-calendar-component-set>
                  </prop>
                  <status>HTTP/1.1 200 OK</status>
                </propstat>
              </response>
              <response>
                <href>/1234/calendars/tasks/</href>
                <propstat>
                  <prop>
                    <resourcetype><collection/><cal:calendar/></resourcetype>
                    <displayname>Erinnerungen</displayname>
                    <cal:supported-calendar-component-set>
                      <cal:comp name="VTODO"/>
                    </cal:supported-calendar-component-set>
                  </prop>
                  <status>HTTP/1.1 200 OK</status>
                </propstat>
              </response>
            </multistatus>"#,
        )
        .unwrap();

        assert_eq!(resources.len(), 3);
        assert!(!resources[0].is_calendar);

        assert!(resources[1].is_calendar);
        assert_eq!(resources[1].href, "/1234/calendars/tum/");
        assert_eq!(resources[1].display_name.as_deref(), Some("TUM"));
        assert_eq!(
            resources[1].components.as_deref(),
            Some([String::from("VEVENT")].as_slice())
        );

        assert_eq!(
            resources[2].components.as_deref(),
            Some([String::from("VTODO")].as_slice())
        );
    }

    #[test]
    fn parses_a_current_user_principal() {
        let resources = parse_multistatus(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <d:multistatus xmlns:d="DAV:">
              <d:response>
                <d:href>/</d:href>
                <d:propstat>
                  <d:prop>
                    <d:current-user-principal>
                      <d:href>/1234/principal/</d:href>
                    </d:current-user-principal>
                  </d:prop>
                  <d:status>HTTP/1.1 200 OK</d:status>
                </d:propstat>
              </d:response>
            </d:multistatus>"#,
        )
        .unwrap();

        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].href, "/");
        assert_eq!(
            resources[0]
                .property_hrefs
                .get("current-user-principal")
                .map(String::as_str),
            Some("/1234/principal/")
        );
    }
}
