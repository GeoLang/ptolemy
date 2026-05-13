// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Relationship classes — define and navigate typed associations between features.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn relationship_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/relationships",
            get(list_classes).post(create_class),
        )
        .route(
            "/relationship-classes/{id}",
            get(get_class).delete(delete_class),
        )
        .route(
            "/relationship-classes/{id}/records",
            get(list_records).post(create_record),
        )
        .route("/relationship-records/{id}", delete(delete_record))
        .route("/features/{id}/related", get(get_related_features))
}

#[derive(Serialize)]
struct RelationshipClass {
    id: Uuid,
    name: String,
    origin_dataset_id: Uuid,
    destination_dataset_id: Uuid,
    rel_type: String,
    cardinality: String,
    forward_label: String,
    backward_label: String,
}

async fn list_classes(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<RelationshipClass>>, RelError> {
    let rows = sqlx::query(
        "SELECT id, name, origin_dataset_id, destination_dataset_id,
                rel_type, cardinality, forward_label, backward_label
         FROM relationship_classes
         WHERE origin_dataset_id = $1 OR destination_dataset_id = $1
         ORDER BY name",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| RelationshipClass {
                id: r.get("id"),
                name: r.get("name"),
                origin_dataset_id: r.get("origin_dataset_id"),
                destination_dataset_id: r.get("destination_dataset_id"),
                rel_type: r.get("rel_type"),
                cardinality: r.get("cardinality"),
                forward_label: r.get("forward_label"),
                backward_label: r.get("backward_label"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateClassRequest {
    name: String,
    origin_dataset_id: Uuid,
    destination_dataset_id: Uuid,
    #[serde(default = "default_rel_type")]
    rel_type: String,
    #[serde(default = "default_cardinality")]
    cardinality: String,
    #[serde(default)]
    forward_label: String,
    #[serde(default)]
    backward_label: String,
}
fn default_rel_type() -> String {
    "simple".into()
}
fn default_cardinality() -> String {
    "one_to_many".into()
}

async fn create_class(
    State(store): State<AppState>,
    Path(_dataset_id): Path<Uuid>,
    Json(req): Json<CreateClassRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), RelError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relationship_classes
            (id, name, origin_dataset_id, destination_dataset_id, rel_type, cardinality, forward_label, backward_label)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    ).bind(id).bind(&req.name).bind(req.origin_dataset_id).bind(req.destination_dataset_id)
    .bind(&req.rel_type).bind(&req.cardinality).bind(&req.forward_label).bind(&req.backward_label)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_class(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RelationshipClass>, RelError> {
    let r = sqlx::query(
        "SELECT id, name, origin_dataset_id, destination_dataset_id,
                rel_type, cardinality, forward_label, backward_label
         FROM relationship_classes WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(RelError::NotFound)?;
    Ok(Json(RelationshipClass {
        id: r.get("id"),
        name: r.get("name"),
        origin_dataset_id: r.get("origin_dataset_id"),
        destination_dataset_id: r.get("destination_dataset_id"),
        rel_type: r.get("rel_type"),
        cardinality: r.get("cardinality"),
        forward_label: r.get("forward_label"),
        backward_label: r.get("backward_label"),
    }))
}

async fn delete_class(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, RelError> {
    sqlx::query("DELETE FROM relationship_classes WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Records ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RelRecord {
    id: Uuid,
    origin_feature_id: Uuid,
    destination_feature_id: Uuid,
    properties: serde_json::Value,
}

async fn list_records(
    State(store): State<AppState>,
    Path(class_id): Path<Uuid>,
) -> Result<Json<Vec<RelRecord>>, RelError> {
    let rows = sqlx::query(
        "SELECT id, origin_feature_id, destination_feature_id, properties
         FROM relationship_records WHERE class_id = $1",
    )
    .bind(class_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| RelRecord {
                id: r.get("id"),
                origin_feature_id: r.get("origin_feature_id"),
                destination_feature_id: r.get("destination_feature_id"),
                properties: r.get("properties"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateRecordRequest {
    origin_feature_id: Uuid,
    destination_feature_id: Uuid,
    #[serde(default)]
    properties: serde_json::Value,
}

async fn create_record(
    State(store): State<AppState>,
    Path(class_id): Path<Uuid>,
    Json(req): Json<CreateRecordRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), RelError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relationship_records (id, class_id, origin_feature_id, destination_feature_id, properties)
         VALUES ($1, $2, $3, $4, $5)",
    ).bind(id).bind(class_id).bind(req.origin_feature_id).bind(req.destination_feature_id).bind(&req.properties)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn delete_record(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, RelError> {
    sqlx::query("DELETE FROM relationship_records WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Navigate relationships from a feature.
#[derive(Deserialize)]
struct RelatedQuery {
    direction: Option<String>, // forward, backward, both
}

async fn get_related_features(
    State(store): State<AppState>,
    Path(feature_id): Path<Uuid>,
    Query(q): Query<RelatedQuery>,
) -> Result<Json<serde_json::Value>, RelError> {
    let dir = q.direction.unwrap_or_else(|| "both".into());

    let forward = if dir != "backward" {
        let rows = sqlx::query(
            "SELECT rr.destination_feature_id, rc.name as class_name, rc.forward_label
             FROM relationship_records rr
             JOIN relationship_classes rc ON rc.id = rr.class_id
             WHERE rr.origin_feature_id = $1",
        )
        .bind(feature_id)
        .fetch_all(store.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                serde_json::json!({
                    "feature_id": r.get::<Uuid, _>("destination_feature_id"),
                    "class": r.get::<String, _>("class_name"),
                    "label": r.get::<String, _>("forward_label"),
                })
            })
            .collect::<Vec<_>>()
    } else {
        vec![]
    };

    let backward = if dir != "forward" {
        let rows = sqlx::query(
            "SELECT rr.origin_feature_id, rc.name as class_name, rc.backward_label
             FROM relationship_records rr
             JOIN relationship_classes rc ON rc.id = rr.class_id
             WHERE rr.destination_feature_id = $1",
        )
        .bind(feature_id)
        .fetch_all(store.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                serde_json::json!({
                    "feature_id": r.get::<Uuid, _>("origin_feature_id"),
                    "class": r.get::<String, _>("class_name"),
                    "label": r.get::<String, _>("backward_label"),
                })
            })
            .collect::<Vec<_>>()
    } else {
        vec![]
    };

    Ok(Json(serde_json::json!({
        "forward": forward,
        "backward": backward,
    })))
}

enum RelError {
    Db(sqlx::Error),
    NotFound,
}
impl From<sqlx::Error> for RelError {
    fn from(e: sqlx::Error) -> Self {
        RelError::Db(e)
    }
}
impl IntoResponse for RelError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            RelError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            RelError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
