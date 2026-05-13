// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ptolemy_core::branch::Branch;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::DiffOp;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;

pub fn v1_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        // Datasets
        .route("/datasets", get(list_datasets).post(create_dataset))
        .route("/datasets/{id}", get(get_dataset))
        // Branches
        .route(
            "/datasets/{dataset_id}/branches",
            get(list_branches).post(create_branch),
        )
        .route("/branches/{id}", get(get_branch))
        .route("/branches/{id}/history", get(get_branch_history))
        .route("/branches/{id}/features", get(list_features))
        // Spatial queries
        .route("/branches/{id}/features/bbox", get(features_bbox))
        .route(
            "/branches/{id}/features/intersects",
            post(features_intersects),
        )
        .route("/branches/{id}/features/within", post(features_within))
        .route("/branches/{id}/features/count", get(features_count))
        // MVT tiles
        .route("/branches/{id}/tiles/{z}/{x}/{y}", get(mvt_tile))
        // Commits
        .route("/branches/{id}/commit", post(commit))
        .route("/branches/{id}/batch", post(batch_commit))
        // Merge
        .route(
            "/branches/{target_id}/merge/{source_id}",
            post(merge_branches),
        )
        // Diff
        .route("/diff/{from_id}/{to_id}", get(diff_changesets))
}

// ─── Health ─────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

// ─── Datasets ───────────────────────────────────────────────────────

async fn list_datasets(State(store): State<AppState>) -> Result<Json<Vec<Dataset>>, AppError> {
    let datasets = store.list_datasets().await?;
    Ok(Json(datasets))
}

#[derive(Deserialize)]
struct CreateDatasetRequest {
    name: String,
    #[serde(default = "default_srid")]
    srid: i32,
    #[serde(default)]
    geometry_type: Option<String>,
    created_by: String,
}

fn default_srid() -> i32 {
    4326
}

async fn create_dataset(
    State(store): State<AppState>,
    Json(req): Json<CreateDatasetRequest>,
) -> Result<(StatusCode, Json<Dataset>), AppError> {
    let geom_type = req.geometry_type.as_deref().unwrap_or("point");
    let ds = Dataset {
        id: Uuid::now_v7(),
        name: req.name,
        srid: req.srid,
        geometry_type: parse_geometry_type(geom_type),
        created_at: OffsetDateTime::now_utc(),
        created_by: req.created_by,
    };
    store.create_dataset(&ds).await?;
    Ok((StatusCode::CREATED, Json(ds)))
}

async fn get_dataset(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Dataset>, AppError> {
    let ds = store.get_dataset(id).await?;
    Ok(Json(ds))
}

// ─── Branches ───────────────────────────────────────────────────────

async fn list_branches(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Branch>>, AppError> {
    let branches = store.list_branches(dataset_id).await?;
    Ok(Json(branches))
}

#[derive(Deserialize)]
struct CreateBranchRequest {
    name: String,
    created_by: String,
    #[serde(default)]
    fork_from_branch: Option<Uuid>,
}

async fn create_branch(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<Branch>), AppError> {
    // If forking, copy the head from the source branch
    let head = if let Some(source_id) = req.fork_from_branch {
        let source = store.get_branch(source_id).await?;
        source.head
    } else {
        None
    };

    let branch = Branch {
        id: Uuid::now_v7(),
        dataset_id,
        name: req.name,
        head,
        created_at: OffsetDateTime::now_utc(),
        created_by: req.created_by,
    };
    store.create_branch(&branch).await?;
    Ok((StatusCode::CREATED, Json(branch)))
}

async fn get_branch(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Branch>, AppError> {
    let branch = store.get_branch(id).await?;
    Ok(Json(branch))
}

async fn get_branch_history(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ptolemy_core::changeset::Changeset>>, AppError> {
    let history = store.get_branch_history(id, 100).await?;
    Ok(Json(history))
}

// ─── Features ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FeatureListParams {
    /// Cursor for pagination (feature UUID)
    #[serde(default)]
    cursor: Option<Uuid>,
    /// Page size (default 100, max 10000)
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    100
}

async fn list_features(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<FeatureListParams>,
) -> Result<Json<FeaturePage>, AppError> {
    let limit = params.limit.clamp(1, 10000);
    let features = store
        .list_features_paginated(id, params.cursor, limit)
        .await?;
    let next_cursor = if features.len() as i64 == limit {
        features.last().map(|f| f.id)
    } else {
        None
    };
    Ok(Json(FeaturePage {
        features,
        next_cursor,
    }))
}

#[derive(Serialize)]
struct FeaturePage {
    features: Vec<ptolemy_core::Feature>,
    next_cursor: Option<Uuid>,
}

// ─── Spatial Queries ────────────────────────────────────────────────

#[derive(Deserialize)]
struct BboxParams {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn features_bbox(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(params): Query<BboxParams>,
) -> Result<Json<Vec<ptolemy_core::Feature>>, AppError> {
    let limit = params.limit.clamp(1, 10000);
    let features = store
        .features_in_bbox(
            branch_id,
            params.min_x,
            params.min_y,
            params.max_x,
            params.max_y,
            limit,
        )
        .await?;
    Ok(Json(features))
}

#[derive(Deserialize)]
struct SpatialFilterRequest {
    /// GeoJSON geometry to test against
    geometry: serde_json::Value,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn features_intersects(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SpatialFilterRequest>,
) -> Result<Json<Vec<ptolemy_core::Feature>>, AppError> {
    let geojson_str = serde_json::to_string(&req.geometry)
        .map_err(|e| AppError::BadRequest(format!("invalid geometry: {e}")))?;
    let limit = req.limit.clamp(1, 10000);
    let features = store
        .features_intersecting(branch_id, &geojson_str, limit)
        .await?;
    Ok(Json(features))
}

async fn features_within(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SpatialFilterRequest>,
) -> Result<Json<Vec<ptolemy_core::Feature>>, AppError> {
    let geojson_str = serde_json::to_string(&req.geometry)
        .map_err(|e| AppError::BadRequest(format!("invalid geometry: {e}")))?;
    let limit = req.limit.clamp(1, 10000);
    let features = store
        .features_within(branch_id, &geojson_str, limit)
        .await?;
    Ok(Json(features))
}

async fn features_count(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let count = store.count_features_at_head(branch_id).await?;
    Ok(Json(serde_json::json!({"count": count})))
}

// ─── MVT Tiles ──────────────────────────────────────────────────────

async fn mvt_tile(
    State(store): State<AppState>,
    Path((branch_id, z, x, y)): Path<(Uuid, u32, u32, u32)>,
) -> Result<Response, AppError> {
    let tile_data = store.get_mvt_tile(branch_id, z, x, y).await?;
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/vnd.mapbox-vector-tile"),
            ("cache-control", "public, max-age=300"),
        ],
        tile_data,
    )
        .into_response())
}

// ─── Batch Operations ───────────────────────────────────────────────

#[derive(Deserialize)]
struct BatchCommitRequest {
    message: String,
    author: String,
    operations: Vec<DiffOpRequest>,
}

async fn batch_commit(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<BatchCommitRequest>,
) -> Result<(StatusCode, Json<BatchCommitResponse>), AppError> {
    let ops: Result<Vec<DiffOp>, AppError> = req
        .operations
        .into_iter()
        .map(|op| match op {
            DiffOpRequest::Insert {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = hex::decode(&geometry_wkb_hex)
                    .map_err(|e| AppError::BadRequest(format!("invalid hex: {e}")))?;
                Ok(DiffOp::Insert {
                    feature_id: feature_id.unwrap_or_else(Uuid::now_v7),
                    geometry_wkb: wkb,
                    properties,
                })
            }
            DiffOpRequest::Update {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = geometry_wkb_hex
                    .map(|h| hex::decode(&h))
                    .transpose()
                    .map_err(|e| AppError::BadRequest(format!("invalid hex: {e}")))?;
                Ok(DiffOp::Update {
                    feature_id,
                    geometry_wkb: wkb,
                    properties,
                })
            }
            DiffOpRequest::Delete { feature_id } => Ok(DiffOp::Delete { feature_id }),
        })
        .collect();

    let ops = ops?;
    let op_count = ops.len();
    let changeset = store
        .commit(branch_id, &req.message, &req.author, &ops)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(BatchCommitResponse {
            changeset,
            operations_applied: op_count,
        }),
    ))
}

#[derive(Serialize)]
struct BatchCommitResponse {
    changeset: ptolemy_core::changeset::Changeset,
    operations_applied: usize,
}

// ─── Commit ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CommitRequest {
    message: String,
    author: String,
    operations: Vec<DiffOpRequest>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DiffOpRequest {
    Insert {
        feature_id: Option<Uuid>,
        geometry_wkb_hex: String,
        properties: serde_json::Value,
    },
    Update {
        feature_id: Uuid,
        #[serde(default)]
        geometry_wkb_hex: Option<String>,
        #[serde(default)]
        properties: Option<serde_json::Value>,
    },
    Delete {
        feature_id: Uuid,
    },
}

async fn commit(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<CommitRequest>,
) -> Result<(StatusCode, Json<ptolemy_core::changeset::Changeset>), AppError> {
    let ops: Result<Vec<DiffOp>, AppError> = req
        .operations
        .into_iter()
        .map(|op| match op {
            DiffOpRequest::Insert {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = hex::decode(&geometry_wkb_hex)
                    .map_err(|e| AppError::BadRequest(format!("invalid hex: {e}")))?;
                Ok(DiffOp::Insert {
                    feature_id: feature_id.unwrap_or_else(Uuid::now_v7),
                    geometry_wkb: wkb,
                    properties,
                })
            }
            DiffOpRequest::Update {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = geometry_wkb_hex
                    .map(|h| hex::decode(&h))
                    .transpose()
                    .map_err(|e| AppError::BadRequest(format!("invalid hex: {e}")))?;
                Ok(DiffOp::Update {
                    feature_id,
                    geometry_wkb: wkb,
                    properties,
                })
            }
            DiffOpRequest::Delete { feature_id } => Ok(DiffOp::Delete { feature_id }),
        })
        .collect();

    let ops = ops?;

    // Schema validation (if dataset has a schema defined)
    let branch = store.get_branch(branch_id).await?;
    let validation_errors = store.validate_commit(branch.dataset_id, &ops).await?;
    if !validation_errors.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Schema validation failed: {} error(s) — {}",
            validation_errors.len(),
            validation_errors
                .iter()
                .take(5)
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }

    let changeset = store
        .commit(branch_id, &req.message, &req.author, &ops)
        .await?;
    Ok((StatusCode::CREATED, Json(changeset)))
}

// ─── Merge ──────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum MergeResponse {
    Success {
        changeset: ptolemy_core::changeset::Changeset,
    },
    Conflicts {
        conflicts: Vec<ConflictResponse>,
    },
}

#[derive(Serialize)]
struct ConflictResponse {
    feature_id: Uuid,
    ours: String,
    theirs: String,
}

async fn merge_branches(
    State(store): State<AppState>,
    Path((target_id, source_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<MergeResponse>, AppError> {
    let result = store.merge(source_id, target_id, "api").await?;
    match result {
        ptolemy_storage::MergeResult::Success(changeset) => {
            Ok(Json(MergeResponse::Success { changeset }))
        }
        ptolemy_storage::MergeResult::Conflicts(conflicts) => {
            let resp: Vec<ConflictResponse> = conflicts
                .into_iter()
                .map(|c| ConflictResponse {
                    feature_id: c.feature_id,
                    ours: format!("{:?}", c.ours),
                    theirs: format!("{:?}", c.theirs),
                })
                .collect();
            Ok(Json(MergeResponse::Conflicts { conflicts: resp }))
        }
    }
}

// ─── Diff ───────────────────────────────────────────────────────────

async fn diff_changesets(
    State(store): State<AppState>,
    Path((from_id, to_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ptolemy_core::diff::Diff>, AppError> {
    let diff = store.diff(Some(from_id), to_id).await?;
    Ok(Json(diff))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum AppError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl From<ptolemy_storage::StoreError> for AppError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        AppError::Store(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            AppError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            AppError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            AppError::Store(ptolemy_storage::StoreError::Db(e)) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

fn parse_geometry_type(s: &str) -> GeometryType {
    match s {
        "point" => GeometryType::Point,
        "linestring" => GeometryType::LineString,
        "polygon" => GeometryType::Polygon,
        "multipoint" => GeometryType::MultiPoint,
        "multilinestring" => GeometryType::MultiLineString,
        "multipolygon" => GeometryType::MultiPolygon,
        "geometrycollection" => GeometryType::GeometryCollection,
        _ => GeometryType::Point,
    }
}
