// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Data catalog: tags, metadata, and search for datasets.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use ptolemy_storage::{WriteGrant, writes::DatasetMetadataInput};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

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
    actor: Actor,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<SearchResult>>, CatalogError> {
    let reader = actor.reader();
    // filtered in SQL rather than after the fact, so LIMIT counts only rows the
    // caller may actually see
    let rows = if let Some(tag) = &q.tag {
        let visible = ptolemy_storage::visible_datasets_sql("d", 4, 5);
        sqlx::query(&format!(
            "SELECT d.id, d.name, COALESCE(m.description, '') as description,
                    ARRAY(SELECT tag FROM dataset_tags WHERE dataset_id = d.id) as tags
             FROM datasets d
             LEFT JOIN dataset_metadata m ON m.dataset_id = d.id
             JOIN dataset_tags t ON t.dataset_id = d.id AND t.tag = $1
             WHERE ($2 = '' OR d.name ILIKE '%' || $2 || '%' OR COALESCE(m.description, '') ILIKE '%' || $2 || '%')
               AND {visible}
             LIMIT $3"
        ))
        .bind(tag)
        .bind(&q.q)
        .bind(q.limit)
        .bind(reader.bypass)
        .bind(reader.id.as_deref())
        .fetch_all(store.pool())
        .await?
    } else {
        let visible = ptolemy_storage::visible_datasets_sql("d", 3, 4);
        sqlx::query(&format!(
            "SELECT d.id, d.name, COALESCE(m.description, '') as description,
                    ARRAY(SELECT tag FROM dataset_tags WHERE dataset_id = d.id) as tags
             FROM datasets d
             LEFT JOIN dataset_metadata m ON m.dataset_id = d.id
             WHERE ($1 = '' OR d.name ILIKE '%' || $1 || '%' OR COALESCE(m.description, '') ILIKE '%' || $1 || '%')
               AND {visible}
             LIMIT $2"
        ))
        .bind(&q.q)
        .bind(q.limit)
        .bind(reader.bypass)
        .bind(reader.id.as_deref())
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<AddTagRequest>,
) -> Result<StatusCode, CatalogError> {
    store.add_dataset_tag(&grant, &req.tag).await?;
    Ok(StatusCode::CREATED)
}

/// `{tag}` is the free-text segment the route-template rule exists for: it is
/// only ever a tag string, never the write target.
async fn remove_tag(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Path((_dataset_id, tag)): Path<(Uuid, String)>,
) -> Result<StatusCode, CatalogError> {
    store.remove_dataset_tag(&grant, &tag).await?;
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<DatasetMetadata>,
) -> Result<StatusCode, CatalogError> {
    store
        .set_dataset_metadata(
            &grant,
            &DatasetMetadataInput {
                description: &req.description,
                source: req.source.as_deref(),
                license: req.license.as_deref(),
                attribution: req.attribution.as_deref(),
                keywords: &req.keywords,
            },
        )
        .await?;
    Ok(StatusCode::OK)
}

// ─── Error Handling ─────────────────────────────────────────────────

enum CatalogError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
}

impl From<sqlx::Error> for CatalogError {
    fn from(e: sqlx::Error) -> Self {
        CatalogError::Db(e)
    }
}

impl From<ptolemy_storage::StoreError> for CatalogError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        CatalogError::Store(e)
    }
}

impl IntoResponse for CatalogError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            CatalogError::Store(e) => crate::errors::store_error_status(&e),
            CatalogError::Db(e) => {
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
