// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Programmatic conflict resolution API.
//!
//! When a merge produces conflicts, clients can use this API to:
//! 1. List pending conflicts for a merge attempt
//! 2. Resolve conflicts by choosing ours/theirs/custom
//! 3. Finalize the merge after all conflicts are resolved

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_core::diff::DiffOp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn conflict_routes() -> Router<AppState> {
    Router::new()
        .route("/conflicts/{merge_id}", get(list_conflicts))
        .route("/conflicts/{merge_id}/resolve", post(resolve_conflicts))
}

#[derive(Serialize)]
struct ConflictDetail {
    feature_id: Uuid,
    field: Option<String>,
    ours: Option<serde_json::Value>,
    theirs: Option<serde_json::Value>,
    base: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Resolution {
    feature_id: Uuid,
    strategy: ResolutionStrategy,
    /// Custom value if strategy is Custom
    custom_properties: Option<serde_json::Value>,
    custom_geometry_wkb_hex: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum ResolutionStrategy {
    Ours,
    Theirs,
    Custom,
}

#[derive(Deserialize)]
struct ResolveRequest {
    resolutions: Vec<Resolution>,
    message: String,
    author: String,
}

async fn list_conflicts(
    State(store): State<AppState>,
    Path(merge_id): Path<Uuid>,
) -> Result<Json<Vec<ConflictDetail>>, ConflictError> {
    // merge_id corresponds to the source branch ID in a failed merge.
    // Look up features that differ between source and target.
    let rows = sqlx::query(
        "WITH source_latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id, fv.properties, fv.geometry
            FROM feature_versions fv
            JOIN changesets c ON c.id = fv.changeset_id
            WHERE c.branch_id = $1
            ORDER BY fv.feature_id, fv.created_at DESC
        ),
        target_branch AS (
            SELECT b.id FROM branches b
            WHERE b.dataset_id = (SELECT dataset_id FROM branches WHERE id = $1)
              AND b.name = 'main'
            LIMIT 1
        ),
        target_latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id, fv.properties, fv.geometry
            FROM feature_versions fv
            JOIN changesets c ON c.id = fv.changeset_id
            JOIN target_branch tb ON c.branch_id = tb.id
            ORDER BY fv.feature_id, fv.created_at DESC
        )
        SELECT
            s.feature_id,
            s.properties as ours,
            t.properties as theirs
        FROM source_latest s
        JOIN target_latest t ON s.feature_id = t.feature_id
        WHERE s.properties IS DISTINCT FROM t.properties
           OR ST_AsBinary(s.geometry) IS DISTINCT FROM ST_AsBinary(t.geometry)",
    )
    .bind(merge_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| ConflictDetail {
                feature_id: row.get("feature_id"),
                field: None,
                ours: row.get("ours"),
                theirs: row.get("theirs"),
                base: None,
            })
            .collect(),
    ))
}

async fn resolve_conflicts(
    State(store): State<AppState>,
    Path(merge_id): Path<Uuid>,
    Json(req): Json<ResolveRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ConflictError> {
    let mut ops = Vec::new();

    for res in &req.resolutions {
        match res.strategy {
            ResolutionStrategy::Ours => {
                // Keep source version — no-op (already in source branch)
            }
            ResolutionStrategy::Theirs => {
                // Take target version — fetch target's current state
                let row = sqlx::query(
                    "SELECT ST_AsBinary(geometry) as geom, properties
                     FROM feature_versions
                     WHERE feature_id = $1
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(res.feature_id)
                .fetch_optional(store.pool())
                .await?;

                if let Some(r) = row {
                    ops.push(DiffOp::Update {
                        feature_id: res.feature_id,
                        geometry_wkb: r.get::<Option<Vec<u8>>, _>("geom"),
                        properties: r.get::<Option<serde_json::Value>, _>("properties"),
                    });
                }
            }
            ResolutionStrategy::Custom => {
                let geom = res
                    .custom_geometry_wkb_hex
                    .as_ref()
                    .and_then(|h| hex::decode(h).ok());
                ops.push(DiffOp::Update {
                    feature_id: res.feature_id,
                    geometry_wkb: geom,
                    properties: res.custom_properties.clone(),
                });
            }
        }
    }

    if !ops.is_empty() {
        let changeset = store
            .commit(merge_id, &req.message, &req.author, &ops)
            .await?;
        Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "resolved": req.resolutions.len(),
                "changeset_id": changeset.id,
            })),
        ))
    } else {
        Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "resolved": req.resolutions.len(),
                "changeset_id": null,
            })),
        ))
    }
}

// ─── Error Handling ─────────────────────────────────────────────────

enum ConflictError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
}

impl From<sqlx::Error> for ConflictError {
    fn from(e: sqlx::Error) -> Self {
        ConflictError::Db(e)
    }
}

impl From<ptolemy_storage::StoreError> for ConflictError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        ConflictError::Store(e)
    }
}

impl IntoResponse for ConflictError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            ConflictError::Db(e) => {
                tracing::error!("Database error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
            ConflictError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            ConflictError::Store(e) => {
                tracing::error!("Store error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
