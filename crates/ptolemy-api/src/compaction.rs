// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Version compaction / garbage collection endpoints.
//!
//! Prunes old feature versions to reclaim storage space while preserving
//! the most recent N versions per feature.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ptolemy_storage::{CompactionResult, CompactionRun};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

pub fn compaction_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/compact", post(compact_branch))
        .route("/datasets/{id}/compaction-history", get(compaction_history))
}

#[derive(Deserialize)]
struct CompactRequest {
    /// Number of most recent versions to keep per feature. Default: 1.
    #[serde(default = "default_keep")]
    keep_latest: i32,
}

fn default_keep() -> i32 {
    1
}

async fn compact_branch(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<CompactRequest>,
) -> Result<Json<CompactionResult>, CompactionError> {
    let keep = req.keep_latest.max(1); // never keep less than 1
    let result = store.compact_versions(branch_id, keep).await?;
    Ok(Json(result))
}

async fn compaction_history(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<CompactionRun>>, CompactionError> {
    let runs = store.list_compaction_runs(dataset_id).await?;
    Ok(Json(runs))
}

// ─── Error type ─────────────────────────────────────────────────────

#[derive(Debug)]
enum CompactionError {
    Store(ptolemy_storage::StoreError),
}

impl From<ptolemy_storage::StoreError> for CompactionError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        Self::Store(e)
    }
}

impl IntoResponse for CompactionError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg).into_response()
            }
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
}
