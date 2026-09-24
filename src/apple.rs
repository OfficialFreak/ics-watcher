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

use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
    time::Duration,
};

use chrono::{DateTime, NaiveDateTime, Utc};

use ical::{
    parser::{ical::component::IcalEvent, Component},
    property::Property,
    IcalParser,
};

use quick_xml::{escape::unescape, events::Event as XmlEvent, Reader};
use reqwest::{
    header::{CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH},
    Client, Method, RequestBuilder, Response, StatusCode, Url,
};
use tokio::time::sleep;

use crate::{replace_courses, unescape_location, CalendarEvent, EventData, PropertyChange};

/// The CalDAV entry point of iCloud, used by [`AppleCalendar::connect`].
pub const ICLOUD_CALDAV_URL: &str = "https://caldav.icloud.com/";

pub(crate) const PRODID: &str = "-//ics-watcher//Apple Calendar Adapter//EN";

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

const OBJECT_LIST_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:getetag/></d:prop>
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

/// How long to wait before each retry of a request that failed for a temporary reason. iCloud
/// answers the odd request with a 500, which works when sent again a moment later.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Whether the server had a temporary problem - unlike a 4xx, which means that the request
/// itself is wrong and sending it again won't help.
fn is_transient_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Whether the request didn't get through: the connection couldn't be established, broke off
/// or timed out.
fn is_transient_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

#[derive(Clone)]
struct DavClient {
    http: Client,
    username: String,
    password: String,
    /// See [RETRY_DELAYS]
    retry_delays: &'static [Duration],
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
            retry_delays: &RETRY_DELAYS,
        })
    }

    fn request(&self, method: Method, url: &Url) -> RequestBuilder {
        self.http
            .request(method, url.clone())
            .basic_auth(&self.username, Some(&self.password))
    }

    /// Sends a request, and sends it again as long as it fails for a temporary reason (see
    /// [is_transient_status] and [is_transient_error]), waiting a bit longer every time.
    ///
    /// A request can only be sent once, so `prepare` is called for every attempt to add the
    /// headers and the body.
    async fn send(
        &self,
        method: Method,
        url: &Url,
        prepare: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response, reqwest::Error> {
        let mut delays = self.retry_delays.iter();

        loop {
            let result = prepare(self.request(method.clone(), url)).send().await;
            let problem = match &result {
                Ok(response) if is_transient_status(response.status()) => {
                    Some(response.status().to_string())
                }
                Err(error) if is_transient_error(error) => Some(error.to_string()),
                _ => None,
            };

            match (problem, delays.next()) {
                (Some(problem), Some(delay)) => {
                    eprintln!("{method} {url} failed ({problem}), retrying in {delay:?}");
                    sleep(*delay).await;
                }
                _ => return result,
            }
        }
    }

    async fn propfind(
        &self,
        url: &Url,
        depth: &str,
        body: &'static str,
    ) -> Result<Vec<DavResource>, Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .send(Method::from_bytes(b"PROPFIND")?, url, |request| {
                request
                    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                    .header("Depth", depth)
                    .body(body)
            })
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

    list_calendars(dav, &home_url).await
}

/// The calendars in a calendar home, the collection all calendars of an account live in.
async fn list_calendars(
    dav: &DavClient,
    home_url: &Url,
) -> Result<Vec<CalendarInfo>, Box<dyn std::error::Error + Send + Sync>> {
    let mut calendars = Vec::new();
    for resource in dav.propfind(home_url, "1", CALENDAR_LIST_BODY).await? {
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

/// The URL of the calendar named `calendar_name`, or an error listing the ones there are.
fn find_calendar(
    calendars: &[CalendarInfo],
    calendar_name: &str,
) -> Result<Url, Box<dyn std::error::Error + Send + Sync>> {
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

    Ok(Url::parse(&calendar.url)?)
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
    /// Where exams go instead, see [`AppleCalendar::with_exam_calendar`]
    exam_calendar_url: Option<Url>,
}

impl std::fmt::Debug for AppleCalendar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately without the credentials
        formatter
            .debug_struct("AppleCalendar")
            .field("username", &self.dav.username)
            .field("calendar_url", &self.calendar_url.as_str())
            .field(
                "exam_calendar_url",
                &self.exam_calendar_url.as_ref().map(Url::as_str),
            )
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

        Ok(AppleCalendar {
            calendar_url: find_calendar(&calendars, calendar_name)?,
            dav,
            exam_calendar_url: None,
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
            exam_calendar_url: None,
        })
    }

    /// Puts exams (events with "Prüfung" in their title) into the calendar named
    /// `calendar_name` instead, so that they can have a color of their own - Apple Calendar
    /// only colors whole calendars, not single events. It has to belong to the same account.
    ///
    /// Exams which are in the main calendar already can be moved over with
    /// [`move_exams_to_exam_calendar`](crate::move_exams_to_exam_calendar).
    pub async fn with_exam_calendar(
        mut self,
        calendar_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // All calendars of an account live side by side in its calendar home
        let home_url = self.calendar_url.join("../")?;
        let calendars = list_calendars(&self.dav, &home_url).await?;

        self.exam_calendar_url = Some(find_calendar(&calendars, calendar_name)?);
        Ok(self)
    }

    /// Same as [`AppleCalendar::with_exam_calendar`], but skips the discovery and uses the
    /// calendar collection at `calendar_url` directly.
    pub fn with_exam_calendar_url(
        mut self,
        calendar_url: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        self.exam_calendar_url = Some(ensure_collection_url(Url::parse(calendar_url)?)?);
        Ok(self)
    }

    pub fn calendar_url(&self) -> &str {
        self.calendar_url.as_str()
    }

    pub fn exam_calendar_url(&self) -> Option<&str> {
        self.exam_calendar_url.as_ref().map(Url::as_str)
    }

    /// The URL an event is stored at in the main calendar. Derived from the uid, so the same
    /// event always ends up at the same place - no lookup needed.
    pub(crate) fn object_url(
        &self,
        uid: &str,
    ) -> Result<Url, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.calendar_url.join(&object_name(uid))?)
    }

    /// The calendars an event can be in: the one it belongs into first, then the other one,
    /// if exams have a calendar of their own. Events can end up in the other one when they
    /// got moved over by hand, or were created before there was an exam calendar.
    pub(crate) fn calendar_urls(&self, exam: bool) -> Vec<&Url> {
        match &self.exam_calendar_url {
            Some(exam_calendar_url) if exam => vec![exam_calendar_url, &self.calendar_url],
            Some(exam_calendar_url) => vec![&self.calendar_url, exam_calendar_url],
            None => vec![&self.calendar_url],
        }
    }

    /// The resource names of all calendar objects currently in the calendar.
    pub(crate) async fn object_names(
        &self,
    ) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        let resources = self
            .dav
            .propfind(&self.calendar_url, "1", OBJECT_LIST_BODY)
            .await?;

        Ok(resources
            .into_iter()
            .filter_map(|resource| {
                let url = self.calendar_url.join(&resource.href).ok()?;
                // The calendar collection lists itself as well
                if url.path() == self.calendar_url.path() {
                    return None;
                }
                url.path_segments()?
                    .next_back()
                    .filter(|name| !name.is_empty())
                    .map(|name| name.to_string())
            })
            .collect())
    }

    /// Returns the stored calendar object and its etag, or `None` if it doesn't exist
    /// (anymore).
    pub(crate) async fn fetch_object(
        &self,
        url: &Url,
    ) -> Result<Option<(String, Option<String>)>, Box<dyn std::error::Error + Send + Sync>> {
        let response = self.dav.send(Method::GET, url, |request| request).await?;

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

    /// Uploads a calendar object unless there already is one at `url`.
    ///
    /// Returns whether the object was created. When a server error stored the object after
    /// all, the retry finds it already there and `false` is returned.
    pub(crate) async fn create_object(
        &self,
        url: &Url,
        calendar_object: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .dav
            .send(Method::PUT, url, |request| {
                request
                    .header(CONTENT_TYPE, "text/calendar; charset=utf-8")
                    .header(IF_NONE_MATCH, "*")
                    .body(calendar_object.to_string())
            })
            .await?;
        let status = response.status();

        if status == StatusCode::PRECONDITION_FAILED {
            return Ok(false);
        }
        if !status.is_success() {
            // The server usually explains why it rejected the event
            let reason: String = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            return Err(format!("PUT {url} failed with status {status}: {reason}").into());
        }

        Ok(true)
    }

    pub(crate) async fn put_object(
        &self,
        url: &Url,
        calendar_object: &str,
        etag: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .dav
            .send(Method::PUT, url, |request| {
                let request = request
                    .header(CONTENT_TYPE, "text/calendar; charset=utf-8")
                    .body(calendar_object.to_string());

                match etag {
                    Some(etag) => request.header(IF_MATCH, etag),
                    None => request,
                }
            })
            .await?;
        let status = response.status();

        // Also happens when a server error stored the event after all and the retry no
        // longer matches the etag - there's no telling the two apart
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

    /// Deletes a calendar object - with an `etag`, only if it hasn't changed since.
    pub(crate) async fn delete_object(
        &self,
        url: &Url,
        etag: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .dav
            .send(Method::DELETE, url, |request| match etag {
                Some(etag) => request.header(IF_MATCH, etag),
                None => request,
            })
            .await?;
        let status = response.status();

        // Already gone - nothing to do
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if status == StatusCode::PRECONDITION_FAILED {
            return Err(format!(
                "DELETE {url} was rejected because the event changed on the server in the meantime"
            )
            .into());
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
pub(crate) fn object_name(uid: &str) -> String {
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
pub(crate) fn escape_text(value: &str) -> String {
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

/// Appends a property as a content line, see [push_line].
///
/// `value` is expected to be escaped already.
pub(crate) fn push_property(
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

    push_line(calendar_object, &line);
}

/// Appends a content line, folded to 75 octets as required by
/// [RFC 5545 3.1](https://datatracker.ietf.org/doc/html/rfc5545#section-3.1).
pub(crate) fn push_line(calendar_object: &mut String, line: &str) {
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
pub(crate) struct StoredEvent {
    pub(crate) event: IcalEvent,
    timezones: Vec<String>,
}

pub(crate) fn parse_stored_event(calendar_object: &str) -> Option<StoredEvent> {
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

pub(crate) fn format_utc(date_time: DateTime<Utc>) -> String {
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

    let location_hint = location_hint(room)?;

    Ok(if description.is_empty() {
        location_hint
    } else {
        format!("{description}\n{location_hint}")
    })
}

/// The last line of every description: where to find the room, or that the event is online.
pub(crate) fn location_hint(
    room: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let lowercase_room = room.to_lowercase();

    Ok(if lowercase_room.contains("online") {
        if lowercase_room.contains("moodle") {
            String::from("Online auf Moodle: https://www.moodle.tum.de/my/")
        } else {
            String::from("Online")
        }
    } else {
        let mut link = Url::parse("https://nav.tum.de/search")?;
        link.query_pairs_mut().append_pair("q", room);
        format!("Wo ist das? {link}")
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
        .map(|location| unescape_location(&location))
        .unwrap_or_else(|| "Kein Ort angegeben".to_string());
    let inherited_location = inherited("LOCATION");
    let location = match inherited_location {
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

    let url = event.get_property("URL").and_then(|url| url.value.clone());

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

    if let Some(url) = &url {
        push_property(&mut calendar_object, "URL", &[], &escape_text(url));
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

    // Whatever else the stored event has (colors, properties the calendar apps added, …) is
    // carried over, otherwise every update from the ICS calendar would wipe it
    if let Some(stored_event) = stored_event {
        let mut managed = vec![
            "UID",
            "DTSTAMP",
            "DTSTART",
            "DTEND",
            "DURATION",
            "SUMMARY",
            "LOCATION",
            "DESCRIPTION",
            "STATUS",
            "SEQUENCE",
            "LAST-MODIFIED",
        ];
        if url.is_some() {
            managed.push("URL");
        }
        if is_exam {
            managed.extend(["CATEGORIES", "COLOR"]);
        }
        if inherited_location.is_none() {
            // Apple's map pin would still point to the old room
            managed.push("X-APPLE-STRUCTURED-LOCATION");
        }

        for property in stored_event.properties.iter().filter(|property| {
            !managed
                .iter()
                .any(|name| property.name.eq_ignore_ascii_case(name))
        }) {
            RawProperty::inherited(property).push_to(&mut calendar_object);
        }
    }

    push_property(&mut calendar_object, "SEQUENCE", &[], &sequence.to_string());
    push_property(&mut calendar_object, "LAST-MODIFIED", &[], &now);

    // Alarms are always set by hand, so they're kept as they are
    for alarm in stored_event
        .map(|stored_event| stored_event.alarms.as_slice())
        .unwrap_or_default()
    {
        calendar_object.push_str("BEGIN:VALARM\r\n");
        for property in &alarm.properties {
            RawProperty::inherited(property).push_to(&mut calendar_object);
        }
        calendar_object.push_str("END:VALARM\r\n");
    }
    calendar_object.push_str("END:VEVENT\r\n");
    calendar_object.push_str("END:VCALENDAR\r\n");

    Ok(calendar_object)
}

// ---------------------------------------------------------------------------
// Syncing
// ---------------------------------------------------------------------------

/// Whether an event is an exam, which gets marked as one and goes into the exam calendar, if
/// there is one.
pub(crate) fn is_exam(event: &IcalEvent) -> bool {
    event
        .get_property("SUMMARY")
        .and_then(|summary| summary.value.as_deref())
        .is_some_and(|summary| summary.contains("Prüfung"))
}

async fn create_event(
    calendar: &AppleCalendar,
    uid: String,
    event: IcalEvent,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = calendar.calendar_urls(is_exam(&event))[0].join(&object_name(&uid))?;
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

    let exam = is_exam(&event);
    let mut stored_at = None;
    for calendar_url in calendar.calendar_urls(exam) {
        let url = calendar_url.join(&object_name(&uid))?;
        if let Some((stored_object, etag)) = calendar.fetch_object(&url).await? {
            stored_at = Some((url, stored_object, etag));
            break;
        }
    }
    let Some((url, stored_object, etag)) = stored_at else {
        // The event was deleted in the Apple Calendar - don't bring it back
        println!("Event {uid} isn't in the Apple Calendar anymore, skipping update");
        return Ok(());
    };

    let stored = parse_stored_event(&stored_object);
    let calendar_object =
        build_calendar_object(&uid, &event, stored.as_ref(), Some(&property_changes))?;

    // An event that just became an exam, or stopped being one, moves over to the other
    // calendar. Otherwise it stays where it is, even if it was moved there by hand.
    let target_url = calendar.calendar_urls(exam)[0].join(&object_name(&uid))?;
    let was_exam = property_changes
        .iter()
        .find(|property_change| property_change.key == "SUMMARY")
        .map(|property_change| {
            property_change
                .from
                .as_ref()
                .and_then(|from| from.value.as_deref())
                .is_some_and(|summary| summary.contains("Prüfung"))
        });
    if target_url != url && was_exam.is_some_and(|was_exam| was_exam != exam) {
        println!("Moving event {uid} to {target_url}");
        calendar
            .put_object(&target_url, &calendar_object, None)
            .await?;
        return calendar.delete_object(&url, etag.as_deref()).await;
    }

    calendar
        .put_object(&url, &calendar_object, etag.as_deref())
        .await
}

async fn delete_event(
    calendar: &AppleCalendar,
    uid: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // The event could be in either calendar
    for calendar_url in calendar.calendar_urls(false) {
        calendar
            .delete_object(&calendar_url.join(&object_name(&uid))?, None)
            .await?;
    }

    Ok(())
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

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
    fn turns_line_breaks_in_locations_into_commas() {
        let mut event = tum_event();
        event.properties.retain(|p| p.name != "LOCATION");
        event
            .properties
            .push(property("LOCATION", r"Online: Videokonferenz\nZoom etc."));

        let calendar_object = build_unfolded(&event, None, None);

        assert!(calendar_object.contains(r"LOCATION:Online: Videokonferenz\, Zoom etc."));
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

    fn stored_event_with_extras() -> StoredEvent {
        parse_stored_event(
            "BEGIN:VCALENDAR\r\n\
             BEGIN:VEVENT\r\n\
             UID:1234567@tum.de\r\n\
             DTSTART:20250114T100000Z\r\n\
             DTEND:20250114T120000Z\r\n\
             SUMMARY:Analysis 1 alt\r\n\
             LOCATION:Zuhause\r\n\
             DESCRIPTION:Meine Notiz\r\n\
             COLOR:seagreen\r\n\
             X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\n\
             X-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-TITLE=Zuhause:geo:48.1,11.5\r\n\
             BEGIN:VALARM\r\n\
             ACTION:DISPLAY\r\n\
             TRIGGER:-PT1H\r\n\
             END:VALARM\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR\r\n",
        )
        .expect("stored event should parse")
    }

    #[test]
    fn update_keeps_alarms_and_other_properties() {
        let changes = vec![PropertyChange {
            key: String::from("SUMMARY"),
            from: Some(property("SUMMARY", "Analysis 1 alt")),
            to: Some(property("SUMMARY", "Analysis 1")),
        }];

        let calendar_object = build_unfolded(
            &tum_event(),
            Some(&stored_event_with_extras()),
            Some(&changes),
        );

        assert!(calendar_object.contains("SUMMARY:Analysis 1\r\n"));
        assert_eq!(calendar_object.matches("SUMMARY:").count(), 1);
        assert!(calendar_object.contains("COLOR:seagreen\r\n"));
        assert!(calendar_object.contains("X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\n"));
        assert!(calendar_object
            .contains("X-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-TITLE=Zuhause:geo:48.1,11.5\r\n"));
        assert!(calendar_object
            .contains("BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT1H\r\nEND:VALARM\r\n"));
    }

    #[test]
    fn update_drops_the_map_pin_when_the_room_changes() {
        let changes = vec![PropertyChange {
            key: String::from("LOCATION"),
            from: Some(property("LOCATION", "Alter Raum")),
            to: Some(property("LOCATION", r"5608.EG.011\, Hörsaal")),
        }];

        let calendar_object = build_unfolded(
            &tum_event(),
            Some(&stored_event_with_extras()),
            Some(&changes),
        );

        assert!(calendar_object.contains(r"LOCATION:5608.EG.011\, Hörsaal"));
        assert!(!calendar_object.contains("X-APPLE-STRUCTURED-LOCATION"));
        // Everything unrelated to the room stays
        assert!(calendar_object.contains("COLOR:seagreen\r\n"));
        assert!(calendar_object.contains("BEGIN:VALARM\r\n"));
    }

    #[test]
    fn exams_keep_their_own_color_on_update() {
        let mut event = tum_event();
        event.properties.retain(|p| p.name != "SUMMARY");
        event
            .properties
            .push(property("SUMMARY", "Prüfung Analysis"));
        let changes = vec![PropertyChange {
            key: String::from("SUMMARY"),
            from: Some(property("SUMMARY", "Analysis 1 alt")),
            to: Some(property("SUMMARY", "Prüfung Analysis")),
        }];

        let calendar_object =
            build_unfolded(&event, Some(&stored_event_with_extras()), Some(&changes));

        assert!(calendar_object.contains("COLOR:tomato\r\n"));
        assert_eq!(calendar_object.matches("COLOR:").count(), 1);
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

    const NO_DELAYS: [Duration; 3] = [Duration::ZERO; 3];

    /// A tiny local server standing in for iCloud. `respond` gets the request line (e.g.
    /// `PUT /calendars/tum/x.ics`) and returns the status and body to answer with. Also
    /// returns the request lines the server got.
    async fn test_server(
        respond: impl Fn(&str) -> (u16, String) + Send + 'static,
    ) -> (Url, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_url =
            Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));

        let log = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();

                // Read the whole request first, otherwise sending the body could fail
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let Some(head_end) = request.windows(4).position(|end| end == b"\r\n\r\n")
                    else {
                        if read == 0 {
                            break;
                        }
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]).to_lowercase();
                    let body_length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map_or(0, |length| length.trim().parse().unwrap());
                    if read == 0 || request.len() >= head_end + 4 + body_length {
                        break;
                    }
                }

                // "PUT /calendars/tum/x.ics HTTP/1.1" without the version
                let request_line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .and_then(|line| line.rsplit_once(' '))
                    .map(|(request_line, _)| request_line.to_string())
                    .unwrap_or_default();
                let (status, body) = respond(&request_line);
                log.lock().unwrap().push(request_line);

                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (server_url, requests)
    }

    /// A calendar at `/calendars/tum/`, which doesn't wait before retrying
    fn calendar_on(server_url: &Url) -> AppleCalendar {
        AppleCalendar {
            dav: DavClient {
                retry_delays: &NO_DELAYS,
                ..DavClient::new("me@icloud.com", "abcd-efgh-ijkl-mnop").unwrap()
            },
            calendar_url: server_url.join("calendars/tum/").unwrap(),
            exam_calendar_url: None,
        }
    }

    /// Same, with the exams going into `/calendars/exams/`
    fn calendar_with_exams_on(server_url: &Url) -> AppleCalendar {
        AppleCalendar {
            exam_calendar_url: Some(server_url.join("calendars/exams/").unwrap()),
            ..calendar_on(server_url)
        }
    }

    /// A calendar on a server answering with `statuses` one after another, repeating the
    /// last one
    async fn calendar_answering(
        statuses: &'static [u16],
    ) -> (AppleCalendar, Arc<Mutex<Vec<String>>>) {
        let answered = AtomicUsize::new(0);
        let (server_url, requests) = test_server(move |_| {
            let index = answered.fetch_add(1, Ordering::SeqCst);
            (statuses[index.min(statuses.len() - 1)], String::new())
        })
        .await;

        (calendar_on(&server_url), requests)
    }

    #[test]
    fn tells_temporary_server_errors_apart() {
        for status in [500, 502, 503, 504] {
            assert!(
                is_transient_status(StatusCode::from_u16(status).unwrap()),
                "{status}"
            );
        }
        for status in [200, 201, 204, 400, 401, 403, 404, 412, 501] {
            assert!(
                !is_transient_status(StatusCode::from_u16(status).unwrap()),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn tells_connection_problems_apart() {
        // Nothing listens on the port anymore once the listener is gone
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert!(is_transient_error(&error));

        let error = Client::new().get("not a url").send().await.unwrap_err();
        assert!(!is_transient_error(&error));
    }

    #[tokio::test]
    async fn retries_server_errors() {
        let (calendar, requests) = calendar_answering(&[500, 503, 201]).await;
        let url = calendar.object_url("1234567@tum.de").unwrap();

        assert!(calendar
            .create_object(&url, "BEGIN:VCALENDAR")
            .await
            .unwrap());
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn gives_up_after_the_last_retry() {
        let (calendar, requests) = calendar_answering(&[502]).await;
        let url = calendar.object_url("1234567@tum.de").unwrap();

        let error = calendar
            .put_object(&url, "BEGIN:VCALENDAR", None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("502"));
        assert_eq!(requests.lock().unwrap().len(), 1 + NO_DELAYS.len());
    }

    #[tokio::test]
    async fn does_not_retry_client_errors() {
        let (calendar, requests) = calendar_answering(&[404]).await;
        let url = calendar.object_url("1234567@tum.de").unwrap();

        // Gone - which both of them are fine with
        assert!(calendar.fetch_object(&url).await.unwrap().is_none());
        calendar.delete_object(&url, None).await.unwrap();

        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn keeps_handling_conflicts_after_a_retry() {
        let (calendar, requests) = calendar_answering(&[500, 412]).await;
        let url = calendar.object_url("1234567@tum.de").unwrap();

        // The first attempt was stored despite the error, so the retry finds the event
        assert!(!calendar
            .create_object(&url, "BEGIN:VCALENDAR")
            .await
            .unwrap());
        assert_eq!(requests.lock().unwrap().len(), 2);

        let error = calendar
            .put_object(&url, "BEGIN:VCALENDAR", Some("\"etag\""))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed on the server"));
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    const UID: &str = "1234567@tum.de";

    fn feed_event(summary: &str) -> IcalEvent {
        let mut event = tum_event();
        event.properties.retain(|p| p.name != "SUMMARY");
        event.properties.push(property("SUMMARY", summary));
        event
    }

    fn updated(from: &str, to: &str) -> CalendarEvent {
        CalendarEvent::Updated {
            event: EventData {
                uid: String::from(UID),
                ical_data: feed_event(to),
            },
            changed_properties: vec![PropertyChange {
                key: String::from("SUMMARY"),
                from: Some(property("SUMMARY", from)),
                to: Some(property("SUMMARY", to)),
            }],
        }
    }

    fn stored_lecture() -> String {
        String::from(
            "BEGIN:VCALENDAR\r\n\
             BEGIN:VEVENT\r\n\
             UID:1234567@tum.de\r\n\
             DTSTART:20250114T100000Z\r\n\
             DTEND:20250114T120000Z\r\n\
             SUMMARY:Analysis 1\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR\r\n",
        )
    }

    /// The request for the event in `calendar`, e.g. `PUT /calendars/tum/…`
    fn request(method: &str, calendar: &str) -> String {
        format!("{method} /calendars/{calendar}/{}", object_name(UID))
    }

    #[test]
    fn finds_calendars_by_name() {
        let calendar = |name: &str, path: &str, supports_events| CalendarInfo {
            name: Some(String::from(name)),
            url: format!("https://caldav.example.com/1/calendars/{path}/"),
            supports_events,
        };
        let calendars = vec![
            calendar("TUM", "tum", true),
            calendar("TUM Prüfungen", "exams", true),
            calendar("Erinnerungen", "tasks", false),
        ];

        assert_eq!(
            find_calendar(&calendars, "TUM Prüfungen").unwrap().as_str(),
            "https://caldav.example.com/1/calendars/exams/"
        );
        let error = find_calendar(&calendars, "Erinnerungen").unwrap_err();
        assert!(error.to_string().contains("TUM, TUM Prüfungen"));
    }

    #[tokio::test]
    async fn puts_new_exams_into_the_exam_calendar() {
        let (server_url, requests) = test_server(|_| (201, String::new())).await;
        let calendar = calendar_with_exams_on(&server_url);

        for summary in ["Analysis 1 Prüfung", "Analysis 1"] {
            let created = CalendarEvent::Created(EventData {
                uid: String::from(UID),
                ical_data: feed_event(summary),
            });
            tum_apple_sync(&calendar, None, None, vec![created])
                .await
                .unwrap();
        }

        assert_eq!(
            *requests.lock().unwrap(),
            vec![request("PUT", "exams"), request("PUT", "tum")]
        );
    }

    #[tokio::test]
    async fn updates_events_where_they_are() {
        // The lecture was moved into the exam calendar by hand
        let stored_at = request("GET", "exams");
        let (server_url, requests) = test_server(move |request| {
            if request == stored_at {
                (200, stored_lecture())
            } else if request.starts_with("GET") {
                (404, String::new())
            } else {
                (204, String::new())
            }
        })
        .await;
        let calendar = calendar_with_exams_on(&server_url);

        tum_apple_sync(
            &calendar,
            None,
            None,
            vec![updated("Analysis 1 alt", "Analysis 1")],
        )
        .await
        .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                request("GET", "tum"),
                request("GET", "exams"),
                request("PUT", "exams")
            ]
        );
    }

    #[tokio::test]
    async fn moves_events_that_became_an_exam() {
        let stored_at = request("GET", "tum");
        let (server_url, requests) = test_server(move |request| {
            if request == stored_at {
                (200, stored_lecture())
            } else if request.starts_with("GET") {
                (404, String::new())
            } else {
                (204, String::new())
            }
        })
        .await;
        let calendar = calendar_with_exams_on(&server_url);

        tum_apple_sync(
            &calendar,
            None,
            None,
            vec![updated("Analysis 1", "Analysis 1 Prüfung")],
        )
        .await
        .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                request("GET", "exams"),
                request("GET", "tum"),
                request("PUT", "exams"),
                request("DELETE", "tum")
            ]
        );
    }

    #[tokio::test]
    async fn deletes_events_from_both_calendars() {
        let (server_url, requests) = test_server(|_| (404, String::new())).await;
        let calendar = calendar_with_exams_on(&server_url);

        // Recent enough to actually get deleted
        let mut event = feed_event("Analysis 1");
        event.properties.retain(|p| p.name != "DTEND");
        event
            .properties
            .push(property("DTEND", &format_utc(Utc::now())));
        let deleted = CalendarEvent::Deleted(EventData {
            uid: String::from(UID),
            ical_data: event,
        });
        tum_apple_sync(&calendar, None, None, vec![deleted])
            .await
            .unwrap();

        assert_eq!(
            *requests.lock().unwrap(),
            vec![request("DELETE", "tum"), request("DELETE", "exams")]
        );
    }

    #[tokio::test]
    async fn moves_existing_exams_over() {
        let (server_url, requests) = test_server(|request| match request {
            "PROPFIND /calendars/tum/" => (
                207,
                String::from(
                    "<multistatus xmlns=\"DAV:\">\
                     <response><href>/calendars/tum/</href></response>\
                     <response><href>/calendars/tum/exam.ics</href></response>\
                     <response><href>/calendars/tum/lecture.ics</href></response>\
                     </multistatus>",
                ),
            ),
            "GET /calendars/tum/exam.ics" => (
                200,
                stored_lecture().replace("SUMMARY:Analysis 1", "SUMMARY:Analysis 1 Prüfung"),
            ),
            "GET /calendars/tum/lecture.ics" => (200, stored_lecture()),
            _ => (204, String::new()),
        })
        .await;
        let calendar = calendar_with_exams_on(&server_url);

        crate::move_exams_to_exam_calendar(&calendar, true)
            .await
            .unwrap();

        let mut requests = requests.lock().unwrap().clone();
        requests.sort();
        assert_eq!(
            requests,
            vec![
                "DELETE /calendars/tum/exam.ics",
                "GET /calendars/tum/exam.ics",
                "GET /calendars/tum/lecture.ics",
                "PROPFIND /calendars/tum/",
                "PUT /calendars/exams/exam.ics",
            ]
        );
    }
}
