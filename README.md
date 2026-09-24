# ICS Watcher

A Rust library that watches ICS calendar files. You give ICS Watcher a URL pointing to an .ics calendar file and it will poll for changes at regular intervals. When changes are detected, your callback functions get called with details about what changed.

## Examples

- **Log all events**: pass `log_events` as one of the callbacks
- **TUM to Google Calendar Proxy**: pass `tum_google_sync` as one of the callbacks
  - This is already implemented in `main.rs` which means, you can create a `.env` with your `TUM_URL` and `GOOGLE_CALENDAR_ID`, put your Google Calendar API client secret in `.secrets/client_secret.json` and start syncing :)
  - Unlike https://github.com/TUM-Dev/CalendarProxy/, events in this implementation can be modified (which is the main reason for creating this crate)
- **TUM to Apple Calendar Proxy**: pass `tum_apple_sync` as one of the callbacks
  - Behaves just like the Google sync, but talks CalDAV to iCloud instead
  - Also implemented in `main.rs`: add `APPLE_ID`, `APPLE_APP_PASSWORD` and `APPLE_CALENDAR_NAME` (the name of the calendar as shown in the Apple Calendar app) to your `.env` and start syncing
    - `APPLE_APP_PASSWORD` has to be an [app-specific password](https://support.apple.com/en-us/102654), your regular Apple ID password won't be accepted
    - If you'd rather skip the calendar discovery, set `APPLE_CALENDAR_URL` to the CalDAV collection instead of `APPLE_CALENDAR_NAME`
    - Not sure how your calendar is named? `list_icloud_calendars` returns all of them
  - `GOOGLE_CALENDAR_ID` and the Apple variables are independent - configure one of them or both to sync to both calendars at once
  - `AppleCalendar::connect_to` works with any other CalDAV server (Fastmail, Nextcloud, Radicale, …) as well

## Moving from Google Calendar to Apple Calendar

Has the Google sync been running for a while? `ics-watcher migrate` copies that calendar over to the Apple Calendar, so that the Apple sync can take over:

- Events created by the Google sync keep their uid, so the Apple sync picks up right where the Google sync left off
- Whatever you changed in Google Calendar comes along (notes, a seat number in the location, reminders, colors), and events you deleted there stay deleted
- Events you added to the calendar yourself are copied as well, recurring ones included
- Nothing is deleted, neither in Google nor in the Apple Calendar, and events that are in the Apple Calendar already are skipped

Configure both calendars in the `.env`, stop the watcher and run the migration in the watcher's folder (it needs `.secrets` and `.backups`):

1. `ics-watcher migrate` shows what would happen without writing anything - `--dump <folder>` additionally saves every event as an `.ics` file to look at
2. `ics-watcher migrate --apply` writes to the Apple Calendar
3. Remove `GOOGLE_CALENDAR_ID` from the `.env` and start the watcher again

## TODO's

- **TUM Sync**
  - Refactor TUM Sync creation and deletion of events
  - Introduce reminders for exams
- Fix the examples in the docs (they work, they just don't pass the docs tests because they're async)

## License

Licensed under either of:

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

The TUM Google / Apple Sync can also function as a summary shortener using the `replacements.json`. Due to licensing restrictions, I do not distribute it myself, but you can find a good `replacements.json` here: https://github.com/TUM-Dev/CalendarProxy.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
