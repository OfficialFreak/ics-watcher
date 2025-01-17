use dotenv::dotenv;
use ics_watcher::{log_events, tum_google_sync, ICSWatcher};
use std::env;

#[tokio::main]
async fn main() {
    dotenv().ok();

    let tum_url = env::var("TUM_URL").expect("TUM_URL not found in environment");
    let google_calendar_id =
        env::var("GOOGLE_CALENDAR_ID").expect("GOOGLE_CALENDAR_ID not found in environment");

    let mut ics_watcher = ICSWatcher::new(
        tum_url.as_str(),
        vec![
            // Box::new(|a, b, e| Box::pin(async move { log_events(a, b, e).await })),
            Box::new(move |a, b, e| {
                let calendar_id = google_calendar_id.clone();
                Box::pin(async move { tum_google_sync(&calendar_id, a, b, e).await })
            }),
        ],
    );

    // Try to load backup
    let _ = ics_watcher.load_backup("TUM Calendar");
    ics_watcher
        .run(Option::from("TUM Calendar"))
        .await
        .expect("ICS Watcher crashed");
}
