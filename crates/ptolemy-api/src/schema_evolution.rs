// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Schema evolution endpoints — versioned schema changes for datasets.
//!
//! Supports adding, removing, renaming fields and changing types without
//! losing version history. Each schema change is tracked as a migration.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use ptolemy_storage::SchemaMigration;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;

pub fn schema_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/schema/migrations",
            get(list_migrations).post(apply_migration),
        )
        .route("/datasets/{id}/schema/version", get(get_version))
}

async fn list_migrations(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<SchemaMigration>>, SchemaError> {
    let migrations = store.list_schema_migrations(dataset_id).await?;
    Ok(Json(migrations))
}

async fn get_version(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, SchemaError> {
    let version = store.get_schema_version(dataset_id).await?;
    Ok(Json(serde_json::json!({
        "dataset_id": dataset_id,
        "schema_version": version,
    })))
}

#[derive(Deserialize)]
struct ApplyMigrationRequest {
    description: String,
    /// "add_field", "remove_field", "rename_field", "change_type"
    migration_type: String,
    #[serde(default)]
    field_name: Option<String>,
    #[serde(default)]
    old_definition: Option<serde_json::Value>,
    #[serde(default)]
    new_definition: Option<serde_json::Value>,
    applied_by: String,
    #[serde(default)]
    rollback_sql: Option<String>,
}

async fn apply_migration(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<ApplyMigrationRequest>,
) -> Result<(StatusCode, Json<SchemaMigration>), SchemaError> {
    // Validate migration type
    let valid_types = ["add_field", "remove_field", "rename_field", "change_type"];
    if !valid_types.contains(&req.migration_type.as_str()) {
        return Err(SchemaError::BadRequest(format!(
            "invalid migration_type: '{}'. Must be one of: {}",
            req.migration_type,
            valid_types.join(", ")
        )));
    }

    let current_version = store.get_schema_version(dataset_id).await?;
    let next_version = current_version + 1;
    let now = OffsetDateTime::now_utc();

    let migration = SchemaMigration {
        id: Uuid::now_v7(),
        dataset_id,
        version: next_version,
        description: req.description,
        migration_type: req.migration_type,
        field_name: req.field_name,
        old_definition: req.old_definition,
        new_definition: req.new_definition,
        applied_by: req.applied_by,
        applied_at: now,
        rollback_sql: req.rollback_sql,
    };

    store.apply_schema_migration(&migration).await?;
    Ok((StatusCode::CREATED, Json(migration)))
}

// ─── Error type ─────────────────────────────────────────────────────

#[derive(Debug)]
enum SchemaError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl From<ptolemy_storage::StoreError> for SchemaError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        Self::Store(e)
    }
}

impl IntoResponse for SchemaError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg).into_response()
            }
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}
