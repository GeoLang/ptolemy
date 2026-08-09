// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! SFCGAL 3D geometry operations and advanced spatial analysis.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn sfcgal_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/3d/extrude", post(extrude_3d))
        .route("/branches/{id}/3d/volume", post(compute_volume))
        .route("/branches/{id}/3d/intersection", post(intersection_3d))
        .route(
            "/branches/{id}/3d/straight-skeleton",
            post(straight_skeleton),
        )
        .route("/branches/{id}/3d/minkowski-sum", post(minkowski_sum))
        .route("/branches/{id}/3d/tesselate", post(tesselate))
        .route("/branches/{id}/3d/visibility", post(visibility))
}

/// Every route here calls an SFCGAL function (`ST_Extrude`, `ST_Volume`,
/// `ST_3DIntersection` and the rest), none of which a PostGIS build without
/// `postgis_sfcgal` defines. Without it the call is not a failure to hide
/// behind a 500, it is a route this deployment does not have.
async fn require_sfcgal(store: &AppState) -> Result<(), SfcgalError> {
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'postgis_sfcgal')",
    )
    .fetch_one(store.read_pool())
    .await?;
    if present {
        Ok(())
    } else {
        Err(SfcgalError::NoSfcgal)
    }
}

#[derive(Deserialize)]
struct ExtrudeRequest {
    feature_id: Uuid,
    height: f64,
}

async fn extrude_3d(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ExtrudeRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    let (_, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_AsGeoJSON(ST_Extrude(ST_Force3D(geometry), 0, 0, $3))::jsonb as geojson
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(req.feature_id)
    .bind(branch_id)
    .bind(req.height)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(row.get("geojson")))
}

#[derive(Deserialize)]
struct VolumeRequest {
    feature_id: Uuid,
}

async fn compute_volume(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    let (_, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_3DArea(geometry) as surface_area, ST_Volume(geometry) as volume
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(req.feature_id)
    .bind(branch_id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(serde_json::json!({
        "surface_area": row.get::<Option<f64>, _>("surface_area"),
        "volume": row.get::<Option<f64>, _>("volume"),
    })))
}

#[derive(Deserialize)]
struct Intersection3DRequest {
    feature_a: Uuid,
    feature_b: Uuid,
}

async fn intersection_3d(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<Intersection3DRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    // both sides are the same branch, so the same scoped source twice
    let (_, source) = store.features_source_at(branch_id, "$3").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_AsGeoJSON(ST_3DIntersection(a.geometry, b.geometry))::jsonb as geojson
         FROM {source} a, {source} b
         WHERE a.id = $1 AND b.id = $2"
    ))
    .bind(req.feature_a)
    .bind(req.feature_b)
    .bind(branch_id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(row.get("geojson")))
}

#[derive(Deserialize)]
struct SkeletonRequest {
    feature_id: Uuid,
}

async fn straight_skeleton(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SkeletonRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    let (_, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_AsGeoJSON(ST_StraightSkeleton(geometry))::jsonb as geojson
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(req.feature_id)
    .bind(branch_id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(row.get("geojson")))
}

#[derive(Deserialize)]
struct MinkowskiRequest {
    feature_id: Uuid,
    buffer_geometry_wkb_hex: String,
}

async fn minkowski_sum(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<MinkowskiRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    let wkb = hex::decode(&req.buffer_geometry_wkb_hex)
        .map_err(|_| SfcgalError::Bad("invalid hex".into()))?;
    let (_, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_AsGeoJSON(ST_MinkowskiSum(geometry, ST_GeomFromWKB($3, 4326)))::jsonb as geojson
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(req.feature_id)
    .bind(branch_id)
    .bind(&wkb)
    .fetch_optional(store.read_pool())
    .await
    .map_err(unsummable_or_internal)?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(row.get("geojson")))
}

/// SFCGAL takes a polygon as the shape to sweep and rejects anything else. The
/// buffer geometry is the client's, so its refusal is the client's answer.
fn unsummable_or_internal(error: sqlx::Error) -> SfcgalError {
    if let sqlx::Error::Database(db) = &error
        && db.code().as_deref() == Some(crate::errors::POSTGIS_ERROR)
        && db.message().contains(MINKOWSKI_REFUSAL)
    {
        return SfcgalError::Bad(db.message().to_string());
    }
    SfcgalError::Db(error)
}

/// SFCGAL prefixes what it will not sum with the name of the operation.
const MINKOWSKI_REFUSAL: &str = "minkowski_sum()";

#[derive(Deserialize)]
struct TesselateRequest {
    feature_id: Uuid,
}

async fn tesselate(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<TesselateRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    let (_, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_AsGeoJSON(ST_Tesselate(geometry))::jsonb as geojson
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(req.feature_id)
    .bind(branch_id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(row.get("geojson")))
}

#[derive(Deserialize)]
struct VisibilityRequest {
    observer_x: f64,
    observer_y: f64,
    observer_z: f64,
    feature_id: Uuid,
}

async fn visibility(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<VisibilityRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    require_sfcgal(&store).await?;
    let (_, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT ST_3DDistance(
            geometry,
            ST_SetSRID(ST_MakePoint($3, $4, $5), 4326)
         ) as distance,
         ST_3DIntersects(
            geometry,
            ST_MakeLine(
                ST_SetSRID(ST_MakePoint($3, $4, $5), 4326),
                ST_Centroid(geometry)
            )
         ) as line_of_sight
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(req.feature_id)
    .bind(branch_id)
    .bind(req.observer_x)
    .bind(req.observer_y)
    .bind(req.observer_z)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(serde_json::json!({
        "distance": row.get::<Option<f64>, _>("distance"),
        "line_of_sight": row.get::<Option<bool>, _>("line_of_sight"),
    })))
}

enum SfcgalError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
    NotFound,
    Bad(String),
    NoSfcgal,
}
impl From<sqlx::Error> for SfcgalError {
    fn from(e: sqlx::Error) -> Self {
        SfcgalError::Db(e)
    }
}
impl From<ptolemy_storage::StoreError> for SfcgalError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        SfcgalError::Store(e)
    }
}
impl IntoResponse for SfcgalError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            SfcgalError::NotFound => (StatusCode::NOT_FOUND, "feature not found".to_string()),
            SfcgalError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            SfcgalError::NoSfcgal => (
                StatusCode::NOT_IMPLEMENTED,
                "3d geometry operations need the postgis_sfcgal extension, which this database does not have"
                    .to_string(),
            ),
            SfcgalError::Db(e) => {
                crate::errors::log_db_error("sfcgal", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
            SfcgalError::Store(e) => crate::errors::store_error_status(&e),
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
