// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Offline sync protocol endpoints.
//!
//! The sync protocol allows field clients (QGIS, mobile apps) to:
//! 1. Pull a snapshot of a branch at a specific changeset (GET /sync/pull)
//! 2. Push local edits as a changeset (POST /sync/push)
//! 3. Check if their local snapshot is behind (GET /sync/status)
//!
//! This enables offline-first workflows where edits happen without connectivity
//! and are reconciled later.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;

pub fn sync_routes() -> Router<AppState> {
    Router::new()
        .route("/sync/pull", get(sync_pull))
        .route("/sync/push", post(sync_push))
        .route("/sync/status", get(sync_status))
}

// ─── Pull ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PullParams {
    branch_id: Uuid,
    /// If provided, only return changes since this changeset (incremental sync).
    /// If omitted, return full snapshot.
    #[serde(default)]
    since_changeset: Option<Uuid>,
}

#[derive(Serialize)]
struct PullResponse {
    branch_id: Uuid,
    head_changeset: Option<Uuid>,
    /// Full feature snapshot (when since_changeset is None)
    #[serde(skip_serializing_if = "Option::is_none")]
    features: Option<Vec<SyncFeature>>,
    /// Incremental diff operations (when since_changeset is Some)
    #[serde(skip_serializing_if = "Option::is_none")]
    operations: Option<Vec<SyncDiffOp>>,
}

#[derive(Serialize)]
struct SyncFeature {
    id: Uuid,
    geometry_wkb_hex: String,
    properties: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SyncDiffOp {
    Insert {
        feature_id: Uuid,
        geometry_wkb_hex: String,
        properties: serde_json::Value,
    },
    Update {
        feature_id: Uuid,
        #[serde(skip_serializing_if = "Option::is_none")]
        geometry_wkb_hex: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        properties: Option<serde_json::Value>,
    },
    Delete {
        feature_id: Uuid,
    },
}

async fn sync_pull(
    State(store): State<AppState>,
    Query(params): Query<PullParams>,
) -> Result<Json<PullResponse>, SyncError> {
    let branch = store.get_branch(params.branch_id).await?;

    if let Some(since) = params.since_changeset {
        // Incremental pull: diff from `since` to current head
        let head = branch
            .head
            .ok_or_else(|| SyncError::BadRequest("branch has no commits".into()))?;

        if since == head {
            // Already up to date
            return Ok(Json(PullResponse {
                branch_id: params.branch_id,
                head_changeset: branch.head,
                features: None,
                operations: Some(vec![]),
            }));
        }

        let diff = store.diff(Some(since), head).await?;
        let ops: Vec<SyncDiffOp> = diff
            .operations
            .into_iter()
            .map(|op| match op {
                ptolemy_core::diff::DiffOp::Insert {
                    feature_id,
                    geometry_wkb,
                    properties,
                } => SyncDiffOp::Insert {
                    feature_id,
                    geometry_wkb_hex: hex::encode(&geometry_wkb),
                    properties,
                },
                ptolemy_core::diff::DiffOp::Update {
                    feature_id,
                    geometry_wkb,
                    properties,
                } => SyncDiffOp::Update {
                    feature_id,
                    geometry_wkb_hex: geometry_wkb.map(|w| hex::encode(&w)),
                    properties,
                },
                ptolemy_core::diff::DiffOp::Delete { feature_id } => {
                    SyncDiffOp::Delete { feature_id }
                }
            })
            .collect();

        Ok(Json(PullResponse {
            branch_id: params.branch_id,
            head_changeset: branch.head,
            features: None,
            operations: Some(ops),
        }))
    } else {
        // Full snapshot
        let features = store.list_features_at_head(params.branch_id).await?;
        let sync_features: Vec<SyncFeature> = features
            .into_iter()
            .map(|f| SyncFeature {
                id: f.id,
                geometry_wkb_hex: hex::encode(&f.geometry_wkb),
                properties: f.properties,
            })
            .collect();

        Ok(Json(PullResponse {
            branch_id: params.branch_id,
            head_changeset: branch.head,
            features: Some(sync_features),
            operations: None,
        }))
    }
}

// ─── Push ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PushRequest {
    branch_id: Uuid,
    /// The changeset the client was synced to when making these edits.
    /// Used to detect if the branch has moved ahead (requires merge).
    base_changeset: Option<Uuid>,
    message: String,
    author: String,
    operations: Vec<SyncDiffOp>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PushResponse {
    Success {
        changeset_id: Uuid,
    },
    BehindHead {
        /// Client needs to pull & merge first
        current_head: Uuid,
        client_base: Option<Uuid>,
    },
}

async fn sync_push(
    State(store): State<AppState>,
    Json(req): Json<PushRequest>,
) -> Result<(StatusCode, Json<PushResponse>), SyncError> {
    let branch = store.get_branch(req.branch_id).await?;

    // Check if client is behind
    if let (Some(base), Some(head)) = (req.base_changeset, branch.head)
        && base != head
    {
        return Ok((
            StatusCode::CONFLICT,
            Json(PushResponse::BehindHead {
                current_head: head,
                client_base: req.base_changeset,
            }),
        ));
    }

    // Convert sync ops to DiffOps
    let ops: Result<Vec<ptolemy_core::diff::DiffOp>, SyncError> = req
        .operations
        .into_iter()
        .map(|op| match op {
            SyncDiffOp::Insert {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = hex::decode(&geometry_wkb_hex)
                    .map_err(|e| SyncError::BadRequest(format!("invalid hex: {e}")))?;
                Ok(ptolemy_core::diff::DiffOp::Insert {
                    feature_id,
                    geometry_wkb: wkb,
                    properties,
                })
            }
            SyncDiffOp::Update {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = geometry_wkb_hex
                    .map(|h| hex::decode(&h))
                    .transpose()
                    .map_err(|e| SyncError::BadRequest(format!("invalid hex: {e}")))?;
                Ok(ptolemy_core::diff::DiffOp::Update {
                    feature_id,
                    geometry_wkb: wkb,
                    properties,
                })
            }
            SyncDiffOp::Delete { feature_id } => {
                Ok(ptolemy_core::diff::DiffOp::Delete { feature_id })
            }
        })
        .collect();

    let changeset = store
        .commit(req.branch_id, &req.message, &req.author, &ops?)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(PushResponse::Success {
            changeset_id: changeset.id,
        }),
    ))
}

// ─── Status ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StatusParams {
    branch_id: Uuid,
    /// Client's current changeset
    #[serde(default)]
    local_head: Option<Uuid>,
}

#[derive(Serialize)]
struct SyncStatus {
    branch_id: Uuid,
    remote_head: Option<Uuid>,
    is_behind: bool,
    changesets_behind: i64,
}

async fn sync_status(
    State(store): State<AppState>,
    Query(params): Query<StatusParams>,
) -> Result<Json<SyncStatus>, SyncError> {
    let branch = store.get_branch(params.branch_id).await?;

    let (is_behind, changesets_behind) = match (params.local_head, branch.head) {
        (Some(local), Some(remote)) if local == remote => (false, 0),
        (Some(local), Some(_remote)) => {
            // Count how many changesets are between local and remote
            let history = store.get_branch_history(params.branch_id, 1000).await?;
            let behind = history
                .iter()
                .position(|cs| cs.id == local)
                .unwrap_or(history.len());
            (true, behind as i64)
        }
        (Some(_), None) => (false, 0),
        (None, Some(_)) => (true, -1), // unknown distance
        (None, None) => (false, 0),
    };

    Ok(Json(SyncStatus {
        branch_id: params.branch_id,
        remote_head: branch.head,
        is_behind,
        changesets_behind,
    }))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum SyncError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl From<ptolemy_storage::StoreError> for SyncError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        SyncError::Store(e)
    }
}

impl IntoResponse for SyncError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            SyncError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            SyncError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            SyncError::Store(ptolemy_storage::StoreError::Db(e)) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            SyncError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
