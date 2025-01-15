# ICS Watcher

A Rust library that watches ICS calendar files. You give ICS Watcher a URL pointing to an .ics calendar file and it will poll for changes at regular intervals. When changes are detected, your callback functions get called with details about what changed.

## Examples

- **Log all events**: pass `log_events` as one of the callbacks
- **TUM to Google Calendar Proxy**: pass `tum_google_sync` as one of the callbacks
  - This is already implemented in `main.rs` which means, you can create a `.env` with your `TUM_URL` and `GOOGLE_CALENDAR_ID`, put your Google Calendar API client secret in `.secrets/client_secret.json` and start syncing :)
  - Unlike https://github.com/TUM-Dev/CalendarProxy/, events in this implementation can be modified (which is the main reason for creating this crate)

## TODO's

- Clean up error handling
- **TUM Sync**
  - Refactor TUM Sync creation and deletion of events
  - Introduce reminders for exams
- Fix the examples in the docs (they work, they just don't pass the docs tests because they're async)

## License

Licensed under either of:

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

The TUM Google Sync can also function as a summary shortener using the `replacements.json`. Due to licensing restrictions, I do not distribute it myself, but you can find a good `replacements.json` here: https://github.com/TUM-Dev/CalendarProxy.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
