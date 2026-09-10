use dotenv::dotenv;
use ics_watcher::{tum_apple_sync, tum_google_sync, AppleCalendar, CalendarCallback, ICSWatcher};
use std::env;

#[tokio::main]
async fn main() {
    dotenv().ok();
    let tum_url = env::var("TUM_URL").expect("TUM_URL not found in environment");

    let mut callbacks: Vec<CalendarCallback> = Vec::new();

    if let Ok(google_calendar_id) = env::var("GOOGLE_CALENDAR_ID") {
        callbacks.push(Box::new(move |a, b, e| {
            let calendar_id = google_calendar_id.clone();
            Box::pin(async move { tum_google_sync(&calendar_id, a, b, e).await })
        }));
    }

    if let Ok(apple_id) = env::var("APPLE_ID") {
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
    let _ = ics_watcher.load_backup("TUM Calendar");
    ics_watcher
        .run(Option::from("TUM Calendar"))
        .await
        .expect("ICS Watcher crashed");
}
