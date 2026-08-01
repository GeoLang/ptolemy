// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Cartographic representations — symbology and label rules per dataset.
//!
//! One symbol format is understood rather than merely stored: a rule tagged
//! `esri-drawing-info` holds an Esri layer's `drawingInfo` verbatim, which is
//! what verne writes when it migrates a hosted feature layer. The ArcGIS facade
//! hands that document straight back to Esri clients, and `/style` translates it
//! into Mapbox GL layers for everything else.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
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

use crate::{AppState, auth::Actor};

pub fn cartography_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/symbology",
            get(list_symbology).post(create_symbology),
        )
        .route("/datasets/{id}/style", get(dataset_style))
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

/// The `format` tag on a symbol that holds an Esri layer's `drawingInfo`
/// verbatim, which is how a migrated dataset keeps its original styling.
///
/// Rules are found by this tag and never by the rule name: the name a writer
/// picks is a convention, the tag is what the document promises to be.
pub(crate) const ESRI_DRAWING_INFO: &str = "esri-drawing-info";

/// The dataset's stored Esri symbol document, when it has one. Lowest priority
/// first, so one dataset carrying several answers with the same one everywhere.
///
/// The whole symbol comes back rather than its `drawingInfo`, so a caller can
/// tell a dataset with no Esri style from one whose stored document is broken.
pub(crate) async fn esri_symbol(
    store: &AppState,
    dataset_id: Uuid,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT symbol FROM symbology_rules
          WHERE dataset_id = $1 AND symbol->>'format' = $2
          ORDER BY priority, id LIMIT 1",
    )
    .bind(dataset_id)
    .bind(ESRI_DRAWING_INFO)
    .fetch_optional(store.read_pool())
    .await?;
    Ok(row.map(|r| r.get("symbol")))
}

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
    .fetch_all(store.read_pool())
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
    .fetch_optional(store.read_pool())
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

// ─── Translated style ───────────────────────────────────────────────

/// The source name emitted layers reference when the caller names none.
const DEFAULT_SOURCE: &str = "ptolemy";

#[derive(Deserialize)]
struct StyleQuery {
    /// source name in the caller's own style document
    source: Option<String>,
    #[serde(rename = "sourceLayer")]
    source_layer: Option<String>,
}

/// The dataset's geometry as the translator names it, which is what decides
/// whether symbols become circle, line or fill layers.
fn style_geometry(stored: &str) -> Option<jung_esri::Geometry> {
    match stored {
        "point" | "multipoint" => Some(jung_esri::Geometry::Point),
        "linestring" | "multilinestring" => Some(jung_esri::Geometry::Line),
        "polygon" | "multipolygon" => Some(jung_esri::Geometry::Polygon),
        _ => None,
    }
}

/// What a JSON value is, for an error that has to say why a stored document
/// could not be read.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// The dataset's stored Esri style as Mapbox GL layers, with the images they name
/// and everything the translation dropped listed alongside them.
///
/// The losses are part of the answer rather than a log line: a client showing a
/// migrated layer needs to know the renderer had a size ramp nobody drew.
///
/// `images` holds the bitmaps a picture marker or fill inlines, as data URIs the
/// consumer registers before the layers draw. Passed through as the translator
/// built them: nothing here decodes the base64 or looks at the bytes.
async fn dataset_style(
    State(store): State<AppState>,
    actor: Actor,
    Path(dataset_id): Path<Uuid>,
    Query(query): Query<StyleQuery>,
) -> Result<Json<serde_json::Value>, CartoError> {
    let reader = actor.reader();
    let visible = ptolemy_storage::visible_datasets_sql("d", 2, 3);
    let row = sqlx::query(&format!(
        "SELECT d.name, d.geometry_type FROM datasets d WHERE d.id = $1 AND {visible}"
    ))
    .bind(dataset_id)
    .bind(reader.bypass)
    .bind(reader.id.as_deref())
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(CartoError::NotFound)?;
    let name: String = row.get("name");
    let stored_geometry: String = row.get("geometry_type");

    let symbol = esri_symbol(&store, dataset_id)
        .await?
        .ok_or(CartoError::NoEsriStyle)?;
    let drawing_info = symbol.get("drawingInfo").ok_or_else(|| {
        CartoError::Unprocessable(format!(
            "the stored {ESRI_DRAWING_INFO} symbol has no drawingInfo key"
        ))
    })?;
    if !drawing_info.is_object() {
        return Err(CartoError::Unprocessable(format!(
            "the stored drawingInfo is {}, not a JSON object",
            json_kind(drawing_info)
        )));
    }
    let geometry = style_geometry(&stored_geometry).ok_or_else(|| {
        CartoError::Unprocessable(format!(
            "the dataset's geometry_type is {stored_geometry}, which has no single symbol \
             kind to draw, so its style cannot be translated"
        ))
    })?;

    let source = jung_esri::Source {
        source: query.source.unwrap_or_else(|| DEFAULT_SOURCE.to_string()),
        source_layer: query.source_layer.unwrap_or(name),
    };
    let translated = jung_esri::translate(drawing_info, &source, geometry);
    Ok(Json(serde_json::json!({
        "source": source.source,
        "sourceLayer": source.source_layer,
        "layers": translated.layers,
        // the bitmaps a picture symbol carries, keyed by the name the layers
        // reference them under. Always present, empty for a vector-only style, so
        // a consumer registers what is there without testing for the key
        "images": translated.images,
        "losses": translated.losses.iter()
            .map(|loss| serde_json::json!({"path": loss.path, "reason": loss.reason}))
            .collect::<Vec<_>>(),
    })))
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
    .fetch_all(store.read_pool())
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
    .fetch_optional(store.read_pool())
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
    /// the dataset is readable and simply carries no stored esri style
    NoEsriStyle,
    /// a stored document or a dataset the translator cannot work from
    Unprocessable(String),
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
            CartoError::NoEsriStyle => (
                StatusCode::NOT_FOUND,
                format!(
                    "the dataset has no stored esri style: no symbology rule on it is tagged {ESRI_DRAWING_INFO}"
                ),
            ),
            CartoError::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            CartoError::Store(e) => crate::errors::store_error_status(&e),
            CartoError::Db(e) => {
                crate::errors::log_db_error("cartography", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
