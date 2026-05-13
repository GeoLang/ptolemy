// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Replication endpoints — change feed and peer management for distributed sync.
//!
//! Provides an ordered change log that replicas can consume to stay in sync.
//! Supports both push and pull replication modes.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ptolemy_storage::{ChangeFeedEntry, ReplicationPeer};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;

pub fn replication_routes() -> Router<AppState> {
    Router::new()
        // Change feed
        .route("/replication/feed/{branch_id}", get(get_feed))
        // Peer management
        .route("/replication/peers", get(list_peers).post(register_peer))
        .route("/replication/peers/{id}/sync", post(ack_sync))
}

// ─── Change Feed ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FeedParams {
    /// Sequence ID to start from (exclusive). Default: 0 (all changes)
    #[serde(default)]
    since: i64,
    /// Max entries to return. Default: 100
    #[serde(default = "default_feed_limit")]
    limit: i64,
}

fn default_feed_limit() -> i64 {
    100
}

async fn get_feed(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(params): Query<FeedParams>,
) -> Result<Json<FeedResponse>, ReplicationError> {
    let limit = params.limit.clamp(1, 10000);
    let entries = store
        .get_change_feed(branch_id, params.since, limit)
        .await?;

    let next_sequence = entries.last().map(|e| e.sequence_id);

    Ok(Json(FeedResponse {
        branch_id,
        since: params.since,
        entries,
        next_sequence,
    }))
}

#[derive(serde::Serialize)]
struct FeedResponse {
    branch_id: Uuid,
    since: i64,
    entries: Vec<ChangeFeedEntry>,
    next_sequence: Option<i64>,
}

// ─── Peer Management ────────────────────────────────────────────────

async fn list_peers(
    State(store): State<AppState>,
) -> Result<Json<Vec<ReplicationPeer>>, ReplicationError> {
    let peers = store.list_peers().await?;
    Ok(Json(peers))
}

#[derive(Deserialize)]
struct RegisterPeerRequest {
    name: String,
    #[serde(default)]
    endpoint_url: Option<String>,
    #[serde(default = "default_direction")]
    direction: String,
}

fn default_direction() -> String {
    "bidirectional".into()
}

async fn register_peer(
    State(store): State<AppState>,
    Json(req): Json<RegisterPeerRequest>,
) -> Result<(StatusCode, Json<ReplicationPeer>), ReplicationError> {
    let now = OffsetDateTime::now_utc();
    let peer = ReplicationPeer {
        id: Uuid::now_v7(),
        name: req.name,
        endpoint_url: req.endpoint_url,
        last_sync_changeset: None,
        last_sync_at: None,
        direction: req.direction,
        status: "active".into(),
        created_at: now,
    };

    store.register_peer(&peer).await?;
    Ok((StatusCode::CREATED, Json(peer)))
}

#[derive(Deserialize)]
struct AckSyncRequest {
    changeset_id: Uuid,
}

async fn ack_sync(
    State(store): State<AppState>,
    Path(peer_id): Path<Uuid>,
    Json(req): Json<AckSyncRequest>,
) -> Result<StatusCode, ReplicationError> {
    store.update_peer_sync(peer_id, req.changeset_id).await?;
    Ok(StatusCode::OK)
}

// ─── Error type ─────────────────────────────────────────────────────

#[derive(Debug)]
enum ReplicationError {
    Store(ptolemy_storage::StoreError),
}

impl From<ptolemy_storage::StoreError> for ReplicationError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        Self::Store(e)
    }
}

impl IntoResponse for ReplicationError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg).into_response()
            }
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
}
