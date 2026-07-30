// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Cartographic representations — symbology and label rules per dataset.

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use ptolemy_storage::{
    WriteGrant,
    writes::{LabelRuleInput, LabelRulePatch, SymbologyRuleInput, SymbologyRulePatch},
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<CreateSymbologyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), CartoError> {
    let id = store
        .create_symbology_rule(
            &grant,
            &SymbologyRuleInput {
                name: &req.name,
                min_scale: req.min_scale,
                max_scale: req.max_scale,
                filter_expression: req.filter_expression.as_deref(),
                symbol: &req.symbol,
                priority: req.priority,
            },
        )
        .await?;
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<UpdateSymbologyRequest>,
) -> Result<StatusCode, CartoError> {
    store
        .update_symbology_rule(
            &grant,
            &SymbologyRulePatch {
                symbol: req.symbol.as_ref(),
                filter_expression: req.filter_expression.as_deref(),
                priority: req.priority,
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_symbology(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
) -> Result<StatusCode, CartoError> {
    store.delete_symbology_rule(&grant).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Labels ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LabelRule {
    id: Uuid,
    name: String,
    min_scale: Option<f64>,
    max_scale: Option<f64>,
    field_expression: String,
    placement: serde_json::Value,
    font: serde_json::Value,
    priority: i32,
}

async fn list_labels(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<LabelRule>>, CartoError> {
    let rows = sqlx::query(
        "SELECT id, name, min_scale, max_scale, field_expression, placement, font, priority
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
                field_expression: r.get("field_expression"),
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
    field_expression: String,
    #[serde(default = "default_placement")]
    placement: serde_json::Value,
    #[serde(default = "default_font")]
    font: serde_json::Value,
    #[serde(default)]
    priority: i32,
}
fn default_placement() -> serde_json::Value {
    serde_json::json!({"type": "point_on_surface"})
}
fn default_font() -> serde_json::Value {
    serde_json::json!({"family": "Arial", "size": 12})
}

async fn create_label(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<CreateLabelRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), CartoError> {
    let id = store
        .create_label_rule(
            &grant,
            &LabelRuleInput {
                name: &req.name,
                min_scale: req.min_scale,
                max_scale: req.max_scale,
                field_expression: &req.field_expression,
                placement: &req.placement,
                font: &req.font,
                priority: req.priority,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_label(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<LabelRule>, CartoError> {
    let r = sqlx::query(
        "SELECT id, name, min_scale, max_scale, field_expression, placement, font, priority
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
        field_expression: r.get("field_expression"),
        placement: r.get("placement"),
        font: r.get("font"),
        priority: r.get("priority"),
    }))
}

#[derive(Deserialize)]
struct UpdateLabelRequest {
    field_expression: Option<String>,
    placement: Option<serde_json::Value>,
    font: Option<serde_json::Value>,
    priority: Option<i32>,
}

async fn update_label(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<UpdateLabelRequest>,
) -> Result<StatusCode, CartoError> {
    store
        .update_label_rule(
            &grant,
            &LabelRulePatch {
                field_expression: req.field_expression.as_deref(),
                placement: req.placement.as_ref(),
                font: req.font.as_ref(),
                priority: req.priority,
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_label(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
) -> Result<StatusCode, CartoError> {
    store.delete_label_rule(&grant).await?;
    Ok(StatusCode::NO_CONTENT)
}

enum CartoError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
    NotFound,
}
impl From<sqlx::Error> for CartoError {
    fn from(e: sqlx::Error) -> Self {
        CartoError::Db(e)
    }
}
impl From<ptolemy_storage::StoreError> for CartoError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        CartoError::Store(e)
    }
}
impl IntoResponse for CartoError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            CartoError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            CartoError::Store(e) => crate::errors::store_error_status(&e),
            CartoError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
