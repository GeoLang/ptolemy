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
use ptolemy_core::dataset::{Dataset, GeometryType, Visibility};
use ptolemy_core::diff::DiffOp;
use ptolemy_core::external::ExternalTable;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn v1_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/healthz", get(liveness))
        .route("/readyz", get(readiness))
        // Datasets
        .route("/datasets", get(list_datasets).post(create_dataset))
        .route(
            "/datasets/{id}",
            get(get_dataset).patch(update_dataset_visibility),
        )
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
        // Temporal query
        .route("/branches/{id}/features/at", get(features_at_time))
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
        // Topology-aware merge
        .route(
            "/branches/{target_id}/merge/{source_id}/topology",
            post(merge_with_topology),
        )
        // Diff
        .route("/diff/{from_id}/{to_id}", get(diff_changesets))
}

// ─── Health ─────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

/// Liveness probe — always returns 200 if the process is running.
async fn liveness() -> &'static str {
    "ok"
}

/// Readiness probe — checks database connectivity.
async fn readiness(State(state): State<AppState>) -> (axum::http::StatusCode, &'static str) {
    match sqlx::query("SELECT 1").execute(state.pool()).await {
        Ok(_) => (axum::http::StatusCode::OK, "ready"),
        Err(_) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready"),
    }
}

// ─── Datasets ───────────────────────────────────────────────────────

async fn list_datasets(
    State(store): State<AppState>,
    actor: Actor,
) -> Result<Json<Vec<Dataset>>, AppError> {
    let datasets = store.list_datasets(&actor.reader()).await?;
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
    /// Set all three to register a read-only view over an existing PostGIS
    /// relation instead of an empty versioned dataset.
    #[serde(default)]
    external_table: Option<String>,
    #[serde(default)]
    external_id_column: Option<String>,
    #[serde(default)]
    external_geometry_column: Option<String>,
    /// `public` (default) keeps anonymous reads; `private` limits reads to
    /// callers holding a permission row on the dataset or one of its branches.
    #[serde(default)]
    visibility: Option<String>,
}

impl CreateDatasetRequest {
    /// The three external fields only mean anything together, so a partial set
    /// is a request error rather than a silently ordinary dataset.
    fn external(&self) -> Result<Option<ExternalTable>, AppError> {
        match (
            &self.external_table,
            &self.external_id_column,
            &self.external_geometry_column,
        ) {
            (None, None, None) => Ok(None),
            (Some(table), Some(id), Some(geom)) => ExternalTable::parse(table, id, geom)
                .map(Some)
                .map_err(|e| AppError::BadRequest(e.to_string())),
            _ => Err(AppError::BadRequest(
                "external_table, external_id_column and external_geometry_column must be set together".into(),
            )),
        }
    }
}

fn default_srid() -> i32 {
    4326
}

async fn create_dataset(
    State(store): State<AppState>,
    actor: Actor,
    Json(req): Json<CreateDatasetRequest>,
) -> Result<(StatusCode, Json<Dataset>), AppError> {
    let external = req.external()?;
    let geom_type = req.geometry_type.as_deref().unwrap_or("point");
    let visibility = parse_visibility(req.visibility.as_deref())?;
    let ds = Dataset {
        id: Uuid::now_v7(),
        name: req.name,
        srid: req.srid,
        geometry_type: parse_geometry_type(geom_type),
        created_at: OffsetDateTime::now_utc(),
        created_by: actor.or_body(&req.created_by).to_string(),
        external,
        visibility,
    };
    // with auth on the creator gets an admin permission row, which also flips
    // the dataset to enforced: from here on only granted users may write to it
    let creator = actor.enforced_id();
    // registering probes the relation and creates the main branch, so the
    // dataset is browsable the moment the call returns
    let ds = if ds.external.is_some() {
        // at registration every rejection is about the request, so report 400
        // rather than the 409 the read-only guard uses
        store
            .register_external_dataset(&ds, creator)
            .await
            .map_err(|e| match e {
                ptolemy_storage::StoreError::Conflict(msg) => AppError::BadRequest(msg),
                other => AppError::Store(other),
            })?
    } else {
        store.create_dataset(&ds, creator).await?;
        ds
    };
    Ok((StatusCode::CREATED, Json(ds)))
}

async fn get_dataset(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Dataset>, AppError> {
    let ds = store.get_dataset(id).await?;
    Ok(Json(ds))
}

#[derive(Deserialize)]
struct UpdateDatasetRequest {
    visibility: String,
}

/// Flipping a dataset between public and private is a dataset-admin operation,
/// not an ordinary write: an editor with a write grant must not be able to
/// publish data someone else marked private.
async fn update_dataset_visibility(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    actor: Actor,
    Json(req): Json<UpdateDatasetRequest>,
) -> Result<Json<Dataset>, AppError> {
    let visibility = parse_visibility(Some(&req.visibility))?;

    if let Some(user_id) = actor.enforced_id()
        && !actor.is_instance_admin()
        && !store.is_dataset_admin(id, user_id).await?
    {
        return Err(AppError::Store(ptolemy_storage::StoreError::Forbidden(
            format!("changing visibility of dataset {id} needs an admin grant on it"),
        )));
    }

    let ds = store.set_dataset_visibility(id, visibility).await?;
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
    actor: Actor,
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
        created_by: actor.or_body(&req.created_by).to_string(),
    };
    store.create_branch(&branch, &actor.writer()).await?;
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

// ─── Temporal Query ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct TemporalParams {
    /// ISO 8601 timestamp to query features "as of"
    at: String,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

async fn features_at_time(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(params): Query<TemporalParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let at =
        time::OffsetDateTime::parse(&params.at, &time::format_description::well_known::Rfc3339)
            .map_err(|e| AppError::BadRequest(format!("invalid timestamp (use RFC 3339): {e}")))?;

    let limit = params.limit.clamp(1, 10000);
    let features = store
        .features_at_time(branch_id, at, limit, params.offset)
        .await?;

    Ok(Json(serde_json::json!({
        "branch_id": branch_id,
        "at": params.at,
        "features": features,
        "count": features.len(),
    })))
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
    actor: Actor,
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
        .commit(
            branch_id,
            &req.message,
            actor.or_body(&req.author),
            &ops,
            &actor.writer(),
        )
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
    actor: Actor,
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
        .commit(
            branch_id,
            &req.message,
            actor.or_body(&req.author),
            &ops,
            &actor.writer(),
        )
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
    actor: Actor,
) -> Result<Json<MergeResponse>, AppError> {
    let result = store
        .merge(source_id, target_id, actor.or_body("api"), &actor.writer())
        .await?;
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

// ─── Topology-Aware Merge ───────────────────────────────────────────

#[derive(Deserialize)]
struct TopologyMergeParams {
    #[serde(default = "default_true")]
    auto_repair: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum TopologyMergeResponse {
    Success {
        changeset: ptolemy_core::changeset::Changeset,
        auto_repaired: Vec<ptolemy_storage::TopologyRepair>,
    },
    MergeConflicts {
        conflicts: Vec<ConflictResponse>,
    },
    TopologyViolations {
        changeset: ptolemy_core::changeset::Changeset,
        violations: Vec<ptolemy_storage::TopologyViolation>,
        auto_repaired: Vec<ptolemy_storage::TopologyRepair>,
    },
}

async fn merge_with_topology(
    State(store): State<AppState>,
    Path((target_id, source_id)): Path<(Uuid, Uuid)>,
    Query(params): Query<TopologyMergeParams>,
    actor: Actor,
) -> Result<Json<TopologyMergeResponse>, AppError> {
    let result = store
        .merge_with_topology(
            source_id,
            target_id,
            actor.or_body("api"),
            params.auto_repair,
            &actor.writer(),
        )
        .await?;

    match result {
        ptolemy_storage::TopologyMergeResult::Success {
            changeset,
            auto_repaired,
            ..
        } => Ok(Json(TopologyMergeResponse::Success {
            changeset,
            auto_repaired,
        })),
        ptolemy_storage::TopologyMergeResult::MergeConflicts(conflicts) => {
            let resp: Vec<ConflictResponse> = conflicts
                .into_iter()
                .map(|c| ConflictResponse {
                    feature_id: c.feature_id,
                    ours: format!("{:?}", c.ours),
                    theirs: format!("{:?}", c.theirs),
                })
                .collect();
            Ok(Json(TopologyMergeResponse::MergeConflicts {
                conflicts: resp,
            }))
        }
        ptolemy_storage::TopologyMergeResult::TopologyViolations {
            changeset,
            violations,
            auto_repaired,
        } => Ok(Json(TopologyMergeResponse::TopologyViolations {
            changeset,
            violations,
            auto_repaired,
        })),
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
            AppError::Store(e) => crate::errors::store_error_status(&e),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Absent means public, matching the column default.
fn parse_visibility(s: Option<&str>) -> Result<Visibility, AppError> {
    match s {
        None => Ok(Visibility::Public),
        Some(v) => Visibility::parse(v).ok_or_else(|| {
            AppError::BadRequest(format!(
                "invalid visibility: '{v}'. Must be 'public' or 'private'"
            ))
        }),
    }
}

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
