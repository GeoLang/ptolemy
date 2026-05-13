// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Webhook and CDC event stream API endpoints.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
};
use ptolemy_core::event::{Event, Webhook};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

pub fn webhook_routes() -> Router<AppState> {
    Router::new()
        .route("/datasets/{id}/webhooks", get(list_webhooks).post(create_webhook))
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

async fn create_webhook(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<Webhook>), WebhookError> {
    let wh = Webhook {
        id: Uuid::now_v7(),
        dataset_id,
        url: req.url,
        events: req.events,
        secret: req.secret,
        active: true,
    };
    store.create_webhook(&wh).await?;
    Ok((StatusCode::CREATED, Json(wh)))
}

async fn delete_webhook(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, WebhookError> {
    store.delete_webhook(id).await?;
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

async fn emit_event(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<EmitEventRequest>,
) -> Result<(StatusCode, Json<Event>), WebhookError> {
    let event = store.emit_event(dataset_id, &req.event_type, &req.payload).await?;
    Ok((StatusCode::CREATED, Json(event)))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum WebhookError {
    Store(ptolemy_storage::StoreError),
}

impl From<ptolemy_storage::StoreError> for WebhookError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        WebhookError::Store(e)
    }
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            WebhookError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            WebhookError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            WebhookError::Store(ptolemy_storage::StoreError::Db(e)) => {
                tracing::error!("Database error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
