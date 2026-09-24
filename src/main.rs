use dotenv::dotenv;
use ics_watcher::{
    migrate_google_to_apple, tum_apple_sync, tum_google_sync, AppleCalendar, CalendarCallback,
    ICSWatcher, MigrationOptions,
};
use std::{env, process};

/// The name the watcher saves its state under
const BACKUP_NAME: &str = "TUM Calendar";

const USAGE: &str = "\
Usage:
  ics-watcher                     Sync the TUM calendar to the calendars configured in .env
  ics-watcher migrate [options]   Copy the Google calendar the sync kept over to the Apple Calendar

Options for migrate:
  --apply           Write to the Apple Calendar - without it, the migration only reports what it would do
  --overwrite       Replace events which already exist in the Apple Calendar instead of skipping them
  --dump <folder>   Additionally save every event as an .ics file into <folder>";

#[tokio::main]
async fn main() {
    dotenv().ok();

    let arguments: Vec<String> = env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        None => watch().await,
        Some("migrate") => migrate(&arguments[1..]).await,
        Some(_) => {
            eprintln!("{USAGE}");
            process::exit(2);
        }
    }
}

async fn watch() {
    let tum_url = env::var("TUM_URL").expect("TUM_URL not found in environment");

    let mut callbacks: Vec<CalendarCallback> = Vec::new();

    if let Ok(google_calendar_id) = env::var("GOOGLE_CALENDAR_ID") {
        callbacks.push(Box::new(move |a, b, e| {
            let calendar_id = google_calendar_id.clone();
            Box::pin(async move { tum_google_sync(&calendar_id, a, b, e).await })
        }));
    }

    if let Some(apple_calendar) = apple_calendar().await {
        callbacks.push(Box::new(move |a, b, e| {
            let calendar = apple_calendar.clone();
            Box::pin(async move { tum_apple_sync(&calendar, a, b, e).await })
        }));
    }

    if callbacks.is_empty() {
        panic!("No calendar configured - set GOOGLE_CALENDAR_ID and / or APPLE_ID in environment");
    }

    // callbacks.push(Box::new(|a, b, e| Box::pin(async move { log_events(a, b, e).await })));

    let mut ics_watcher = ICSWatcher::new(tum_url.as_str(), callbacks);

    // Try to load backup
    let _ = ics_watcher.load_backup(BACKUP_NAME);
    ics_watcher
        .run(Option::from(BACKUP_NAME))
        .await
        .expect("ICS Watcher crashed");
}

/// The Apple Calendar configured in the environment, if there is one
async fn apple_calendar() -> Option<AppleCalendar> {
    let apple_id = env::var("APPLE_ID").ok()?;
    let app_password =
        env::var("APPLE_APP_PASSWORD").expect("APPLE_APP_PASSWORD not found in environment");

    // Either address the calendar directly or look it up by its name
    let apple_calendar = match env::var("APPLE_CALENDAR_URL") {
        Ok(calendar_url) => {
            AppleCalendar::from_calendar_url(&apple_id, &app_password, &calendar_url)
        }
        Err(_) => {
            let calendar_name = env::var("APPLE_CALENDAR_NAME")
                .expect("APPLE_CALENDAR_NAME not found in environment");
            AppleCalendar::connect(&apple_id, &app_password, &calendar_name).await
        }
    }
    .expect("Failed to connect to the Apple Calendar");

    Some(apple_calendar)
}

async fn migrate(arguments: &[String]) {
    let mut options = MigrationOptions {
        backup_name: String::from(BACKUP_NAME),
        ..Default::default()
    };

    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), arguments.clone().next()) {
            ("--apply", _) => options.apply = true,
            ("--overwrite", _) => options.overwrite = true,
            ("--dump", Some(folder)) => {
                options.dump_directory = Some(folder.into());
                arguments.next();
            }
            _ => {
                eprintln!("{USAGE}");
                process::exit(2);
            }
        }
    }

    let google_calendar_id =
        env::var("GOOGLE_CALENDAR_ID").expect("GOOGLE_CALENDAR_ID not found in environment");
    let apple_calendar = apple_calendar().await;
    if options.apply && apple_calendar.is_none() {
        panic!("APPLE_ID not found in environment - it's needed to write to the Apple Calendar");
    }

    if let Err(error) =
        migrate_google_to_apple(&google_calendar_id, apple_calendar.as_ref(), &options).await
    {
        eprintln!("Migration failed: {error}");
        process::exit(1);
    }
}
