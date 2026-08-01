// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! H3 hexagonal spatial indexing and aggregation.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_storage::WriteGrant;
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn h3_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/h3/index", post(index_features_h3))
        .route("/branches/{id}/h3/hexagons", get(get_hexagons))
        .route("/branches/{id}/h3/aggregate", get(aggregate_by_hex))
        .route("/branches/{id}/h3/neighbors", get(hex_neighbors))
        .route("/branches/{id}/h3/compact", post(compact_hexes))
        .route("/h3/cell", get(point_to_cell))
        .route("/h3/boundary", get(cell_boundary))
}

/// Index all features on a branch with H3 cells at given resolution.
#[derive(Deserialize)]
struct IndexH3Request {
    #[serde(default = "default_resolution")]
    resolution: i32,
}
fn default_resolution() -> i32 {
    7
}

async fn index_features_h3(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<IndexH3Request>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let indexed = store
        .index_branch_features_h3(&grant, req.resolution)
        .await?;

    Ok(Json(serde_json::json!({
        "indexed": indexed,
        "resolution": req.resolution,
    })))
}

/// Get H3 hexagons covering features.
#[derive(Deserialize)]
struct HexQuery {
    #[serde(default = "default_resolution")]
    resolution: i32,
    limit: Option<i64>,
}

async fn get_hexagons(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<HexQuery>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let (_, source) = store.features_source(branch_id).await?;
    let rows = sqlx::query(&format!(
        "SELECT DISTINCT h3_lat_lng_to_cell(ST_Centroid(geometry), $2)::text as cell,
                ST_AsGeoJSON(h3_cell_to_boundary(h3_lat_lng_to_cell(ST_Centroid(geometry), $2))::geometry)::jsonb as boundary
         FROM {source} f
         WHERE geometry IS NOT NULL
         LIMIT $3"
    )).bind(branch_id).bind(q.resolution).bind(q.limit.unwrap_or(1000))
    .fetch_all(store.read_pool()).await?;

    let hexagons: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "cell": r.get::<String, _>("cell"),
                "boundary": r.get::<serde_json::Value, _>("boundary"),
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({"hexagons": hexagons, "count": hexagons.len()}),
    ))
}

/// Aggregate feature counts by H3 hex.
async fn aggregate_by_hex(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<HexQuery>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let (_, source) = store.features_source(branch_id).await?;
    let rows = sqlx::query(&format!(
        "SELECT h3_lat_lng_to_cell(ST_Centroid(geometry), $2)::text as cell,
                COUNT(*) as feature_count
         FROM {source} f
         WHERE geometry IS NOT NULL
         GROUP BY cell
         ORDER BY feature_count DESC
         LIMIT $3"
    ))
    .bind(branch_id)
    .bind(q.resolution)
    .bind(q.limit.unwrap_or(500))
    .fetch_all(store.read_pool())
    .await?;

    let cells: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "cell": r.get::<String, _>("cell"),
                "count": r.get::<i64, _>("feature_count"),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({"cells": cells})))
}

/// Get neighbors (k-ring) of a hex cell.
#[derive(Deserialize)]
struct NeighborQuery {
    cell: String,
    #[serde(default = "default_k")]
    k: i32,
}
fn default_k() -> i32 {
    1
}

async fn hex_neighbors(
    State(store): State<AppState>,
    Path(_branch_id): Path<Uuid>,
    Query(q): Query<NeighborQuery>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let rows = sqlx::query("SELECT h3_grid_disk($1::h3index, $2) as neighbors")
        .bind(&q.cell)
        .bind(q.k)
        .fetch_all(store.read_pool())
        .await?;

    let neighbors: Vec<String> = rows
        .iter()
        .map(|r| r.get::<String, _>("neighbors"))
        .collect();
    Ok(Json(
        serde_json::json!({"cell": q.cell, "k": q.k, "neighbors": neighbors}),
    ))
}

/// Compact a set of hexagons to the minimal representation.
#[derive(Deserialize)]
struct CompactRequest {
    cells: Vec<String>,
}

async fn compact_hexes(
    State(store): State<AppState>,
    Path(_branch_id): Path<Uuid>,
    Json(req): Json<CompactRequest>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let row = sqlx::query(
        "SELECT array_agg(h3_compact_cells(ARRAY(SELECT unnest($1::h3index[]))::h3index[])::text) as compacted",
    ).bind(&req.cells)
    .fetch_one(store.read_pool()).await?;

    let compacted: Option<Vec<String>> = row.get("compacted");
    Ok(Json(
        serde_json::json!({"compacted": compacted.unwrap_or_default()}),
    ))
}

/// Convert a lat/lng point to an H3 cell.
#[derive(Deserialize)]
struct PointToCellQuery {
    lng: f64,
    lat: f64,
    #[serde(default = "default_resolution")]
    resolution: i32,
}

async fn point_to_cell(
    State(store): State<AppState>,
    Query(q): Query<PointToCellQuery>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let row = sqlx::query(
        "SELECT h3_lat_lng_to_cell(ST_SetSRID(ST_MakePoint($1, $2), 4326)::point, $3)::text as cell",
    ).bind(q.lng).bind(q.lat).bind(q.resolution)
    .fetch_one(store.read_pool()).await?;
    Ok(Json(
        serde_json::json!({"cell": row.get::<String, _>("cell"), "resolution": q.resolution}),
    ))
}

/// Get the boundary polygon of an H3 cell.
#[derive(Deserialize)]
struct CellBoundaryQuery {
    cell: String,
}

async fn cell_boundary(
    State(store): State<AppState>,
    Query(q): Query<CellBoundaryQuery>,
) -> Result<Json<serde_json::Value>, H3Error> {
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(h3_cell_to_boundary($1::h3index)::geometry)::jsonb as boundary",
    )
    .bind(&q.cell)
    .fetch_one(store.read_pool())
    .await?;
    Ok(Json(row.get("boundary")))
}

enum H3Error {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
}
impl From<sqlx::Error> for H3Error {
    fn from(e: sqlx::Error) -> Self {
        H3Error::Db(e)
    }
}
impl From<ptolemy_storage::StoreError> for H3Error {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        H3Error::Store(e)
    }
}
impl IntoResponse for H3Error {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            H3Error::Db(e) => {
                crate::errors::log_db_error("h3", &e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            H3Error::Store(e) => crate::errors::store_error_status(&e),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
