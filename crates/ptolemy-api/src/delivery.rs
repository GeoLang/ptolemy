// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The webhook delivery worker: drains the outbox, signs each body, retries with
//! a backoff and gives up after a bounded number of attempts.
//!
//! The queue is the `webhook_deliveries` table rather than a channel, because a
//! subscriber that was promised an event has to get it across a restart. A row
//! is written in the same transaction as the change that raised the event, so
//! nothing that happened goes unsent and nothing unsent describes something that
//! did not happen.
//!
//! Scheduling lives in the claim: `claim_due_webhook_deliveries` bumps the
//! attempt count and pushes the next attempt out by the backoff before this
//! module makes the request, so a worker that dies mid-delivery costs one
//! attempt. Two workers on the same database cannot take the same row.
//!
//! The worker also retires what it has finished with. Every commit writes an
//! event row and one delivery row per subscription, so without a sweep both
//! tables grow for as long as the instance runs. It happens here rather than in
//! a scheduler of its own because this is the only thing that reads those
//! tables, and it already runs on a timer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use ptolemy_storage::{DueDelivery, EventRetentionDays, PgStore};
use reqwest::Client;
use sha2::Sha256;
use tracing::{info, warn};

type HmacSha256 = Hmac<Sha256>;

/// How many times one event is offered to one subscriber before it is left as a
/// dead letter. Attempts are spaced 1s, 2s, 4s, 8s by `BACKOFF_BASE_SECS`.
pub const MAX_DELIVERY_ATTEMPTS: i32 = 5;

/// First retry gap. Each later one doubles, up to [`BACKOFF_CAP_SECS`].
const BACKOFF_BASE_SECS: f64 = 1.0;
const BACKOFF_CAP_SECS: f64 = 300.0;

/// How long the worker sleeps when it found nothing due.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Deliveries taken per pass. Small enough that one slow batch does not hold the
/// oldest event back for long.
const BATCH: i64 = 50;

/// A receiver that never answers must not hold the batch open.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the worker keeps a settled delivery and the event behind it, unless
/// `PTOLEMY_EVENTS_RETENTION_DAYS` says otherwise. `0` there keeps everything.
const DEFAULT_EVENTS_RETENTION_DAYS: i32 = 30;
const EVENTS_RETENTION_DAYS_VAR: &str = "PTOLEMY_EVENTS_RETENTION_DAYS";

/// How often the worker retires what it is done with. Far apart because nothing
/// waits on it, and a sweep competes with delivery for the same pool.
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Rows one delete statement may take, so no sweep holds a long transaction.
const RETENTION_BATCH: i64 = 1000;

/// Batches one sweep may run per table. The cap is what keeps a first sweep
/// against a long-unswept database from running for minutes: whatever is left
/// over goes on the next pass, an hour later.
const RETENTION_MAX_BATCHES: usize = 50;

/// The retention window `PTOLEMY_EVENTS_RETENTION_DAYS` asks for. `None` turns
/// the sweep off: that is `0`, a negative number, or a value that is not a whole
/// number of days at all, where deleting on a guess is the wrong way to be
/// wrong. Unset takes the default.
///
/// Split from the environment read so it can be tested without mutating the
/// process environment, which edition 2024 makes unsafe.
fn parse_retention_days(raw: Option<&str>) -> Option<EventRetentionDays> {
    let days = match raw {
        None => DEFAULT_EVENTS_RETENTION_DAYS,
        Some(raw) => match raw.trim().parse::<i32>() {
            Ok(days) => days,
            Err(_) => {
                warn!(
                    value = %raw,
                    "{EVENTS_RETENTION_DAYS_VAR} is not a whole number of days, keeping everything"
                );
                0
            }
        },
    };
    EventRetentionDays::new(days)
}

fn retention_from_env() -> Option<EventRetentionDays> {
    parse_retention_days(std::env::var(EVENTS_RETENTION_DAYS_VAR).ok().as_deref())
}

/// The `X-Ptolemy-Signature` value for a body, `None` when the secret cannot key
/// an HMAC. Receivers verify this string, so it is a wire contract.
fn signature_header(secret: &str, body: &[u8]) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    Some(format!(
        "sha256={}",
        hex::encode(mac.finalize().into_bytes())
    ))
}

pub struct DeliveryWorker {
    store: Arc<PgStore>,
    client: Client,
    backoff_base_secs: f64,
}

impl DeliveryWorker {
    pub fn new(store: Arc<PgStore>) -> Self {
        DeliveryWorker {
            store,
            client: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            backoff_base_secs: BACKOFF_BASE_SECS,
        }
    }

    /// Only for a test that has to walk the retry ladder without waiting for it.
    pub fn with_backoff_base_secs(mut self, secs: f64) -> Self {
        self.backoff_base_secs = secs;
        self
    }

    /// Retire what nothing will look at again: settled deliveries, then the
    /// events no undelivered delivery still refers to. Returns the two counts.
    ///
    /// Order matters. A dead letter counts as undelivered and pins its event, so
    /// the deliveries have to go first for the events behind them to come free.
    /// Each table is swept in `RETENTION_BATCH` batches up to
    /// `RETENTION_MAX_BATCHES`, and a short batch means there is no more to take.
    pub async fn sweep_retention(&self, retention: EventRetentionDays) -> (u64, u64) {
        let deliveries = self
            .sweep_batches(|| {
                self.store.delete_settled_webhook_deliveries(
                    retention,
                    MAX_DELIVERY_ATTEMPTS,
                    RETENTION_BATCH,
                )
            })
            .await;
        let events = self
            .sweep_batches(|| {
                self.store
                    .delete_unreferenced_events(retention, RETENTION_BATCH)
            })
            .await;
        if deliveries > 0 || events > 0 {
            info!(
                deliveries,
                events,
                retention_days = retention.days(),
                "retired settled webhook deliveries and their events"
            );
        }
        (deliveries, events)
    }

    /// Run one delete until it stops finding rows, or the batch cap stops it.
    async fn sweep_batches<F, Fut>(&self, delete: F) -> u64
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<u64, ptolemy_storage::StoreError>>,
    {
        let mut total = 0;
        for _ in 0..RETENTION_MAX_BATCHES {
            match delete().await {
                Ok(0) => break,
                Ok(gone) => {
                    total += gone;
                    if gone < RETENTION_BATCH as u64 {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "retention sweep failed");
                    break;
                }
            }
        }
        total
    }

    /// One pass: claim what is due and deliver it. Returns how many were tried,
    /// so a caller can tell a quiet queue from a busy one.
    pub async fn run_once(&self) -> usize {
        let due = match self
            .store
            .claim_due_webhook_deliveries(
                MAX_DELIVERY_ATTEMPTS,
                self.backoff_base_secs,
                BACKOFF_CAP_SECS,
                BATCH,
            )
            .await
        {
            Ok(due) => due,
            Err(e) => {
                warn!(error = %e, "could not claim webhook deliveries");
                return 0;
            }
        };
        let tried = due.len();
        futures::future::join_all(due.iter().map(|d| self.deliver(d))).await;
        tried
    }

    async fn deliver(&self, delivery: &DueDelivery) {
        let body = serde_json::json!({
            "event_id": delivery.event_id.to_string(),
            "event_type": &delivery.event_type,
            "webhook_id": delivery.webhook_id.to_string(),
            "payload": &delivery.payload,
        });
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

        let mut request = self
            .client
            .post(&delivery.url)
            .header("Content-Type", "application/json")
            .header("X-Ptolemy-Event", &delivery.event_type)
            .header("X-Ptolemy-Delivery", delivery.event_id.to_string());
        if let Some(secret) = &delivery.secret
            && let Some(signature) = signature_header(secret, &body_bytes)
        {
            request = request.header("X-Ptolemy-Signature", signature);
        }

        let outcome = match request.body(body_bytes).send().await {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("HTTP {}", response.status())),
            Err(e) => Some(e.to_string()),
        };

        let result = match &outcome {
            None => {
                info!(
                    webhook_id = %delivery.webhook_id,
                    event_id = %delivery.event_id,
                    "webhook delivered"
                );
                self.store
                    .mark_webhook_delivered(delivery.webhook_id, delivery.event_id)
                    .await
            }
            Some(error) => {
                let exhausted = delivery.attempt >= MAX_DELIVERY_ATTEMPTS;
                warn!(
                    webhook_id = %delivery.webhook_id,
                    event_id = %delivery.event_id,
                    attempt = delivery.attempt,
                    exhausted,
                    error = %error,
                    "webhook delivery failed"
                );
                self.store
                    .record_webhook_delivery_failure(delivery.webhook_id, delivery.event_id, error)
                    .await
            }
        };
        if let Err(e) = result {
            warn!(error = %e, "could not record webhook delivery outcome");
        }
    }
}

/// Start the worker. The server holds the handle only to keep the task alive for
/// as long as it serves.
pub fn spawn_delivery_worker(store: Arc<PgStore>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let worker = DeliveryWorker::new(store);
        let retention = retention_from_env();
        match retention {
            Some(days) => info!(
                retention_days = days.days(),
                "webhook delivery worker started"
            ),
            None => {
                info!("webhook delivery worker started, events and deliveries kept indefinitely")
            }
        }
        // never swept, so the first pass sweeps: a server that was down for a
        // week has a backlog waiting, and there is no reason to sit on it
        let mut last_sweep: Option<Instant> = None;
        loop {
            if let Some(retention) = retention
                && last_sweep.is_none_or(|at| at.elapsed() >= RETENTION_SWEEP_INTERVAL)
            {
                worker.sweep_retention(retention).await;
                last_sweep = Some(Instant::now());
            }
            if worker.run_once().await == 0 {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `0` is how an operator turns the sweep off, and nothing but a positive
    /// whole number may switch it on.
    #[test]
    fn only_a_positive_day_count_turns_the_sweep_on() {
        assert_eq!(parse_retention_days(Some("0")), None);
        assert_eq!(parse_retention_days(Some("-7")), None);
        // a value nobody meant must not be read as "delete after 0 days"
        for junk in ["", "  ", "thirty", "7.5", "30d"] {
            assert_eq!(parse_retention_days(Some(junk)), None, "{junk}");
        }
        assert_eq!(parse_retention_days(Some("7")).map(|r| r.days()), Some(7));
        assert_eq!(
            parse_retention_days(Some(" 90 ")).map(|r| r.days()),
            Some(90)
        );
        assert_eq!(
            parse_retention_days(None).map(|r| r.days()),
            Some(DEFAULT_EVENTS_RETENTION_DAYS)
        );
    }

    // this digest holds a byte below 0x10, so a dropped zero pad fails here
    #[test]
    fn signature_header_golden() {
        assert_eq!(
            signature_header("ptolemy webhook golden secret", br#"{"event":"golden"}"#),
            Some(
                "sha256=0eb8a80f1ebdca22ca35b7d6d1687a05a0f03dda1db87ddd7788142b730a4286"
                    .to_string()
            )
        );
    }
}
