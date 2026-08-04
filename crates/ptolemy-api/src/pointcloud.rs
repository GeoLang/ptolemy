// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Point cloud (LiDAR) management using the pointcloud extension.

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_storage::{WriteGrant, writes::PointCloudPatchInput};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn pointcloud_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/pointclouds",
            get(list_catalogs).post(create_catalog),
        )
        .route("/pointclouds/{id}", get(get_catalog))
        .route(
            "/pointclouds/{id}/patches",
            get(list_patches).post(add_patch),
        )
        .route("/pointclouds/{id}/query", post(spatial_query))
        .route("/pointclouds/{id}/stats", get(catalog_stats))
        .route("/pointclouds/{id}/profile", post(elevation_profile))
}

/// Migration 015 only gives `pointcloud_patches.pa` the `pcpatch` type where
/// the pointcloud extension is installed; without it the column is BYTEA and
/// the `PC_*` functions are undefined. The catalog and patch listings read
/// neither, so only the two routes that do are gated, and each gates after its
/// own ladder and argument checks so a 501 never stands in for a 403 or a 400.
async fn require_pointcloud(store: &AppState) -> Result<(), PcError> {
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pointcloud')",
    )
    .fetch_one(store.read_pool())
    .await?;
    if present {
        Ok(())
    } else {
        Err(PcError::NoPointCloud)
    }
}

#[derive(Serialize)]
struct PointCloudCatalog {
    id: Uuid,
    name: String,
    srid: i32,
    schema_xml: Option<String>,
    created_at: String,
}

async fn list_catalogs(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<PointCloudCatalog>>, PcError> {
    let rows = sqlx::query(
        "SELECT id, name, srid, schema_xml, created_at::text as ts
         FROM pointcloud_catalogs WHERE dataset_id = $1 ORDER BY name",
    )
    .bind(dataset_id)
    .fetch_all(store.read_pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| PointCloudCatalog {
                id: r.get("id"),
                name: r.get("name"),
                srid: r.get("srid"),
                schema_xml: r.get("schema_xml"),
                created_at: r.get("ts"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateCatalogRequest {
    name: String,
    #[serde(default = "default_srid")]
    srid: i32,
    schema_xml: Option<String>,
}
fn default_srid() -> i32 {
    4326
}

async fn create_catalog(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    actor: Actor,
    Json(req): Json<CreateCatalogRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), PcError> {
    // a catalog hangs off the dataset, so creating one is a dataset write
    store
        .ensure_dataset_writable(grant.id(), &actor.writer())
        .await?;
    let id = store
        .create_pointcloud_catalog(&grant, &req.name, req.srid, req.schema_xml.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_catalog(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PointCloudCatalog>, PcError> {
    let r = sqlx::query(
        "SELECT id, name, srid, schema_xml, created_at::text as ts
         FROM pointcloud_catalogs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(PcError::NotFound)?;
    Ok(Json(PointCloudCatalog {
        id: r.get("id"),
        name: r.get("name"),
        srid: r.get("srid"),
        schema_xml: r.get("schema_xml"),
        created_at: r.get("ts"),
    }))
}

#[derive(Serialize)]
struct PatchInfo {
    id: Uuid,
    num_points: i32,
    bounds: Option<serde_json::Value>,
}

async fn list_patches(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
) -> Result<Json<Vec<PatchInfo>>, PcError> {
    let rows = sqlx::query(
        "SELECT id, num_points, ST_AsGeoJSON(bounds)::jsonb as bounds_geojson
         FROM pointcloud_patches WHERE catalog_id = $1",
    )
    .bind(catalog_id)
    .fetch_all(store.read_pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| PatchInfo {
                id: r.get("id"),
                num_points: r.get("num_points"),
                bounds: r.get("bounds_geojson"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct AddPatchRequest {
    bounds_wkb_hex: String,
    num_points: i32,
    /// PC patch binary data as hex-encoded WKB (from pdal or pc_astext output)
    patch_hex: String,
}

/// The dataset a catalog hangs off, so a patch write runs the dataset's ladder.
async fn catalog_dataset(store: &AppState, catalog_id: Uuid) -> Result<Uuid, PcError> {
    sqlx::query_scalar("SELECT dataset_id FROM pointcloud_catalogs WHERE id = $1")
        .bind(catalog_id)
        .fetch_optional(store.read_pool())
        .await?
        .ok_or(PcError::NotFound)
}

async fn add_patch(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    actor: Actor,
    Json(req): Json<AddPatchRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), PcError> {
    let dataset_id = catalog_dataset(&store, grant.id()).await?;
    store
        .ensure_dataset_writable(dataset_id, &actor.writer())
        .await?;
    let wkb =
        hex::decode(&req.bounds_wkb_hex).map_err(|_| PcError::Bad("invalid bounds hex".into()))?;
    let patch_data =
        hex::decode(&req.patch_hex).map_err(|_| PcError::Bad("invalid patch hex".into()))?;
    require_pointcloud(&store).await?;
    let id = store
        .add_pointcloud_patch(
            &grant,
            &PointCloudPatchInput {
                bounds_wkb: &wkb,
                num_points: req.num_points,
                patch: &patch_data,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

/// Spatial query: find patches within a bounding box.
#[derive(Deserialize)]
struct SpatialQueryRequest {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

async fn spatial_query(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
    Json(req): Json<SpatialQueryRequest>,
) -> Result<Json<serde_json::Value>, PcError> {
    let rows = sqlx::query(
        "SELECT id, num_points, ST_AsGeoJSON(bounds)::jsonb as bounds_geojson
         FROM pointcloud_patches
         WHERE catalog_id = $1
           AND bounds && ST_MakeEnvelope($2, $3, $4, $5, 4326)",
    )
    .bind(catalog_id)
    .bind(req.min_x)
    .bind(req.min_y)
    .bind(req.max_x)
    .bind(req.max_y)
    .fetch_all(store.read_pool())
    .await?;

    let patches: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<Uuid, _>("id"),
                "num_points": r.get::<i32, _>("num_points"),
                "bounds": r.get::<Option<serde_json::Value>, _>("bounds_geojson"),
            })
        })
        .collect();

    let total_points: i64 = rows
        .iter()
        .map(|r| r.get::<i32, _>("num_points") as i64)
        .sum();
    Ok(Json(serde_json::json!({
        "patches": patches,
        "total_points": total_points,
        "patch_count": patches.len(),
    })))
}

/// Get catalog statistics.
async fn catalog_stats(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, PcError> {
    let row = sqlx::query(
        "SELECT COUNT(*) as patch_count,
                COALESCE(SUM(num_points), 0) as total_points,
                ST_AsGeoJSON(ST_Extent(bounds))::jsonb as extent
         FROM pointcloud_patches WHERE catalog_id = $1",
    )
    .bind(catalog_id)
    .fetch_one(store.read_pool())
    .await?;

    Ok(Json(serde_json::json!({
        "patch_count": row.get::<i64, _>("patch_count"),
        "total_points": row.get::<i64, _>("total_points"),
        "extent": row.get::<Option<serde_json::Value>, _>("extent"),
    })))
}

/// Extract an elevation profile along a line.
#[derive(Deserialize)]
struct ProfileRequest {
    line_wkb_hex: String,
    #[serde(default = "default_samples")]
    num_samples: i32,
}
fn default_samples() -> i32 {
    100
}

async fn elevation_profile(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<serde_json::Value>, PcError> {
    let wkb = hex::decode(&req.line_wkb_hex).map_err(|_| PcError::Bad("invalid hex".into()))?;
    require_pointcloud(&store).await?;
    // Query real elevation from point cloud patches along the line
    let rows = sqlx::query(
        "WITH line AS (
            SELECT ST_GeomFromWKB($2, 4326) as geom
        ),
        samples AS (
            SELECT ST_LineInterpolatePoint(l.geom, i::float / $3) as pt,
                   i::float / $3 as fraction
            FROM line l, generate_series(0, $3) i
        ),
        elevations AS (
            SELECT s.fraction,
                   ST_X(s.pt) as lng,
                   ST_Y(s.pt) as lat,
                   (SELECT AVG(PC_Get(p, 'z')::float)
                    FROM (
                        SELECT PC_Explode(pp.pa) as p
                        FROM pointcloud_patches pp
                        WHERE pp.catalog_id = $1
                          AND pp.bounds && ST_Expand(s.pt, 0.0001)
                    ) sub
                    WHERE ST_DWithin(PC_Get(p, 'x')::float, ST_X(s.pt), 0.0001)
                      AND ST_DWithin(PC_Get(p, 'y')::float, ST_Y(s.pt), 0.0001)
                   ) as elevation
            FROM samples s
        )
        SELECT fraction, lng, lat, elevation FROM elevations ORDER BY fraction",
    )
    .bind(catalog_id)
    .bind(&wkb)
    .bind(req.num_samples)
    .fetch_all(store.read_pool())
    .await?;

    let profile: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "fraction": r.get::<f64, _>("fraction"),
                "lng": r.get::<f64, _>("lng"),
                "lat": r.get::<f64, _>("lat"),
                "elevation": r.get::<Option<f64>, _>("elevation"),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({"profile": profile})))
}

enum PcError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
    NotFound,
    Bad(String),
    NoPointCloud,
}
impl From<sqlx::Error> for PcError {
    fn from(e: sqlx::Error) -> Self {
        PcError::Db(e)
    }
}
impl From<ptolemy_storage::StoreError> for PcError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        PcError::Store(e)
    }
}
impl IntoResponse for PcError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            PcError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            PcError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            PcError::NoPointCloud => (
                StatusCode::NOT_IMPLEMENTED,
                "point cloud patches need the pointcloud extension, which this database does not have"
                    .to_string(),
            ),
            PcError::Store(e) => crate::errors::store_error_status(&e),
            PcError::Db(e) => {
                crate::errors::log_db_error("pointcloud", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
