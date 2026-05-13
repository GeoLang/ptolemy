// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Multi-tenancy: organization management and dataset isolation.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

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
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(&req.name)
        .bind(&req.slug)
        .execute(store.pool())
        .await?;

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
    Path(org_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Result<StatusCode, TenantError> {
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (org_id, user_id) DO UPDATE SET role = $3",
    )
    .bind(org_id)
    .bind(&req.user_id)
    .bind(&req.role)
    .execute(store.pool())
    .await?;
    Ok(StatusCode::CREATED)
}

async fn remove_member(
    State(store): State<AppState>,
    Path((org_id, user_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, TenantError> {
    sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
        .bind(org_id)
        .bind(&user_id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn org_datasets(
    State(store): State<AppState>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<serde_json::Value>>, TenantError> {
    let rows = sqlx::query(
        "SELECT id, name, srid, geometry_type FROM datasets WHERE org_id = $1 ORDER BY name",
    )
    .bind(org_id)
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
    NotFound(String),
}

impl From<sqlx::Error> for TenantError {
    fn from(e: sqlx::Error) -> Self {
        TenantError::Db(e)
    }
}

impl IntoResponse for TenantError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            TenantError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
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
