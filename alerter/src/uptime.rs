use crate::error::Error;
use humantime::Duration;
use log::{debug, error, info};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Response {
    pub ok: bool,
}

/// Push alerter status to uptimekuma.
/// This simply calls the urls with ping time and status as up.
pub(crate) async fn push_uptime_status(url: String, every: Duration) -> Result<(), Error> {
    info!("Starting uptimekuma health push for every {every} ...");
    let mut tick = tokio::time::interval(every.into());
    let client = Client::builder()
        .build()
        .expect("must be able to create a client");
    loop {
        tick.tick().await;

        debug!("Pushing uptime status...");

        let formatted_url = format!("{url}?status=up&msg=Ok&ping={}", every.as_millis());
        let res = client.get(&formatted_url).send().await?;
        if res.status().is_success() {
            debug!("Pushed uptime status.")
        } else {
            error!("Failed to push uptime status: {}", res.status());
        }
    }
}
