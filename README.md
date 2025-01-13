# ICS Watcher

A Rust library that watches ICS calendar files. You give ICS Watcher a URL pointing to an .ics calendar file and it will poll for changes at regular intervals. When changes are detected, your callback functions get called with details about what changed.

## Examples

- **Log all events**: pass `log_events` as one of the callbacks
- **TUM to Google Calendar Proxy**: pass `tum_google_sync` as one of the callbacks
  - Unlike https://github.com/TUM-Dev/CalendarProxy/, events in this implementation can be modified (which is the main reason for creating this crate)
  - `replacements.json` taken from https://github.com/TUM-Dev/CalendarProxy/ and modified

## TODO's
- Clean up error handling
- **TUM Sync**
  - Refactor TUM Sync creation and deletion of events
  - Introduce reminders for exams
- Fix the examples in the docs (they work, they just don't pass the docs tests because they're async)
- Publishing the crate to crates.io

## Contributing
Issues and Pull Requests are welcome :)
