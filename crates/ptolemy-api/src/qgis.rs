// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! QGIS plugin integration endpoints.
//!
//! Provides a discovery/capabilities endpoint and a WFS-T-compatible
//! transaction interface so QGIS can natively sync changes via toolbar buttons.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_storage::WriteGrant;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn qgis_routes() -> Router<AppState> {
    Router::new()
        // Discovery & capabilities
        .route("/qgis/capabilities", get(capabilities))
        .route("/qgis/datasets", get(list_datasets_for_qgis))
        .route("/qgis/branches/{branch_id}/layer", get(layer_definition))
        // WFS-T compatible transaction endpoint
        .route(
            "/qgis/branches/{branch_id}/transaction",
            post(wfs_transaction),
        )
        // QGIS-friendly sync (simplified pull/push)
        .route("/qgis/branches/{branch_id}/sync", get(qgis_pull))
        .route("/qgis/branches/{branch_id}/sync", post(qgis_push))
        // Conflict handling
        .route("/qgis/branches/{branch_id}/conflicts", get(list_conflicts))
        .route(
            "/qgis/branches/{branch_id}/conflicts/resolve",
            post(resolve_conflict),
        )
}

// ─── Capabilities ───────────────────────────────────────────────────

#[derive(Serialize)]
struct Capabilities {
    service: &'static str,
    version: &'static str,
    protocol: &'static str,
    operations: Vec<&'static str>,
    formats: Vec<&'static str>,
    crs: Vec<&'static str>,
    supports_versioning: bool,
    supports_branching: bool,
    supports_offline_sync: bool,
    supports_conflict_resolution: bool,
    supports_topology_rules: bool,
    sync_endpoint: &'static str,
    wfs_t_endpoint: &'static str,
}

async fn capabilities() -> Json<Capabilities> {
    Json(Capabilities {
        service: "Ptolemy Versioned GIS",
        version: env!("CARGO_PKG_VERSION"),
        protocol: "ptolemy-sync/1.0",
        operations: vec![
            "GetCapabilities",
            "DescribeFeatureType",
            "GetFeature",
            "Transaction",
            "LockFeature",
            "Pull",
            "Push",
            "ListConflicts",
            "ResolveConflict",
        ],
        formats: vec![
            "application/geo+json",
            "application/vnd.ogc.wfs_xml",
            "application/x-wkb",
            "text/csv",
            "application/flatgeobuf",
        ],
        crs: vec!["EPSG:4326", "EPSG:3857", "EPSG:*"],
        supports_versioning: true,
        supports_branching: true,
        supports_offline_sync: true,
        supports_conflict_resolution: true,
        supports_topology_rules: true,
        sync_endpoint: "/api/v1/qgis/branches/{branch_id}/sync",
        wfs_t_endpoint: "/api/v1/qgis/branches/{branch_id}/transaction",
    })
}

// ─── Dataset listing for QGIS ───────────────────────────────────────

#[derive(Serialize)]
struct QgisDataset {
    id: Uuid,
    name: String,
    srid: i32,
    geometry_type: String,
    branches: Vec<QgisBranch>,
}

#[derive(Serialize)]
struct QgisBranch {
    id: Uuid,
    name: String,
    head: Option<Uuid>,
}

async fn list_datasets_for_qgis(
    State(store): State<AppState>,
    actor: Actor,
) -> Result<Json<Vec<QgisDataset>>, QgisError> {
    let reader = actor.reader();
    let visible = ptolemy_storage::visible_datasets_sql("d", 1, 2);
    let rows = sqlx::query(&format!(
        "SELECT d.id, d.name, d.srid, d.geometry_type,
                b.id as branch_id, b.name as branch_name, b.head
         FROM datasets d
         LEFT JOIN branches b ON b.dataset_id = d.id
         WHERE {visible}
         ORDER BY d.name, b.name"
    ))
    .bind(reader.bypass)
    .bind(reader.id.as_deref())
    .fetch_all(store.read_pool())
    .await?;

    let mut datasets: Vec<QgisDataset> = Vec::new();
    let mut current_ds_id: Option<Uuid> = None;

    for row in &rows {
        let ds_id: Uuid = row.get("id");
        if current_ds_id != Some(ds_id) {
            datasets.push(QgisDataset {
                id: ds_id,
                name: row.get("name"),
                srid: row.get("srid"),
                geometry_type: row.get("geometry_type"),
                branches: Vec::new(),
            });
            current_ds_id = Some(ds_id);
        }
        if let Some(branch_id) = row.get::<Option<Uuid>, _>("branch_id")
            && let Some(ds) = datasets.last_mut()
        {
            ds.branches.push(QgisBranch {
                id: branch_id,
                name: row.get("branch_name"),
                head: row.get("head"),
            });
        }
    }

    Ok(Json(datasets))
}

// ─── Layer Definition ───────────────────────────────────────────────

#[derive(Serialize)]
struct LayerDefinition {
    branch_id: Uuid,
    dataset_name: String,
    geometry_type: String,
    srid: i32,
    fields: Vec<LayerField>,
    feature_count: i64,
    extent: Option<Extent>,
}

#[derive(Serialize)]
struct LayerField {
    name: String,
    field_type: String,
}

#[derive(Serialize)]
struct Extent {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

async fn layer_definition(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<LayerDefinition>, QgisError> {
    // Get branch + dataset info
    let row = sqlx::query(
        "SELECT b.id, d.name, d.srid, d.geometry_type
         FROM branches b
         JOIN datasets d ON d.id = b.dataset_id
         WHERE b.id = $1",
    )
    .bind(branch_id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(QgisError::NotFound)?;

    let dataset_name: String = row.get("name");
    let srid: i32 = row.get("srid");
    let geometry_type: String = row.get("geometry_type");

    // Count features and compute extent. The "features" view already resolves the
    // latest live version per feature from the branch head's ancestor chain.
    let (external, source) = store.features_source(branch_id).await?;
    let pool = store.source_pool(external.as_ref()).await?;
    let stats = sqlx::query(&format!(
        "SELECT count(*) as cnt,
                ST_XMin(ST_Extent(geometry)) as min_x,
                ST_YMin(ST_Extent(geometry)) as min_y,
                ST_XMax(ST_Extent(geometry)) as max_x,
                ST_YMax(ST_Extent(geometry)) as max_y
         FROM {source} f
         WHERE branch_id = $1"
    ))
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    let feature_count: i64 = stats.get("cnt");
    let extent = stats.get::<Option<f64>, _>("min_x").map(|min_x| Extent {
        min_x,
        min_y: stats.get("min_y"),
        max_x: stats.get("max_x"),
        max_y: stats.get("max_y"),
    });

    // Get field names from properties of first feature
    let fields = sqlx::query(&format!(
        "SELECT DISTINCT jsonb_object_keys(properties) as key
         FROM {source} f WHERE branch_id = $1
         LIMIT 100"
    ))
    .bind(branch_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| LayerField {
        name: r.get("key"),
        field_type: "string".into(), // QGIS will infer actual types
    })
    .collect();

    Ok(Json(LayerDefinition {
        branch_id,
        dataset_name,
        geometry_type,
        srid,
        fields,
        feature_count,
        extent,
    }))
}

// ─── WFS-T Transaction ──────────────────────────────────────────────

#[derive(Deserialize)]
struct WfsTransaction {
    /// Operations in this transaction
    operations: Vec<WfsOperation>,
    /// Commit message for the changeset
    #[serde(default = "default_message")]
    message: String,
    /// Author name
    author: String,
    /// Handle to reference in response
    #[serde(default)]
    handle: Option<String>,
}

fn default_message() -> String {
    "QGIS transaction".into()
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WfsOperation {
    Insert {
        feature_id: Option<Uuid>,
        geometry_wkb_hex: String,
        properties: serde_json::Value,
    },
    Update {
        feature_id: Uuid,
        #[serde(skip_serializing_if = "Option::is_none")]
        geometry_wkb_hex: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        properties: Option<serde_json::Value>,
    },
    Delete {
        feature_id: Uuid,
    },
    /// Lock a feature for editing
    Lock {
        feature_id: Uuid,
    },
    /// Release a lock
    Unlock {
        feature_id: Uuid,
    },
}

#[derive(Serialize)]
struct WfsTransactionResponse {
    success: bool,
    changeset_id: Option<Uuid>,
    total_inserted: usize,
    total_updated: usize,
    total_deleted: usize,
    handle: Option<String>,
    errors: Vec<String>,
}

async fn wfs_transaction(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    actor: Actor,
    Json(req): Json<WfsTransaction>,
) -> Result<Json<WfsTransactionResponse>, QgisError> {
    store
        .ensure_branch_writable(branch_id, &actor.writer())
        .await?;
    let mut diff_ops: Vec<ptolemy_core::diff::DiffOp> = Vec::new();
    let mut inserted = 0usize;
    let mut updated = 0usize;
    let mut deleted = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for op in &req.operations {
        match op {
            WfsOperation::Insert {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let fid = feature_id.unwrap_or_else(Uuid::now_v7);
                match hex::decode(geometry_wkb_hex) {
                    Ok(wkb) => {
                        diff_ops.push(ptolemy_core::diff::DiffOp::Insert {
                            feature_id: fid,
                            geometry_wkb: wkb,
                            properties: properties.clone(),
                            native: None,
                            valid_from: None,
                            valid_to: None,
                        });
                        inserted += 1;
                    }
                    Err(e) => errors.push(format!("insert {fid}: invalid hex: {e}")),
                }
            }
            WfsOperation::Update {
                feature_id,
                geometry_wkb_hex,
                properties,
            } => {
                let wkb = geometry_wkb_hex.as_ref().map(hex::decode).transpose();
                match wkb {
                    Ok(w) => {
                        diff_ops.push(ptolemy_core::diff::DiffOp::Update {
                            feature_id: *feature_id,
                            geometry_wkb: w,
                            properties: properties.clone(),
                            native: None,
                            valid_from: None,
                            valid_to: None,
                        });
                        updated += 1;
                    }
                    Err(e) => errors.push(format!("update {feature_id}: invalid hex: {e}")),
                }
            }
            WfsOperation::Delete { feature_id } => {
                diff_ops.push(ptolemy_core::diff::DiffOp::Delete {
                    feature_id: *feature_id,
                });
                deleted += 1;
            }
            WfsOperation::Lock { .. } | WfsOperation::Unlock { .. } => {
                // Lock/unlock handled separately via lock endpoints
            }
        }
    }

    if !errors.is_empty() {
        return Ok(Json(WfsTransactionResponse {
            success: false,
            changeset_id: None,
            total_inserted: 0,
            total_updated: 0,
            total_deleted: 0,
            handle: req.handle,
            errors,
        }));
    }

    let changeset = store
        .commit(
            branch_id,
            &req.message,
            actor.or_body(&req.author),
            &diff_ops,
            &actor.writer(),
        )
        .await?;

    Ok(Json(WfsTransactionResponse {
        success: true,
        changeset_id: Some(changeset.id),
        total_inserted: inserted,
        total_updated: updated,
        total_deleted: deleted,
        handle: req.handle,
        errors: vec![],
    }))
}

// ─── QGIS Pull/Push (simplified sync) ──────────────────────────────

#[derive(Deserialize)]
struct QgisPullParams {
    /// Client's last known changeset (for incremental sync)
    #[serde(default)]
    since: Option<Uuid>,
    /// Max features per page
    #[serde(default = "default_page_size")]
    limit: i64,
}

fn default_page_size() -> i64 {
    5000
}

#[derive(Serialize)]
struct QgisPullResponse {
    branch_id: Uuid,
    head: Option<Uuid>,
    /// GeoJSON FeatureCollection for full sync or changes
    geojson: serde_json::Value,
    /// If true, client is fully up to date
    up_to_date: bool,
}

async fn qgis_pull(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(params): Query<QgisPullParams>,
) -> Result<Json<QgisPullResponse>, QgisError> {
    let branch = store.get_branch(branch_id).await?;

    // Check if already up to date
    if params.since == branch.head {
        return Ok(Json(QgisPullResponse {
            branch_id,
            head: branch.head,
            geojson: serde_json::json!({"type": "FeatureCollection", "features": []}),
            up_to_date: true,
        }));
    }

    // Fetch features as GeoJSON
    let limit = params.limit.clamp(1, 50000);
    // resolve versions from the branch head's changeset ancestor chain, so forks
    // see inherited features; a feature whose latest version is a delete drops out
    let rows = sqlx::query(
        "WITH RECURSIVE chain AS (
            SELECT c.id, c.parent_id
            FROM changesets c
            JOIN branches b ON b.head = c.id
            WHERE b.id = $1
          UNION ALL
            SELECT c.id, c.parent_id
            FROM changesets c
            JOIN chain ch ON ch.parent_id = c.id
        ),
        latest AS (
            SELECT DISTINCT ON (fv.feature_id)
                fv.id, fv.feature_id, fv.operation, fv.geometry, fv.properties, fv.created_at
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
        )
        SELECT feature_id, ST_AsGeoJSON(geometry)::jsonb as geojson, properties
        FROM latest
        WHERE operation != 'delete'
        ORDER BY created_at DESC, id DESC
        LIMIT $2",
    )
    .bind(branch_id)
    .bind(limit)
    .fetch_all(store.read_pool())
    .await?;

    let features: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": r.get::<serde_json::Value, _>("properties"),
            })
        })
        .collect();

    Ok(Json(QgisPullResponse {
        branch_id,
        head: branch.head,
        geojson: serde_json::json!({
            "type": "FeatureCollection",
            "features": features,
        }),
        up_to_date: false,
    }))
}

#[derive(Deserialize)]
struct QgisPushRequest {
    /// The changeset the client was synced to
    base_changeset: Option<Uuid>,
    /// GeoJSON FeatureCollection of edited features
    geojson: serde_json::Value,
    /// Author
    author: String,
    #[serde(default = "default_message")]
    message: String,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QgisPushResponse {
    Success { changeset_id: Uuid, head: Uuid },
    Conflict { current_head: Uuid, conflicts: i64 },
}

async fn qgis_push(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    actor: Actor,
    Json(req): Json<QgisPushRequest>,
) -> Result<(StatusCode, Json<QgisPushResponse>), QgisError> {
    store
        .ensure_branch_writable(branch_id, &actor.writer())
        .await?;
    let branch = store.get_branch(branch_id).await?;

    // Check if behind
    if let (Some(base), Some(head)) = (req.base_changeset, branch.head)
        && base != head
    {
        return Ok((
            StatusCode::CONFLICT,
            Json(QgisPushResponse::Conflict {
                current_head: head,
                conflicts: 0,
            }),
        ));
    }

    // Parse GeoJSON features into DiffOps
    let features = req.geojson["features"]
        .as_array()
        .ok_or_else(|| QgisError::Bad("geojson.features must be an array".into()))?;

    let mut ops: Vec<ptolemy_core::diff::DiffOp> = Vec::new();
    // built once: the same branch for every feature in the push
    let (_, live) = store.features_source_at(branch_id, "$2").await?;

    for feat in features {
        let fid_str = feat["id"]
            .as_str()
            .ok_or_else(|| QgisError::Bad("each feature must have an 'id' field".into()))?;
        let fid: Uuid = fid_str
            .parse()
            .map_err(|_| QgisError::Bad(format!("invalid feature id: {fid_str}")))?;

        let geom = &feat["geometry"];
        let geom_str = serde_json::to_string(geom)
            .map_err(|e| QgisError::Bad(format!("invalid geometry: {e}")))?;

        // Convert GeoJSON geometry to WKB via PostGIS
        let row = sqlx::query("SELECT ST_AsBinary(ST_GeomFromGeoJSON($1)) as wkb")
            .bind(&geom_str)
            .fetch_one(store.read_pool())
            .await?;
        let wkb: Vec<u8> = row.get("wkb");

        let properties = feat["properties"].clone();

        // Check if the feature is live on this branch (update) or is new (insert).
        // A feature whose latest version is a delete counts as new again.
        let exists = sqlx::query(&format!("SELECT 1 FROM {live} f WHERE f.id = $1 LIMIT 1"))
            .bind(fid)
            .bind(branch_id)
            .fetch_optional(store.read_pool())
            .await?;

        if exists.is_some() {
            ops.push(ptolemy_core::diff::DiffOp::Update {
                feature_id: fid,
                geometry_wkb: Some(wkb),
                properties: Some(properties),
                native: None,
                valid_from: None,
                valid_to: None,
            });
        } else {
            ops.push(ptolemy_core::diff::DiffOp::Insert {
                feature_id: fid,
                geometry_wkb: wkb,
                properties,
                native: None,
                valid_from: None,
                valid_to: None,
            });
        }
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

    Ok((
        StatusCode::CREATED,
        Json(QgisPushResponse::Success {
            changeset_id: changeset.id,
            head: changeset.id,
        }),
    ))
}

// ─── Conflict Listing & Resolution ─────────────────────────────────

async fn list_conflicts(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, QgisError> {
    // List unresolved conflicts for this branch
    let rows = sqlx::query(
        "SELECT id, feature_id, ours_geojson, theirs_geojson, ours_properties, theirs_properties,
                created_at, resolved
         FROM merge_conflicts
         WHERE branch_id = $1 AND resolved = false
         ORDER BY created_at DESC",
    )
    .bind(branch_id)
    .fetch_all(store.read_pool())
    .await;

    match rows {
        Ok(rows) => {
            let conflicts: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.get::<Uuid, _>("id"),
                        "feature_id": r.get::<Uuid, _>("feature_id"),
                        "ours": {
                            "geometry": r.get::<serde_json::Value, _>("ours_geojson"),
                            "properties": r.get::<serde_json::Value, _>("ours_properties"),
                        },
                        "theirs": {
                            "geometry": r.get::<serde_json::Value, _>("theirs_geojson"),
                            "properties": r.get::<serde_json::Value, _>("theirs_properties"),
                        },
                        "created_at": r.get::<time::OffsetDateTime, _>("created_at").to_string(),
                    })
                })
                .collect();
            Ok(Json(serde_json::json!({"conflicts": conflicts})))
        }
        Err(_) => {
            // Table might not exist yet
            Ok(Json(serde_json::json!({"conflicts": []})))
        }
    }
}

#[derive(Deserialize)]
struct ResolveConflictRequest {
    conflict_id: Uuid,
    /// Which version to keep: "ours", "theirs", or "custom"
    resolution: String,
    /// If resolution is "custom", provide the merged geometry & properties
    #[serde(default)]
    custom_geometry_wkb_hex: Option<String>,
    #[serde(default)]
    custom_properties: Option<serde_json::Value>,
}

async fn resolve_conflict(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    actor: Actor,
    Json(req): Json<ResolveConflictRequest>,
) -> Result<Json<serde_json::Value>, QgisError> {
    let branch_id = grant.id();
    // marking a conflict resolved is a write in its own right, and it runs
    // before the commit below that would otherwise be the only guard
    store
        .ensure_branch_writable(branch_id, &actor.writer())
        .await?;
    let result = store
        .resolve_merge_conflict(&grant, req.conflict_id, &req.resolution)
        .await;

    match result {
        Ok(Some(feature_id)) => {
            // If custom resolution, apply the edit
            if req.resolution == "custom"
                && let Some(hex) = &req.custom_geometry_wkb_hex
            {
                let wkb =
                    hex::decode(hex).map_err(|e| QgisError::Bad(format!("invalid hex: {e}")))?;
                let ops = vec![ptolemy_core::diff::DiffOp::Update {
                    feature_id,
                    geometry_wkb: Some(wkb),
                    properties: req.custom_properties,
                    native: None,
                    valid_from: None,
                    valid_to: None,
                }];
                store
                    .commit(
                        branch_id,
                        "resolve conflict (custom)",
                        actor.or_body("system"),
                        &ops,
                        &actor.writer(),
                    )
                    .await?;
            }

            Ok(Json(serde_json::json!({
                "resolved": true,
                "feature_id": feature_id,
                "resolution": req.resolution,
            })))
        }
        Ok(None) => Err(QgisError::NotFound),
        Err(_) => Ok(Json(serde_json::json!({
            "resolved": false,
            "error": "merge_conflicts table not available"
        }))),
    }
}

// ─── Error Handling ─────────────────────────────────────────────────

enum QgisError {
    Store(ptolemy_storage::StoreError),
    Db(sqlx::Error),
    NotFound,
    Bad(String),
}

impl From<ptolemy_storage::StoreError> for QgisError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        QgisError::Store(e)
    }
}

impl From<sqlx::Error> for QgisError {
    fn from(e: sqlx::Error) -> Self {
        QgisError::Db(e)
    }
}

impl IntoResponse for QgisError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            QgisError::Store(ptolemy_storage::StoreError::Conflict(m)) => (StatusCode::CONFLICT, m),
            QgisError::Store(ptolemy_storage::StoreError::NotFound(m)) => {
                (StatusCode::NOT_FOUND, m)
            }
            // database errors are logged, never echoed: they can carry
            // connection details and schema internals
            QgisError::Store(ptolemy_storage::StoreError::Db(e)) => {
                crate::errors::log_db_error("qgis store", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
            QgisError::Store(e) => {
                tracing::error!("Store error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
            QgisError::Db(e) => {
                crate::errors::log_db_error("qgis", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
            QgisError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            QgisError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": msg}))).into_response()
    }
}
