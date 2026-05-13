// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Data catalog: tags, metadata, and search for datasets.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn catalog_routes() -> Router<AppState> {
    Router::new()
        .route("/catalog/search", get(search_datasets))
        .route("/datasets/{id}/tags", get(list_tags).post(add_tag))
        .route(
            "/datasets/{id}/tags/{tag}",
            axum::routing::delete(remove_tag),
        )
        .route(
            "/datasets/{id}/metadata",
            get(get_metadata).put(set_metadata),
        )
}

// ─── Search ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct SearchResult {
    id: Uuid,
    name: String,
    description: String,
    tags: Vec<String>,
}

async fn search_datasets(
    State(store): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<SearchResult>>, CatalogError> {
    let rows = if let Some(tag) = &q.tag {
        sqlx::query(
            "SELECT d.id, d.name, COALESCE(m.description, '') as description,
                    ARRAY(SELECT tag FROM dataset_tags WHERE dataset_id = d.id) as tags
             FROM datasets d
             LEFT JOIN dataset_metadata m ON m.dataset_id = d.id
             JOIN dataset_tags t ON t.dataset_id = d.id AND t.tag = $1
             WHERE ($2 = '' OR d.name ILIKE '%' || $2 || '%' OR COALESCE(m.description, '') ILIKE '%' || $2 || '%')
             LIMIT $3",
        )
        .bind(tag)
        .bind(&q.q)
        .bind(q.limit)
        .fetch_all(store.pool())
        .await?
    } else {
        sqlx::query(
            "SELECT d.id, d.name, COALESCE(m.description, '') as description,
                    ARRAY(SELECT tag FROM dataset_tags WHERE dataset_id = d.id) as tags
             FROM datasets d
             LEFT JOIN dataset_metadata m ON m.dataset_id = d.id
             WHERE ($1 = '' OR d.name ILIKE '%' || $1 || '%' OR COALESCE(m.description, '') ILIKE '%' || $1 || '%')
             LIMIT $2",
        )
        .bind(&q.q)
        .bind(q.limit)
        .fetch_all(store.pool())
        .await?
    };

    Ok(Json(
        rows.into_iter()
            .map(|row| SearchResult {
                id: row.get("id"),
                name: row.get("name"),
                description: row.get("description"),
                tags: row.get("tags"),
            })
            .collect(),
    ))
}

// ─── Tags ───────────────────────────────────────────────────────────

async fn list_tags(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<String>>, CatalogError> {
    let rows = sqlx::query("SELECT tag FROM dataset_tags WHERE dataset_id = $1 ORDER BY tag")
        .bind(dataset_id)
        .fetch_all(store.pool())
        .await?;

    Ok(Json(rows.into_iter().map(|r| r.get("tag")).collect()))
}

#[derive(Deserialize)]
struct AddTagRequest {
    tag: String,
}

async fn add_tag(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<AddTagRequest>,
) -> Result<StatusCode, CatalogError> {
    sqlx::query(
        "INSERT INTO dataset_tags (dataset_id, tag) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(dataset_id)
    .bind(&req.tag)
    .execute(store.pool())
    .await?;
    Ok(StatusCode::CREATED)
}

async fn remove_tag(
    State(store): State<AppState>,
    Path((dataset_id, tag)): Path<(Uuid, String)>,
) -> Result<StatusCode, CatalogError> {
    sqlx::query("DELETE FROM dataset_tags WHERE dataset_id = $1 AND tag = $2")
        .bind(dataset_id)
        .bind(&tag)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Metadata ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct DatasetMetadata {
    description: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    attribution: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
}

async fn get_metadata(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<DatasetMetadata>, CatalogError> {
    let row = sqlx::query(
        "SELECT description, source, license, attribution, keywords
         FROM dataset_metadata WHERE dataset_id = $1",
    )
    .bind(dataset_id)
    .fetch_optional(store.pool())
    .await?;

    match row {
        Some(r) => Ok(Json(DatasetMetadata {
            description: r.get("description"),
            source: r.get("source"),
            license: r.get("license"),
            attribution: r.get("attribution"),
            keywords: r.get("keywords"),
        })),
        None => Ok(Json(DatasetMetadata {
            description: String::new(),
            source: None,
            license: None,
            attribution: None,
            keywords: vec![],
        })),
    }
}

async fn set_metadata(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<DatasetMetadata>,
) -> Result<StatusCode, CatalogError> {
    sqlx::query(
        "INSERT INTO dataset_metadata (dataset_id, description, source, license, attribution, keywords, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, now())
         ON CONFLICT (dataset_id) DO UPDATE SET
            description = $2, source = $3, license = $4, attribution = $5, keywords = $6, updated_at = now()",
    )
    .bind(dataset_id)
    .bind(&req.description)
    .bind(&req.source)
    .bind(&req.license)
    .bind(&req.attribution)
    .bind(&req.keywords)
    .execute(store.pool())
    .await?;
    Ok(StatusCode::OK)
}

// ─── Error Handling ─────────────────────────────────────────────────

enum CatalogError {
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CatalogError {
    fn from(e: sqlx::Error) -> Self {
        CatalogError::Db(e)
    }
}

impl IntoResponse for CatalogError {
    fn into_response(self) -> axum::response::Response {
        let CatalogError::Db(e) = self;
        tracing::error!("Database error: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response()
    }
}
