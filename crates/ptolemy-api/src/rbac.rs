// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Per-dataset and per-branch RBAC permission endpoints.
//!
//! Permission hierarchy: admin > write > read.
//! Permissions cascade: org membership → dataset permission → branch permission.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use ptolemy_storage::{BranchPermission, DatasetPermission};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

pub fn rbac_routes() -> Router<AppState> {
    Router::new()
        // Dataset permissions
        .route(
            "/datasets/{id}/permissions",
            get(list_dataset_permissions).post(grant_dataset_permission),
        )
        .route(
            "/datasets/{dataset_id}/permissions/{user_id}",
            delete(revoke_dataset_permission),
        )
        .route(
            "/datasets/{dataset_id}/permissions/{user_id}/check",
            get(check_dataset_permission),
        )
        // Branch permissions
        .route(
            "/branches/{id}/permissions",
            get(list_branch_permissions).post(grant_branch_permission),
        )
        .route(
            "/branches/{branch_id}/permissions/{user_id}",
            delete(revoke_branch_permission),
        )
        .route(
            "/branches/{branch_id}/permissions/{user_id}/check",
            get(check_branch_permission),
        )
}

// ─── Dataset Permissions ────────────────────────────────────────────

async fn list_dataset_permissions(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<DatasetPermission>>, RbacError> {
    let perms = store.list_dataset_permissions(dataset_id).await?;
    Ok(Json(perms))
}

#[derive(Deserialize)]
struct GrantRequest {
    user_id: String,
    permission: String,
    granted_by: String,
}

async fn grant_dataset_permission(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<GrantRequest>,
) -> Result<(StatusCode, Json<DatasetPermission>), RbacError> {
    validate_permission(&req.permission)?;
    let perm = store
        .grant_dataset_permission(dataset_id, &req.user_id, &req.permission, &req.granted_by)
        .await?;
    Ok((StatusCode::CREATED, Json(perm)))
}

async fn revoke_dataset_permission(
    State(store): State<AppState>,
    Path((dataset_id, user_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, RbacError> {
    store
        .revoke_dataset_permission(dataset_id, &user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CheckParams {
    #[serde(default = "default_read")]
    required: String,
}

fn default_read() -> String {
    "read".into()
}

async fn check_dataset_permission(
    State(store): State<AppState>,
    Path((dataset_id, user_id)): Path<(Uuid, String)>,
    axum::extract::Query(params): axum::extract::Query<CheckParams>,
) -> Result<Json<serde_json::Value>, RbacError> {
    let allowed = store
        .check_dataset_permission(dataset_id, &user_id, &params.required)
        .await?;
    Ok(Json(serde_json::json!({
        "dataset_id": dataset_id,
        "user_id": user_id,
        "required": params.required,
        "allowed": allowed,
    })))
}

// ─── Branch Permissions ─────────────────────────────────────────────

async fn list_branch_permissions(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<Vec<BranchPermission>>, RbacError> {
    // We need to query the branch permissions table directly
    let rows = sqlx::query(
        "SELECT id, branch_id, user_id, permission, granted_by, granted_at
         FROM branch_permissions WHERE branch_id = $1 ORDER BY granted_at",
    )
    .bind(branch_id)
    .fetch_all(store.pool())
    .await?;

    use sqlx::Row;
    Ok(Json(
        rows.into_iter()
            .map(|r| BranchPermission {
                id: r.get("id"),
                branch_id: r.get("branch_id"),
                user_id: r.get("user_id"),
                permission: r.get("permission"),
                granted_by: r.get("granted_by"),
                granted_at: r.get("granted_at"),
            })
            .collect(),
    ))
}

async fn grant_branch_permission(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<GrantRequest>,
) -> Result<(StatusCode, Json<BranchPermission>), RbacError> {
    validate_permission(&req.permission)?;
    let perm = store
        .grant_branch_permission(branch_id, &req.user_id, &req.permission, &req.granted_by)
        .await?;
    Ok((StatusCode::CREATED, Json(perm)))
}

async fn revoke_branch_permission(
    State(store): State<AppState>,
    Path((branch_id, user_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, RbacError> {
    store.revoke_branch_permission(branch_id, &user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn check_branch_permission(
    State(store): State<AppState>,
    Path((branch_id, user_id)): Path<(Uuid, String)>,
    axum::extract::Query(params): axum::extract::Query<CheckParams>,
) -> Result<Json<serde_json::Value>, RbacError> {
    let allowed = store
        .check_branch_permission(branch_id, &user_id, &params.required)
        .await?;
    Ok(Json(serde_json::json!({
        "branch_id": branch_id,
        "user_id": user_id,
        "required": params.required,
        "allowed": allowed,
    })))
}

// ─── Helpers ────────────────────────────────────────────────────────

fn validate_permission(perm: &str) -> Result<(), RbacError> {
    match perm {
        "read" | "write" | "admin" => Ok(()),
        _ => Err(RbacError::BadRequest(format!(
            "invalid permission: '{perm}'. Must be 'read', 'write', or 'admin'"
        ))),
    }
}

// ─── Error type ─────────────────────────────────────────────────────

#[derive(Debug)]
enum RbacError {
    Store(ptolemy_storage::StoreError),
    Db(sqlx::Error),
    BadRequest(String),
}

impl From<ptolemy_storage::StoreError> for RbacError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<sqlx::Error> for RbacError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl IntoResponse for RbacError {
    fn into_response(self) -> Response {
        match self {
            Self::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg).into_response()
            }
            Self::Store(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            Self::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}
