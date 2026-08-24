// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Webhook subscriptions and the CDC event log they are fed from.
//!
//! A subscription names a url the server will post dataset content to, so
//! `auth::classify` keeps every method here admin-only: it hands out access to
//! data and there is no per-dataset owner to delegate that to. The writes take a
//! [`WriteGrant`] on top of that, so the target is the dataset the ladder checked
//! rather than whatever the body says.
//!
//! What actually delivers is [`crate::delivery`]. Emission is in the store, in
//! the transaction that made the change.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
};
use ptolemy_core::event::{Event, EventType, Webhook};
use ptolemy_storage::{WebhookInput, WriteGrant};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

pub fn webhook_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/webhooks",
            get(list_webhooks).post(create_webhook),
        )
        .route("/webhooks/{id}", delete(delete_webhook))
        .route("/datasets/{id}/events", get(list_events).post(emit_event))
}

// ─── Webhooks ───────────────────────────────────────────────────────

async fn list_webhooks(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Webhook>>, WebhookError> {
    let hooks = store.list_webhooks(dataset_id).await?;
    Ok(Json(hooks))
}

#[derive(Deserialize)]
struct CreateWebhookRequest {
    url: String,
    #[serde(default)]
    events: Vec<String>,
    secret: Option<String>,
}

/// The worker posts to whatever this url names, so a scheme it cannot post to is
/// a subscription that can only ever fail, and a scheme like `file:` is a
/// request to read the server's disk. Only http and https get in.
fn is_deliverable_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

async fn create_webhook(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<Webhook>), WebhookError> {
    if !is_deliverable_url(&req.url) {
        return Err(WebhookError::BadUrl);
    }
    let wh = store
        .create_webhook(
            &grant,
            &WebhookInput {
                url: &req.url,
                events: &req.events,
                secret: req.secret.as_deref(),
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(wh)))
}

async fn delete_webhook(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
) -> Result<StatusCode, WebhookError> {
    store.delete_webhook(&grant).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Events ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

async fn list_events(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Query(q): Query<EventQuery>,
) -> Result<Json<Vec<Event>>, WebhookError> {
    let events = store.list_events(dataset_id, q.limit).await?;
    Ok(Json(events))
}

#[derive(Deserialize)]
struct EmitEventRequest {
    event_type: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// The types the store raises itself. A caller emitting one would put a
/// `commit` a subscriber cannot tell from a real one on the wire, and the
/// signature proves only that the event came from this server.
fn is_reserved_event_type(event_type: &str) -> bool {
    EventType::ALL
        .iter()
        .any(|reserved| reserved.to_string() == event_type)
}

async fn emit_event(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<EmitEventRequest>,
) -> Result<(StatusCode, Json<Event>), WebhookError> {
    if is_reserved_event_type(&req.event_type) {
        return Err(WebhookError::ReservedEventType);
    }
    let event = store
        .emit_event(&grant, &req.event_type, &req.payload)
        .await?;
    Ok((StatusCode::CREATED, Json(event)))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum WebhookError {
    Store(ptolemy_storage::StoreError),
    BadUrl,
    ReservedEventType,
}

impl From<ptolemy_storage::StoreError> for WebhookError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        WebhookError::Store(e)
    }
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            WebhookError::Store(e) => crate::errors::store_error_status(&e),
            WebhookError::BadUrl => (
                StatusCode::BAD_REQUEST,
                "webhook url must be http or https".to_string(),
            ),
            WebhookError::ReservedEventType => (
                StatusCode::BAD_REQUEST,
                "event_type is one the server raises itself".to_string(),
            ),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_own_event_types_cannot_be_emitted_by_a_caller() {
        for reserved in EventType::ALL {
            assert!(
                is_reserved_event_type(&reserved.to_string()),
                "{reserved:?}"
            );
        }
        for allowed in ["sweep", "inspection_due", "Commit", "commit "] {
            assert!(!is_reserved_event_type(allowed), "{allowed}");
        }
    }

    #[test]
    fn only_http_urls_are_deliverable() {
        assert!(is_deliverable_url("https://example.test/hook"));
        assert!(is_deliverable_url("http://127.0.0.1:9000/hook"));
        for url in [
            "file:///etc/passwd",
            "gopher://example.test/",
            "ftp://example.test/",
            "example.test/hook",
            "",
            "sweep",
        ] {
            assert!(!is_deliverable_url(url), "{url}");
        }
    }
}
