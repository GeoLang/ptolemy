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
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(ST_Extrude(ST_Force3D(geometry), 0, 0, $3))::jsonb as geojson
         FROM features WHERE id = $1 AND branch_id = $2",
    )
    .bind(req.feature_id)
    .bind(branch_id)
    .bind(req.height)
    .fetch_optional(store.pool())
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
    let row = sqlx::query(
        "SELECT ST_3DArea(geometry) as surface_area, ST_Volume(geometry) as volume
         FROM features WHERE id = $1 AND branch_id = $2",
    )
    .bind(req.feature_id)
    .bind(branch_id)
    .fetch_optional(store.pool())
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
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(ST_3DIntersection(a.geometry, b.geometry))::jsonb as geojson
         FROM features a, features b
         WHERE a.id = $1 AND b.id = $2 AND a.branch_id = $3 AND b.branch_id = $3",
    )
    .bind(req.feature_a)
    .bind(req.feature_b)
    .bind(branch_id)
    .fetch_optional(store.pool())
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
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(ST_StraightSkeleton(geometry))::jsonb as geojson
         FROM features WHERE id = $1 AND branch_id = $2",
    )
    .bind(req.feature_id)
    .bind(branch_id)
    .fetch_optional(store.pool())
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
    let wkb = hex::decode(&req.buffer_geometry_wkb_hex)
        .map_err(|_| SfcgalError::Bad("invalid hex".into()))?;
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(ST_MinkowskiSum(geometry, ST_GeomFromWKB($3, 4326)))::jsonb as geojson
         FROM features WHERE id = $1 AND branch_id = $2",
    )
    .bind(req.feature_id)
    .bind(branch_id)
    .bind(&wkb)
    .fetch_optional(store.pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(row.get("geojson")))
}

#[derive(Deserialize)]
struct TesselateRequest {
    feature_id: Uuid,
}

async fn tesselate(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<TesselateRequest>,
) -> Result<Json<serde_json::Value>, SfcgalError> {
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(ST_Tesselate(geometry))::jsonb as geojson
         FROM features WHERE id = $1 AND branch_id = $2",
    )
    .bind(req.feature_id)
    .bind(branch_id)
    .fetch_optional(store.pool())
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
    let row = sqlx::query(
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
         FROM features WHERE id = $1 AND branch_id = $2",
    )
    .bind(req.feature_id)
    .bind(branch_id)
    .bind(req.observer_x)
    .bind(req.observer_y)
    .bind(req.observer_z)
    .fetch_optional(store.pool())
    .await?
    .ok_or(SfcgalError::NotFound)?;
    Ok(Json(serde_json::json!({
        "distance": row.get::<Option<f64>, _>("distance"),
        "line_of_sight": row.get::<Option<bool>, _>("line_of_sight"),
    })))
}

enum SfcgalError {
    Db(sqlx::Error),
    NotFound,
    Bad(String),
}
impl From<sqlx::Error> for SfcgalError {
    fn from(e: sqlx::Error) -> Self {
        SfcgalError::Db(e)
    }
}
impl IntoResponse for SfcgalError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            SfcgalError::NotFound => (StatusCode::NOT_FOUND, "feature not found".to_string()),
            SfcgalError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            SfcgalError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
