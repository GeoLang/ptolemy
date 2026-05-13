// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Feature locking API for pessimistic concurrency control.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
};
use ptolemy_storage::FeatureLock;
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

pub fn lock_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/locks", get(list_locks).post(lock_feature))
        .route(
            "/branches/{branch_id}/locks/{feature_id}",
            delete(unlock_feature),
        )
}

async fn list_locks(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<Vec<FeatureLock>>, LockError> {
    let locks = store.list_locks(branch_id).await?;
    Ok(Json(locks))
}

#[derive(Deserialize)]
struct LockRequest {
    feature_id: Uuid,
    locked_by: String,
    #[serde(default = "default_duration")]
    duration_minutes: i64,
    reason: Option<String>,
}

fn default_duration() -> i64 {
    60
}

async fn lock_feature(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<LockRequest>,
) -> Result<StatusCode, LockError> {
    store
        .lock_feature(
            req.feature_id,
            branch_id,
            &req.locked_by,
            req.duration_minutes,
            req.reason.as_deref(),
        )
        .await?;
    Ok(StatusCode::CREATED)
}

async fn unlock_feature(
    State(store): State<AppState>,
    Path((branch_id, feature_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, LockError> {
    // In production, actor would come from auth token
    store
        .unlock_feature(feature_id, branch_id, "system")
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Error Handling ─────────────────────────────────────────────────

enum LockError {
    Store(ptolemy_storage::StoreError),
}

impl From<ptolemy_storage::StoreError> for LockError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        LockError::Store(e)
    }
}

impl IntoResponse for LockError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            LockError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            LockError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            LockError::Store(ptolemy_storage::StoreError::Db(e)) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
