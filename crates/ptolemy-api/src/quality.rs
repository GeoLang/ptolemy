// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Schema validation, topology rules, and data quality API endpoints.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
};
use ptolemy_core::schema::{
    DatasetSchema, FieldDef, GeometryRules, QualityReport, TopologyRule, TopologyRuleType,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;

pub fn quality_routes() -> Router<AppState> {
    Router::new()
        // Schema
        .route("/datasets/{id}/schema", get(get_schema).put(set_schema))
        // Topology rules
        .route(
            "/datasets/{id}/topology",
            get(list_topology_rules).post(add_topology_rule),
        )
        .route("/topology/{rule_id}", delete(delete_topology_rule))
        // Quality
        .route("/branches/{id}/quality", get(quality_report))
        .route("/branches/{id}/repair", post(repair_geometries))
}

// ─── Schema ─────────────────────────────────────────────────────────

async fn get_schema(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Option<DatasetSchema>>, QualityError> {
    let schema = store.get_dataset_schema(dataset_id).await?;
    Ok(Json(schema))
}

#[derive(Deserialize)]
struct SetSchemaRequest {
    fields: Vec<FieldDef>,
    #[serde(default)]
    geometry_rules: Option<GeometryRules>,
}

async fn set_schema(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<SetSchemaRequest>,
) -> Result<StatusCode, QualityError> {
    let schema = DatasetSchema {
        dataset_id,
        fields: req.fields,
        geometry_rules: req.geometry_rules.unwrap_or(GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        }),
    };
    store.set_dataset_schema(&schema).await?;
    Ok(StatusCode::OK)
}

// ─── Topology Rules ─────────────────────────────────────────────────

async fn list_topology_rules(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<TopologyRule>>, QualityError> {
    let rules = store.list_topology_rules(dataset_id).await?;
    Ok(Json(rules))
}

#[derive(Deserialize)]
struct AddTopologyRuleRequest {
    rule_type: TopologyRuleType,
    #[serde(default)]
    description: String,
}

async fn add_topology_rule(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<AddTopologyRuleRequest>,
) -> Result<(StatusCode, Json<TopologyRule>), QualityError> {
    let rule = TopologyRule {
        id: Uuid::now_v7(),
        dataset_id,
        rule_type: req.rule_type,
        description: req.description,
    };
    store.add_topology_rule(&rule).await?;
    Ok((StatusCode::CREATED, Json(rule)))
}

async fn delete_topology_rule(
    State(store): State<AppState>,
    Path(rule_id): Path<Uuid>,
) -> Result<StatusCode, QualityError> {
    store.delete_topology_rule(rule_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Quality ────────────────────────────────────────────────────────

async fn quality_report(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<QualityReport>, QualityError> {
    let report = store.quality_report(branch_id).await?;
    Ok(Json(report))
}

#[derive(serde::Serialize)]
struct RepairResponse {
    repaired: bool,
    features_fixed: usize,
    changeset_id: Option<Uuid>,
}

async fn repair_geometries(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<RepairResponse>, QualityError> {
    let result = store.repair_geometries(branch_id, "system").await?;
    match result {
        Some(cs) => Ok(Json(RepairResponse {
            repaired: true,
            features_fixed: 1, // simplified; in practice would track count
            changeset_id: Some(cs.id),
        })),
        None => Ok(Json(RepairResponse {
            repaired: false,
            features_fixed: 0,
            changeset_id: None,
        })),
    }
}

// ─── Error Handling ─────────────────────────────────────────────────

enum QualityError {
    Store(ptolemy_storage::StoreError),
}

impl From<ptolemy_storage::StoreError> for QualityError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        QualityError::Store(e)
    }
}

impl IntoResponse for QualityError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            QualityError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            QualityError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            QualityError::Store(ptolemy_storage::StoreError::Db(e)) => {
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
