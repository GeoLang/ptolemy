// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Per-dataset and per-branch RBAC permission endpoints.
//!
//! Permission hierarchy: admin > write > read.
//!
//! Who may manage grants: the instance admin role anywhere, or the holder of an
//! `admin` grant on the dataset in question — including for grants on that
//! dataset's branches. A branch-level `admin` grant does not carry delegation:
//! it would let a branch grantee widen their own scope. A dataset with no rows
//! has no dataset admin, so only an instance admin can make the first grant;
//! normally the creator auto-grant provides one.
//!
//! Attaching a dataset to a project is the other way access is handed out here.
//! It grants no rows: it records the project on the dataset, and from then on the
//! caller's effective role on that project counts as a dataset grant. So it takes
//! both halves of the change: admin on the dataset, editor or owner on the
//! project. It makes the dataset private on the way, because a project's data
//! readable by anyone who asks is not what attaching it meant, except when the
//! dataset is already in that project and the attach only repeats itself.
//!
//! Detaching takes either half instead, so the dataset's own admin can undo an
//! attach without joining the project.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, put},
};
use ptolemy_storage::{BranchPermission, DatasetPermission, StoreError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppState, auth::Actor};

/// Managing grants needs the instance admin role or an `admin` grant on this
/// dataset. Off entirely in dev mode, where there is no identity to check.
async fn require_dataset_admin(
    store: &AppState,
    actor: &Actor,
    dataset_id: Uuid,
) -> Result<(), RbacError> {
    if !actor.enforces() || actor.is_instance_admin() {
        return Ok(());
    }
    let Some(user_id) = actor.id() else {
        return Err(RbacError::Forbidden(
            "managing permissions needs a token".into(),
        ));
    };
    if store.is_dataset_admin(dataset_id, user_id).await? {
        Ok(())
    } else {
        Err(RbacError::Forbidden(format!(
            "managing permissions on dataset {dataset_id} needs an admin grant on it"
        )))
    }
}

/// Changing what a project's members reach takes the project's own editor bar. A
/// project viewer who administers some dataset would otherwise attach it and hand
/// themselves the write access their viewer role withholds.
async fn require_project_editor(
    store: &AppState,
    actor: &Actor,
    project_id: Uuid,
) -> Result<(), RbacError> {
    if !actor.enforces() || actor.is_instance_admin() {
        return Ok(());
    }
    let Some(user_id) = actor.id() else {
        return Err(RbacError::Forbidden(
            "attaching a dataset to a project needs a token".into(),
        ));
    };
    match store.project_role(project_id, user_id).await? {
        Some(role) if role.can_edit() => Ok(()),
        Some(_) => Err(RbacError::Forbidden(format!(
            "attaching a dataset to project {project_id} needs an editor or owner role on it"
        ))),
        // a project the caller is not in is not theirs to know about
        None => Err(RbacError::Store(StoreError::NotFound(format!(
            "project {project_id}"
        )))),
    }
}

/// Detaching takes either half of what attaching took: an `admin` grant on the
/// dataset, or editor or owner on the project it is leaving. Requiring both
/// leaves a dataset admin outside the project unable to revoke the access their
/// own attach handed out.
///
/// A caller in neither position gets a 403 naming both ways in, not the 404
/// `require_project_editor` gives a non-member. Reaching this handler already
/// took reading the dataset, and an attached dataset is private, so there is no
/// id left to confirm.
async fn require_detach_role(
    store: &AppState,
    actor: &Actor,
    dataset_id: Uuid,
    project_id: Uuid,
) -> Result<(), RbacError> {
    match require_dataset_admin(store, actor, dataset_id).await {
        Ok(()) => return Ok(()),
        Err(RbacError::Forbidden(_)) => {}
        Err(other) => return Err(other),
    }
    match require_project_editor(store, actor, project_id).await {
        Ok(()) => Ok(()),
        Err(RbacError::Forbidden(_) | RbacError::Store(StoreError::NotFound(_))) => {
            Err(RbacError::Forbidden(format!(
                "detaching dataset {dataset_id} needs an admin grant on it or an editor \
                 or owner role on project {project_id}"
            )))
        }
        Err(other) => Err(other),
    }
}

/// Same check for a branch endpoint, resolved through the branch's dataset.
async fn require_branch_dataset_admin(
    store: &AppState,
    actor: &Actor,
    branch_id: Uuid,
) -> Result<(), RbacError> {
    // resolving the branch first means an unknown branch is a 404, not a 403
    let dataset_id = store.get_branch(branch_id).await?.dataset_id;
    require_dataset_admin(store, actor, dataset_id).await
}

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
        // Project membership as a dataset grant
        .route(
            "/datasets/{id}/project",
            put(attach_dataset_project).delete(detach_dataset_project),
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
    actor: Actor,
) -> Result<Json<Vec<DatasetPermission>>, RbacError> {
    require_dataset_admin(&store, &actor, dataset_id).await?;
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
    actor: Actor,
    Json(req): Json<GrantRequest>,
) -> Result<(StatusCode, Json<DatasetPermission>), RbacError> {
    require_dataset_admin(&store, &actor, dataset_id).await?;
    validate_grant(&req)?;
    let perm = store
        .grant_dataset_permission(
            dataset_id,
            &req.user_id,
            &req.permission,
            actor.or_body(&req.granted_by),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(perm)))
}

async fn revoke_dataset_permission(
    State(store): State<AppState>,
    Path((dataset_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
) -> Result<StatusCode, RbacError> {
    require_dataset_admin(&store, &actor, dataset_id).await?;
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
    actor: Actor,
    axum::extract::Query(params): axum::extract::Query<CheckParams>,
) -> Result<Json<serde_json::Value>, RbacError> {
    require_dataset_admin(&store, &actor, dataset_id).await?;
    validate_permission(&params.required)?;
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

// ─── Dataset Project ────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachProjectRequest {
    project_id: Uuid,
    /// The project the caller believes the dataset is in right now. Absent
    /// attaches whatever it currently belongs to.
    #[serde(default)]
    expected_project_id: Option<Uuid>,
}

#[derive(Serialize)]
struct DatasetProjectResponse {
    dataset_id: Uuid,
    project_id: Option<Uuid>,
}

/// Attach a dataset to a project, so the project's members reach it without a
/// grant each: viewer reads, editor writes, owner administers, and an explicit
/// grant that is stronger still stands. The dataset becomes private in the same
/// transaction, unless it is already in the named project, where a repeated
/// attach leaves the visibility an admin chose alone.
///
/// Both halves are required, and each is refused on its own: `admin` on the
/// dataset, editor or owner on the project. Moving a dataset from one project to
/// another needs no consent from the project it leaves: dataset admin plus
/// editor on the destination is the whole bar.
///
/// The two role checks run before the store's transaction opens, so a role
/// revoked mid-request can still land this update. `expected_project_id` is what
/// closes the window that matters: a caller that names the project it read
/// refuses an update onto a dataset that moved in between, and gets a `409`.
///
/// An external dataset is refused twice: by the write ladder ahead of this
/// handler, and by the store itself, which is the one that still refuses when
/// auth is disabled and every check in this file passes by definition.
async fn attach_dataset_project(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    actor: Actor,
    Json(req): Json<AttachProjectRequest>,
) -> Result<Json<DatasetProjectResponse>, RbacError> {
    require_dataset_admin(&store, &actor, dataset_id).await?;
    require_project_editor(&store, &actor, req.project_id).await?;
    store
        .attach_dataset_to_project(dataset_id, req.project_id, req.expected_project_id)
        .await?;
    Ok(Json(DatasetProjectResponse {
        dataset_id,
        project_id: Some(req.project_id),
    }))
}

/// Detach, authorized against the dataset or against the project it is leaving,
/// by `require_detach_role`.
///
/// Visibility is left private. Attaching made it private, and a detach that
/// published it would turn losing project access into an accidental release, so
/// it stays private until an admin flips it back.
async fn detach_dataset_project(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<DatasetProjectResponse>, RbacError> {
    let Some(project_id) = store.dataset_project(dataset_id).await? else {
        return Err(RbacError::Store(StoreError::NotFound(format!(
            "dataset {dataset_id} belongs to no project"
        ))));
    };
    require_detach_role(&store, &actor, dataset_id, project_id).await?;
    store
        .detach_dataset_from_project(dataset_id, project_id)
        .await?;
    Ok(Json(DatasetProjectResponse {
        dataset_id,
        project_id: None,
    }))
}

// ─── Branch Permissions ─────────────────────────────────────────────

async fn list_branch_permissions(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<BranchPermission>>, RbacError> {
    require_branch_dataset_admin(&store, &actor, branch_id).await?;
    // We need to query the branch permissions table directly
    let rows = sqlx::query(
        "SELECT id, branch_id, user_id, permission, granted_by, granted_at
         FROM branch_permissions WHERE branch_id = $1 ORDER BY granted_at",
    )
    .bind(branch_id)
    .fetch_all(store.read_pool())
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
    actor: Actor,
    Json(req): Json<GrantRequest>,
) -> Result<(StatusCode, Json<BranchPermission>), RbacError> {
    require_branch_dataset_admin(&store, &actor, branch_id).await?;
    validate_grant(&req)?;
    let perm = store
        .grant_branch_permission(
            branch_id,
            &req.user_id,
            &req.permission,
            actor.or_body(&req.granted_by),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(perm)))
}

async fn revoke_branch_permission(
    State(store): State<AppState>,
    Path((branch_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
) -> Result<StatusCode, RbacError> {
    require_branch_dataset_admin(&store, &actor, branch_id).await?;
    store.revoke_branch_permission(branch_id, &user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn check_branch_permission(
    State(store): State<AppState>,
    Path((branch_id, user_id)): Path<(Uuid, String)>,
    actor: Actor,
    axum::extract::Query(params): axum::extract::Query<CheckParams>,
) -> Result<Json<serde_json::Value>, RbacError> {
    require_branch_dataset_admin(&store, &actor, branch_id).await?;
    validate_permission(&params.required)?;
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

/// A grant names a token subject and a level. Identity is external to this
/// service so there is nothing to validate a subject against, but a blank one is
/// nobody: it would sit there as a row a token whose `sub` is empty matches.
fn validate_grant(req: &GrantRequest) -> Result<(), RbacError> {
    if req.user_id.trim().is_empty() {
        return Err(RbacError::BadRequest("user_id must not be blank".into()));
    }
    validate_permission(&req.permission)
}

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
    Forbidden(String),
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
            Self::Store(e) => {
                let (status, message) = crate::errors::store_error_status(&e);
                (status, message).into_response()
            }
            Self::Db(e) => {
                crate::errors::log_db_error("rbac", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
        }
    }
}
