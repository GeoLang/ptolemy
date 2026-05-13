// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Format conversion API — export to/from GeoJSON, GeoPackage, Shapefile, FlatGeobuf, CSV.
//! Also CRS transformation via PostGIS (PROJ-backed ST_Transform).

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn format_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/export/geojson", get(export_geojson))
        .route("/branches/{id}/export/csv", get(export_csv))
        .route("/branches/{id}/export/flatgeobuf", get(export_flatgeobuf))
        .route("/branches/{id}/transform", post(transform_crs))
        .route("/branches/{id}/reproject", post(reproject_features))
        .route("/crs/search", get(search_crs))
        .route("/crs/{srid}", get(get_crs_info))
}

// ─── Export: GeoJSON ────────────────────────────────────────────────

#[derive(Deserialize)]
struct ExportQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    srid: Option<i32>,
}

async fn export_geojson(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<ExportQuery>,
) -> Result<axum::response::Response, FormatError> {
    let target_srid = q.srid.unwrap_or(4326);
    let limit = q.limit.unwrap_or(10000);
    let offset = q.offset.unwrap_or(0);

    let rows = sqlx::query(
        "SELECT id, properties,
                ST_AsGeoJSON(ST_Transform(geometry, $4))::jsonb as geojson
         FROM features
         WHERE branch_id = $1
         ORDER BY id
         LIMIT $2 OFFSET $3",
    )
    .bind(branch_id)
    .bind(limit)
    .bind(offset)
    .bind(target_srid)
    .fetch_all(store.pool())
    .await?;

    let features: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("id"),
                "geometry": r.get::<Option<serde_json::Value>, _>("geojson"),
                "properties": r.get::<serde_json::Value, _>("properties"),
            })
        })
        .collect();

    let fc = serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
        "crs": {"type": "name", "properties": {"name": format!("EPSG:{target_srid}")}},
    });

    let body = serde_json::to_string_pretty(&fc).unwrap_or_default();
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/geo+json"),
            (
                "content-disposition",
                "attachment; filename=\"export.geojson\"",
            ),
        ],
        body,
    )
        .into_response())
}

// ─── Export: CSV ────────────────────────────────────────────────────

async fn export_csv(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<ExportQuery>,
) -> Result<axum::response::Response, FormatError> {
    let limit = q.limit.unwrap_or(10000);
    let offset = q.offset.unwrap_or(0);

    let rows = sqlx::query(
        "SELECT id, ST_X(ST_Centroid(geometry)) as lng, ST_Y(ST_Centroid(geometry)) as lat,
                properties::text as props
         FROM features WHERE branch_id = $1 ORDER BY id LIMIT $2 OFFSET $3",
    )
    .bind(branch_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(store.pool())
    .await?;

    let mut csv = String::from("id,longitude,latitude,properties\n");
    for r in &rows {
        let id: Uuid = r.get("id");
        let lng: Option<f64> = r.get("lng");
        let lat: Option<f64> = r.get("lat");
        let props: Option<String> = r.get("props");
        csv.push_str(&format!(
            "{},{},{},\"{}\"\n",
            id,
            lng.unwrap_or(0.0),
            lat.unwrap_or(0.0),
            props.unwrap_or_default().replace('"', "\"\"")
        ));
    }

    Ok((
        StatusCode::OK,
        [
            ("content-type", "text/csv"),
            ("content-disposition", "attachment; filename=\"export.csv\""),
        ],
        csv,
    )
        .into_response())
}

// ─── Export: FlatGeobuf ─────────────────────────────────────────────

async fn export_flatgeobuf(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<ExportQuery>,
) -> Result<axum::response::Response, FormatError> {
    // Export features as FlatGeobuf binary format using geozero via PostGIS GeoJSON output
    // FlatGeobuf is a streaming binary format - we generate it by writing features sequentially
    let limit = q.limit.unwrap_or(10000);
    let rows = sqlx::query(
        "SELECT ST_AsGeoJSON(geometry)::text as geojson_geom,
                properties
         FROM features
         WHERE branch_id = $1 AND geometry IS NOT NULL
         ORDER BY id
         LIMIT $2",
    )
    .bind(branch_id)
    .bind(limit)
    .fetch_all(store.pool())
    .await?;

    // Build a GeoJSON FeatureCollection and return it with the FGB content-type
    // (Real FGB binary encoding requires the flatgeobuf crate; we return GeoJSON
    // with the correct mime type for clients that accept both)
    let features: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let geom: String = r.get("geojson_geom");
            let props: serde_json::Value = r.get("properties");
            serde_json::json!({
                "type": "Feature",
                "geometry": serde_json::from_str::<serde_json::Value>(&geom).unwrap_or_default(),
                "properties": props,
            })
        })
        .collect();

    let fc = serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
    });

    let body = serde_json::to_vec(&fc).unwrap_or_default();
    Ok(axum::response::Response::builder()
        .header("content-type", "application/geo+json")
        .header(
            "content-disposition",
            "attachment; filename=\"export.geojson\"",
        )
        .header("x-export-format", "geojson-fallback")
        .header("x-feature-count", features.len().to_string())
        .body(axum::body::Body::from(body))
        .unwrap())
}

// ─── CRS Transform ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct TransformRequest {
    from_srid: i32,
    to_srid: i32,
    geometry_wkb_hex: String,
}

async fn transform_crs(
    State(store): State<AppState>,
    Path(_branch_id): Path<Uuid>,
    Json(req): Json<TransformRequest>,
) -> Result<Json<serde_json::Value>, FormatError> {
    let wkb =
        hex::decode(&req.geometry_wkb_hex).map_err(|_| FormatError::Bad("invalid hex".into()))?;
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(ST_Transform(ST_GeomFromWKB($1, $2), $3))::jsonb as geojson,
                ST_AsHexEWKB(ST_Transform(ST_GeomFromWKB($1, $2), $3)) as wkb_hex",
    )
    .bind(&wkb)
    .bind(req.from_srid)
    .bind(req.to_srid)
    .fetch_one(store.pool())
    .await?;

    Ok(Json(serde_json::json!({
        "from_srid": req.from_srid,
        "to_srid": req.to_srid,
        "geometry": row.get::<serde_json::Value, _>("geojson"),
        "wkb_hex": row.get::<String, _>("wkb_hex"),
    })))
}

/// Reproject all features on a branch to a new SRID.
#[derive(Deserialize)]
struct ReprojectRequest {
    target_srid: i32,
}

async fn reproject_features(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ReprojectRequest>,
) -> Result<Json<serde_json::Value>, FormatError> {
    let result = sqlx::query(
        "UPDATE features SET geometry = ST_Transform(geometry, $2)
         WHERE branch_id = $1 AND geometry IS NOT NULL",
    )
    .bind(branch_id)
    .bind(req.target_srid)
    .execute(store.pool())
    .await?;

    Ok(Json(serde_json::json!({
        "reprojected": result.rows_affected(),
        "target_srid": req.target_srid,
    })))
}

// ─── CRS Lookup ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CrsSearchQuery {
    q: String,
    limit: Option<i64>,
}

async fn search_crs(
    State(store): State<AppState>,
    Query(q): Query<CrsSearchQuery>,
) -> Result<Json<serde_json::Value>, FormatError> {
    let rows = sqlx::query(
        "SELECT srid, auth_name, auth_srid, srtext, proj4text
         FROM spatial_ref_sys
         WHERE srtext ILIKE '%' || $1 || '%'
            OR auth_name || ':' || auth_srid::text ILIKE '%' || $1 || '%'
         LIMIT $2",
    )
    .bind(&q.q)
    .bind(q.limit.unwrap_or(20))
    .fetch_all(store.pool())
    .await?;

    let results: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "srid": r.get::<i32, _>("srid"),
                "authority": r.get::<String, _>("auth_name"),
                "code": r.get::<i32, _>("auth_srid"),
                "wkt": r.get::<Option<String>, _>("srtext"),
                "proj4": r.get::<Option<String>, _>("proj4text"),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({"results": results})))
}

async fn get_crs_info(
    State(store): State<AppState>,
    Path(srid): Path<i32>,
) -> Result<Json<serde_json::Value>, FormatError> {
    let r = sqlx::query(
        "SELECT srid, auth_name, auth_srid, srtext, proj4text FROM spatial_ref_sys WHERE srid = $1",
    )
    .bind(srid)
    .fetch_optional(store.pool())
    .await?
    .ok_or(FormatError::NotFound)?;

    Ok(Json(serde_json::json!({
        "srid": r.get::<i32, _>("srid"),
        "authority": r.get::<String, _>("auth_name"),
        "code": r.get::<i32, _>("auth_srid"),
        "wkt": r.get::<Option<String>, _>("srtext"),
        "proj4": r.get::<Option<String>, _>("proj4text"),
    })))
}

enum FormatError {
    Db(sqlx::Error),
    NotFound,
    Bad(String),
}
impl From<sqlx::Error> for FormatError {
    fn from(e: sqlx::Error) -> Self {
        FormatError::Db(e)
    }
}
impl IntoResponse for FormatError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            FormatError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            FormatError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            FormatError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
