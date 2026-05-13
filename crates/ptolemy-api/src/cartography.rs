// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Cartographic representations — symbology and label rules per dataset.

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

pub fn cartography_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/symbology",
            get(list_symbology).post(create_symbology),
        )
        .route(
            "/symbology/{id}",
            get(get_symbology)
                .put(update_symbology)
                .delete(delete_symbology),
        )
        .route("/datasets/{id}/labels", get(list_labels).post(create_label))
        .route(
            "/labels/{id}",
            get(get_label).put(update_label).delete(delete_label),
        )
}

// ─── Symbology ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct SymbologyRule {
    id: Uuid,
    name: String,
    min_scale: Option<f64>,
    max_scale: Option<f64>,
    filter_expression: Option<String>,
    symbol: serde_json::Value,
    priority: i32,
}

async fn list_symbology(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<SymbologyRule>>, CartoError> {
    let rows = sqlx::query(
        "SELECT id, name, min_scale, max_scale, filter_expression, symbol, priority
         FROM symbology_rules WHERE dataset_id = $1 ORDER BY priority",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| SymbologyRule {
                id: r.get("id"),
                name: r.get("name"),
                min_scale: r.get("min_scale"),
                max_scale: r.get("max_scale"),
                filter_expression: r.get("filter_expression"),
                symbol: r.get("symbol"),
                priority: r.get("priority"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateSymbologyRequest {
    name: String,
    min_scale: Option<f64>,
    max_scale: Option<f64>,
    filter_expression: Option<String>,
    symbol: serde_json::Value,
    #[serde(default)]
    priority: i32,
}

async fn create_symbology(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateSymbologyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), CartoError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO symbology_rules (id, dataset_id, name, min_scale, max_scale, filter_expression, symbol, priority)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    ).bind(id).bind(dataset_id).bind(&req.name)
    .bind(req.min_scale).bind(req.max_scale).bind(&req.filter_expression)
    .bind(&req.symbol).bind(req.priority)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_symbology(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<SymbologyRule>, CartoError> {
    let r = sqlx::query(
        "SELECT id, name, min_scale, max_scale, filter_expression, symbol, priority
         FROM symbology_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(CartoError::NotFound)?;
    Ok(Json(SymbologyRule {
        id: r.get("id"),
        name: r.get("name"),
        min_scale: r.get("min_scale"),
        max_scale: r.get("max_scale"),
        filter_expression: r.get("filter_expression"),
        symbol: r.get("symbol"),
        priority: r.get("priority"),
    }))
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateSymbologyRequest {
    symbol: Option<serde_json::Value>,
    filter_expression: Option<String>,
    min_scale: Option<f64>,
    max_scale: Option<f64>,
    priority: Option<i32>,
}

async fn update_symbology(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateSymbologyRequest>,
) -> Result<StatusCode, CartoError> {
    if let Some(sym) = &req.symbol {
        sqlx::query("UPDATE symbology_rules SET symbol = $2 WHERE id = $1")
            .bind(id)
            .bind(sym)
            .execute(store.pool())
            .await?;
    }
    if let Some(expr) = &req.filter_expression {
        sqlx::query("UPDATE symbology_rules SET filter_expression = $2 WHERE id = $1")
            .bind(id)
            .bind(expr)
            .execute(store.pool())
            .await?;
    }
    if let Some(p) = req.priority {
        sqlx::query("UPDATE symbology_rules SET priority = $2 WHERE id = $1")
            .bind(id)
            .bind(p)
            .execute(store.pool())
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_symbology(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, CartoError> {
    sqlx::query("DELETE FROM symbology_rules WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Labels ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LabelRule {
    id: Uuid,
    name: String,
    min_scale: Option<f64>,
    max_scale: Option<f64>,
    label_expression: String,
    placement: String,
    font: serde_json::Value,
    priority: i32,
}

async fn list_labels(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<LabelRule>>, CartoError> {
    let rows = sqlx::query(
        "SELECT id, name, min_scale, max_scale, label_expression, placement, font, priority
         FROM label_rules WHERE dataset_id = $1 ORDER BY priority",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| LabelRule {
                id: r.get("id"),
                name: r.get("name"),
                min_scale: r.get("min_scale"),
                max_scale: r.get("max_scale"),
                label_expression: r.get("label_expression"),
                placement: r.get("placement"),
                font: r.get("font"),
                priority: r.get("priority"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateLabelRequest {
    name: String,
    min_scale: Option<f64>,
    max_scale: Option<f64>,
    label_expression: String,
    #[serde(default = "default_placement")]
    placement: String,
    #[serde(default = "default_font")]
    font: serde_json::Value,
    #[serde(default)]
    priority: i32,
}
fn default_placement() -> String {
    "point_on_surface".into()
}
fn default_font() -> serde_json::Value {
    serde_json::json!({"family": "Arial", "size": 12})
}

async fn create_label(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateLabelRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), CartoError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO label_rules (id, dataset_id, name, min_scale, max_scale, label_expression, placement, font, priority)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    ).bind(id).bind(dataset_id).bind(&req.name)
    .bind(req.min_scale).bind(req.max_scale)
    .bind(&req.label_expression).bind(&req.placement).bind(&req.font).bind(req.priority)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_label(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<LabelRule>, CartoError> {
    let r = sqlx::query(
        "SELECT id, name, min_scale, max_scale, label_expression, placement, font, priority
         FROM label_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(CartoError::NotFound)?;
    Ok(Json(LabelRule {
        id: r.get("id"),
        name: r.get("name"),
        min_scale: r.get("min_scale"),
        max_scale: r.get("max_scale"),
        label_expression: r.get("label_expression"),
        placement: r.get("placement"),
        font: r.get("font"),
        priority: r.get("priority"),
    }))
}

#[derive(Deserialize)]
struct UpdateLabelRequest {
    label_expression: Option<String>,
    placement: Option<String>,
    font: Option<serde_json::Value>,
    priority: Option<i32>,
}

async fn update_label(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateLabelRequest>,
) -> Result<StatusCode, CartoError> {
    if let Some(expr) = &req.label_expression {
        sqlx::query("UPDATE label_rules SET label_expression = $2 WHERE id = $1")
            .bind(id)
            .bind(expr)
            .execute(store.pool())
            .await?;
    }
    if let Some(p) = &req.placement {
        sqlx::query("UPDATE label_rules SET placement = $2 WHERE id = $1")
            .bind(id)
            .bind(p)
            .execute(store.pool())
            .await?;
    }
    if let Some(f) = &req.font {
        sqlx::query("UPDATE label_rules SET font = $2 WHERE id = $1")
            .bind(id)
            .bind(f)
            .execute(store.pool())
            .await?;
    }
    if let Some(p) = req.priority {
        sqlx::query("UPDATE label_rules SET priority = $2 WHERE id = $1")
            .bind(id)
            .bind(p)
            .execute(store.pool())
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_label(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, CartoError> {
    sqlx::query("DELETE FROM label_rules WHERE id = $1")
        .bind(id)
        .execute(store.pool())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

enum CartoError {
    Db(sqlx::Error),
    NotFound,
}
impl From<sqlx::Error> for CartoError {
    fn from(e: sqlx::Error) -> Self {
        CartoError::Db(e)
    }
}
impl IntoResponse for CartoError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            CartoError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            CartoError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
