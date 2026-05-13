// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Raster/imagery catalog and tile management.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn raster_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/rasters",
            get(list_catalogs).post(create_catalog),
        )
        .route("/rasters/{id}", get(get_catalog))
        .route("/rasters/{id}/tiles", get(list_tiles).post(upload_tile))
        .route("/rasters/{id}/value", get(point_value))
        .route("/rasters/{id}/stats", get(band_stats))
}

#[derive(Serialize)]
struct RasterCatalog {
    id: Uuid,
    name: String,
    srid: i32,
    pixel_type: String,
    num_bands: i32,
    tile_width: i32,
    tile_height: i32,
    nodata_value: Option<f64>,
}

async fn list_catalogs(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<RasterCatalog>>, RasterError> {
    let rows = sqlx::query(
        "SELECT id, name, srid, pixel_type, num_bands, tile_width, tile_height, nodata_value
         FROM raster_catalogs WHERE dataset_id = $1",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| RasterCatalog {
                id: r.get("id"),
                name: r.get("name"),
                srid: r.get("srid"),
                pixel_type: r.get("pixel_type"),
                num_bands: r.get("num_bands"),
                tile_width: r.get("tile_width"),
                tile_height: r.get("tile_height"),
                nodata_value: r.get("nodata_value"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateCatalogRequest {
    name: String,
    #[serde(default = "default_srid")]
    srid: i32,
    #[serde(default = "default_pixel_type")]
    pixel_type: String,
    #[serde(default = "default_bands")]
    num_bands: i32,
}
fn default_srid() -> i32 {
    4326
}
fn default_pixel_type() -> String {
    "uint8".into()
}
fn default_bands() -> i32 {
    1
}

async fn create_catalog(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateCatalogRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), RasterError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO raster_catalogs (id, dataset_id, name, srid, pixel_type, num_bands)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(dataset_id)
    .bind(&req.name)
    .bind(req.srid)
    .bind(&req.pixel_type)
    .bind(req.num_bands)
    .execute(store.pool())
    .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_catalog(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RasterCatalog>, RasterError> {
    let r = sqlx::query(
        "SELECT id, name, srid, pixel_type, num_bands, tile_width, tile_height, nodata_value
         FROM raster_catalogs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(RasterError::NotFound)?;
    Ok(Json(RasterCatalog {
        id: r.get("id"),
        name: r.get("name"),
        srid: r.get("srid"),
        pixel_type: r.get("pixel_type"),
        num_bands: r.get("num_bands"),
        tile_width: r.get("tile_width"),
        tile_height: r.get("tile_height"),
        nodata_value: r.get("nodata_value"),
    }))
}

#[derive(Serialize)]
struct TileInfo {
    id: Uuid,
    zoom_level: i32,
    bounds: Option<serde_json::Value>,
}

async fn list_tiles(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
) -> Result<Json<Vec<TileInfo>>, RasterError> {
    let rows = sqlx::query(
        "SELECT id, zoom_level, ST_AsGeoJSON(bounds)::jsonb as bounds_geojson
         FROM raster_tiles WHERE catalog_id = $1 ORDER BY zoom_level",
    )
    .bind(catalog_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TileInfo {
                id: r.get("id"),
                zoom_level: r.get("zoom_level"),
                bounds: r.get("bounds_geojson"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct UploadTileRequest {
    zoom_level: i32,
    bounds_wkb_hex: String,
    /// Raster data in WKB hex format (as produced by ST_AsWKB or raster2pgsql)
    rast_hex: String,
}

async fn upload_tile(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
    Json(req): Json<UploadTileRequest>,
) -> Result<StatusCode, RasterError> {
    let id = Uuid::now_v7();
    let bounds_wkb = hex::decode(&req.bounds_wkb_hex)
        .map_err(|_| RasterError::Bad("invalid bounds hex".into()))?;
    let rast_bytes =
        hex::decode(&req.rast_hex).map_err(|_| RasterError::Bad("invalid raster hex".into()))?;
    sqlx::query(
        "INSERT INTO raster_tiles (id, catalog_id, bounds, zoom_level, rast)
         VALUES ($1, $2, ST_GeomFromWKB($3, 4326), $4, $5::raster)",
    )
    .bind(id)
    .bind(catalog_id)
    .bind(&bounds_wkb)
    .bind(req.zoom_level)
    .bind(&rast_bytes)
    .execute(store.pool())
    .await?;
    Ok(StatusCode::CREATED)
}

/// Get pixel value at a point.
#[derive(Deserialize)]
struct PointValueQuery {
    lng: f64,
    lat: f64,
}

async fn point_value(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
    Query(q): Query<PointValueQuery>,
) -> Result<Json<serde_json::Value>, RasterError> {
    let row = sqlx::query(
        "SELECT ST_Value(rast, ST_SetSRID(ST_MakePoint($2, $3), 4326)) as val
         FROM raster_tiles
         WHERE catalog_id = $1
           AND ST_Intersects(bounds, ST_SetSRID(ST_MakePoint($2, $3), 4326))
         ORDER BY zoom_level DESC LIMIT 1",
    )
    .bind(catalog_id)
    .bind(q.lng)
    .bind(q.lat)
    .fetch_optional(store.pool())
    .await?;

    match row {
        Some(r) => {
            let val: Option<f64> = r.get("val");
            Ok(Json(serde_json::json!({"value": val})))
        }
        None => Ok(Json(
            serde_json::json!({"value": null, "note": "no tile at location"}),
        )),
    }
}

/// Get band statistics for a raster catalog.
async fn band_stats(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, RasterError> {
    let row = sqlx::query(
        "SELECT COUNT(*) as tile_count,
                MIN(zoom_level) as min_zoom,
                MAX(zoom_level) as max_zoom
         FROM raster_tiles WHERE catalog_id = $1",
    )
    .bind(catalog_id)
    .fetch_one(store.pool())
    .await?;

    Ok(Json(serde_json::json!({
        "tile_count": row.get::<i64, _>("tile_count"),
        "min_zoom": row.get::<Option<i32>, _>("min_zoom"),
        "max_zoom": row.get::<Option<i32>, _>("max_zoom"),
    })))
}

enum RasterError {
    Db(sqlx::Error),
    NotFound,
    Bad(String),
}
impl From<sqlx::Error> for RasterError {
    fn from(e: sqlx::Error) -> Self {
        RasterError::Db(e)
    }
}
impl IntoResponse for RasterError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            RasterError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            RasterError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            RasterError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
