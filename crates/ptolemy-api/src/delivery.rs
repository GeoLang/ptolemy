// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Background webhook delivery engine with HMAC signing and retries.

use hmac::{Hmac, Mac};
use ptolemy_core::event::Webhook;
use reqwest::Client;
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// A webhook delivery job.
#[derive(Debug, Clone)]
pub struct DeliveryJob {
    pub webhook: Webhook,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub event_id: Uuid,
}

/// Sender half for queueing webhook deliveries.
pub type DeliverySender = mpsc::UnboundedSender<DeliveryJob>;

/// Spawn the webhook delivery background worker.
/// Returns a sender to queue jobs.
pub fn spawn_delivery_worker() -> DeliverySender {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(delivery_loop(rx));
    tx
}

async fn delivery_loop(mut rx: mpsc::UnboundedReceiver<DeliveryJob>) {
    let client = Arc::new(Client::new());
    info!("Webhook delivery worker started");

    while let Some(job) = rx.recv().await {
        let client = client.clone();
        tokio::spawn(async move {
            deliver_with_retries(&client, &job, 3).await;
        });
    }
}

async fn deliver_with_retries(client: &Client, job: &DeliveryJob, max_retries: u32) {
    let body = serde_json::json!({
        "event_id": job.event_id.to_string(),
        "event_type": &job.event_type,
        "webhook_id": job.webhook.id.to_string(),
        "payload": &job.payload,
    });

    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

    for attempt in 0..=max_retries {
        let mut request = client
            .post(&job.webhook.url)
            .header("Content-Type", "application/json")
            .header("X-Ptolemy-Event", &job.event_type)
            .header("X-Ptolemy-Delivery", job.event_id.to_string());

        // HMAC signature if secret is configured
        if let Some(secret) = &job.webhook.secret
            && let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes())
        {
            mac.update(&body_bytes);
            let signature = hex::encode(mac.finalize().into_bytes());
            request = request.header("X-Ptolemy-Signature", format!("sha256={signature}"));
        }

        match request.body(body_bytes.clone()).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    webhook_id = %job.webhook.id,
                    event_id = %job.event_id,
                    "Webhook delivered successfully"
                );
                return;
            }
            Ok(resp) => {
                warn!(
                    webhook_id = %job.webhook.id,
                    status = %resp.status(),
                    attempt,
                    "Webhook delivery failed with HTTP error"
                );
            }
            Err(e) => {
                warn!(
                    webhook_id = %job.webhook.id,
                    error = %e,
                    attempt,
                    "Webhook delivery failed"
                );
            }
        }

        if attempt < max_retries {
            // Exponential backoff: 1s, 2s, 4s
            let delay = std::time::Duration::from_secs(1 << attempt);
            tokio::time::sleep(delay).await;
        }
    }

    error!(
        webhook_id = %job.webhook.id,
        event_id = %job.event_id,
        "Webhook delivery exhausted all retries"
    );
}
