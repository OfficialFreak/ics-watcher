use std::{collections::HashMap, io::BufReader, thread, time::Duration};

use ical::{
    parser::{
        ical::component::{IcalCalendar, IcalEvent},
        Component,
    },
    property::Property,
    IcalParser,
};

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
            '-' => {} // Handle negative durations if needed
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

#[derive(Debug)]
pub struct EventData {
    pub uid: String,
    pub ical_data: IcalEvent,
}

#[derive(Debug)]
pub struct PropertyChange {
    pub key: String,
    pub from: Option<Property>,
    pub to: Option<Property>,
}

#[derive(Debug)]
pub enum CalendarEvent {
    Setup(EventData),
    Created(EventData),
    Updated {
        event: EventData,
        changed_properties: Vec<PropertyChange>,
    },
    Deleted {
        uid: String,
    },
}

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
            let event_uid = event
                .get_property("UID")
                .and_then(|prop| prop.value.clone())
                .expect("Event is missing a UID")
                + event
                    .get_property("RECURRENCE-ID")
                    .and_then(|prop| prop.value.clone())
                    .unwrap_or(String::from(""))
                    .as_str();

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

        for event_uid in self.previous.keys() {
            if !new_previous.contains_key(event_uid) {
                result.push(CalendarEvent::Deleted {
                    uid: event_uid.clone(),
                });
            }
        }

        self.previous = new_previous;
        self.initialized = true;

        result
    }
}

pub struct ICSWatcher<'a> {
    // usize is the n-th calendar inside of the ics file
    // You can ignore it to handle all calendars the same
    ics_link: &'a str,
    pub callbacks: Vec<Box<dyn Fn(&Option<String>, &Option<String>, &Vec<CalendarEvent>)>>,
    change_detector: CalendarChangeDetector,
}

impl<'a> ICSWatcher<'a> {
    pub fn new(
        ics_link: &'a str,
        callbacks: Vec<Box<dyn Fn(&Option<String>, &Option<String>, &Vec<CalendarEvent>)>>,
    ) -> Self {
        ICSWatcher {
            ics_link,
            callbacks,
            change_detector: CalendarChangeDetector::new(),
        }
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let res = reqwest::blocking::get(self.ics_link)?;

            let buf = BufReader::new(res);
            let calendar = IcalParser::new(buf).next().unwrap().unwrap();

            let events = self.change_detector.compare(calendar);

            if !events.is_empty() {
                for callback in &self.callbacks {
                    callback(
                        &self.change_detector.name,
                        &self.change_detector.description,
                        &events,
                    );
                }
            }

            // thread::sleep(self.change_detector.ttl);
            thread::sleep(Duration::from_secs(1));
        }
    }
}

pub fn print_events(
    name: &Option<String>,
    description: &Option<String>,
    events: &Vec<CalendarEvent>,
) {
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
            CalendarEvent::Deleted { uid } => {
                println!("Deleted {uid}\n")
            }
        }
    }
}

// TODO: Add tests
// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn it_works() {
//         let result = add(2, 2);
//         assert_eq!(result, 4);
//     }
// }
