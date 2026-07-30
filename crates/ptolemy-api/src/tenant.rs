// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Multi-tenancy: organization management and dataset isolation.

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use ptolemy_storage::WriteGrant;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn tenant_routes() -> Router<AppState> {
    Router::new()
        .route("/orgs", get(list_orgs).post(create_org))
        .route("/orgs/{id}", get(get_org))
        .route("/orgs/{id}/members", get(list_members).post(add_member))
        .route(
            "/orgs/{id}/members/{user_id}",
            axum::routing::delete(remove_member),
        )
        .route("/orgs/{id}/datasets", get(org_datasets))
}

#[derive(Serialize)]
struct Organization {
    id: Uuid,
    name: String,
    slug: String,
}

#[derive(Serialize)]
struct OrgMember {
    user_id: String,
    role: String,
}

async fn list_orgs(State(store): State<AppState>) -> Result<Json<Vec<Organization>>, TenantError> {
    let rows = sqlx::query("SELECT id, name, slug FROM organizations ORDER BY name")
        .fetch_all(store.pool())
        .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| Organization {
                id: row.get("id"),
                name: row.get("name"),
                slug: row.get("slug"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateOrgRequest {
    name: String,
    slug: String,
}

async fn create_org(
    State(store): State<AppState>,
    Json(req): Json<CreateOrgRequest>,
) -> Result<(StatusCode, Json<Organization>), TenantError> {
    let id = store.create_organization(&req.name, &req.slug).await?;

    Ok((
        StatusCode::CREATED,
        Json(Organization {
            id,
            name: req.name,
            slug: req.slug,
        }),
    ))
}

async fn get_org(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Organization>, TenantError> {
    let row = sqlx::query("SELECT id, name, slug FROM organizations WHERE id = $1")
        .bind(id)
        .fetch_optional(store.pool())
        .await?
        .ok_or_else(|| TenantError::NotFound("organization not found".into()))?;

    Ok(Json(Organization {
        id: row.get("id"),
        name: row.get("name"),
        slug: row.get("slug"),
    }))
}

async fn list_members(
    State(store): State<AppState>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<OrgMember>>, TenantError> {
    let rows = sqlx::query("SELECT user_id, role FROM org_members WHERE org_id = $1")
        .bind(org_id)
        .fetch_all(store.pool())
        .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| OrgMember {
                user_id: row.get("user_id"),
                role: row.get("role"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct AddMemberRequest {
    user_id: String,
    #[serde(default = "default_role")]
    role: String,
}

fn default_role() -> String {
    "member".into()
}

async fn add_member(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<AddMemberRequest>,
) -> Result<StatusCode, TenantError> {
    store
        .add_org_member(&grant, &req.user_id, &req.role)
        .await?;
    Ok(StatusCode::CREATED)
}

async fn remove_member(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Path((_org_id, user_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, TenantError> {
    store.remove_org_member(&grant, &user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn org_datasets(
    State(store): State<AppState>,
    Path(org_id): Path<Uuid>,
    actor: Actor,
) -> Result<Json<Vec<serde_json::Value>>, TenantError> {
    let reader = actor.reader();
    let visible = ptolemy_storage::visible_datasets_sql("d", 2, 3);
    let rows = sqlx::query(&format!(
        "SELECT d.id, d.name, d.srid, d.geometry_type FROM datasets d
          WHERE d.org_id = $1 AND {visible} ORDER BY d.name"
    ))
    .bind(org_id)
    .bind(reader.bypass)
    .bind(reader.id.as_deref())
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.get::<Uuid, _>("id"),
                    "name": row.get::<String, _>("name"),
                    "srid": row.get::<i32, _>("srid"),
                    "geometry_type": row.get::<String, _>("geometry_type"),
                })
            })
            .collect(),
    ))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum TenantError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
    NotFound(String),
}

impl From<sqlx::Error> for TenantError {
    fn from(e: sqlx::Error) -> Self {
        TenantError::Db(e)
    }
}

impl From<ptolemy_storage::StoreError> for TenantError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        TenantError::Store(e)
    }
}

impl IntoResponse for TenantError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            TenantError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            TenantError::Store(e) => crate::errors::store_error_status(&e),
            TenantError::Db(e) => {
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
