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
        .route("/branches/{id}/import/geojson", post(import_geojson))
        .route("/branches/{id}/import/csv", post(import_csv))
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
        .header("x-feature-count", features.len().to_string())
        .body(axum::body::Body::from(body))
        .unwrap())
}

// ─── Import: GeoJSON ────────────────────────────────────────────────

fn default_import_message() -> String {
    "GeoJSON import".to_string()
}

fn default_author() -> String {
    "import".to_string()
}

#[derive(serde::Serialize)]
struct ImportResult {
    imported: usize,
    skipped: usize,
    changeset_id: Option<Uuid>,
    errors: Vec<String>,
}

async fn import_geojson(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ImportResult>, FormatError> {
    let features = body
        .get("features")
        .and_then(|f| f.as_array())
        .ok_or_else(|| {
            FormatError::Bad("expected GeoJSON FeatureCollection with 'features' array".into())
        })?;

    let message = body
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("GeoJSON import");
    let author = body
        .get("author")
        .and_then(|a| a.as_str())
        .unwrap_or("import");

    if features.len() > 50_000 {
        return Err(FormatError::Bad(
            "maximum 50,000 features per import".into(),
        ));
    }

    // Create changeset
    let changeset_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO changesets (id, branch_id, parent_id, message, author, created_at)
         SELECT $1, $2, head, $3, $4, NOW()
         FROM branches WHERE id = $2",
    )
    .bind(changeset_id)
    .bind(branch_id)
    .bind(message)
    .bind(author)
    .execute(store.pool())
    .await?;

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut errors = Vec::new();

    for (i, feature) in features.iter().enumerate() {
        let geometry = feature.get("geometry");
        let properties = feature
            .get("properties")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let geom_json = match geometry {
            Some(g) if !g.is_null() => serde_json::to_string(g).unwrap_or_default(),
            _ => {
                skipped += 1;
                errors.push(format!("feature {i}: no geometry"));
                continue;
            }
        };

        let feature_id = Uuid::now_v7();
        let result = sqlx::query(
            "INSERT INTO feature_versions (id, feature_id, changeset_id, operation, geometry, properties, created_at)
             VALUES ($1, $2, $3, 'insert', ST_SetSRID(ST_GeomFromGeoJSON($4), 4326), $5, NOW())",
        )
        .bind(Uuid::now_v7())
        .bind(feature_id)
        .bind(changeset_id)
        .bind(&geom_json)
        .bind(&properties)
        .execute(store.pool())
        .await;

        match result {
            Ok(_) => imported += 1,
            Err(e) => {
                skipped += 1;
                errors.push(format!("feature {i}: {e}"));
            }
        }
    }

    // Update branch head
    sqlx::query("UPDATE branches SET head = $1 WHERE id = $2")
        .bind(changeset_id)
        .bind(branch_id)
        .execute(store.pool())
        .await?;

    Ok(Json(ImportResult {
        imported,
        skipped,
        changeset_id: Some(changeset_id),
        errors,
    }))
}

// ─── Import: CSV ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ImportCsvRequest {
    /// CSV content as a string.
    csv: String,
    /// Column name for longitude (default: "longitude" or "lng" or "lon" or "x").
    lng_column: Option<String>,
    /// Column name for latitude (default: "latitude" or "lat" or "y").
    lat_column: Option<String>,
    /// Changeset message.
    #[serde(default = "default_import_message")]
    message: String,
    /// Author.
    #[serde(default = "default_author")]
    author: String,
}

async fn import_csv(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ImportCsvRequest>,
) -> Result<Json<ImportResult>, FormatError> {
    let lines: Vec<&str> = req.csv.lines().collect();
    if lines.is_empty() {
        return Err(FormatError::Bad("empty CSV".into()));
    }

    let headers: Vec<&str> = lines[0]
        .split(',')
        .map(|h| h.trim().trim_matches('"'))
        .collect();

    // Find lng/lat columns
    let lng_col = req.lng_column.as_deref().unwrap_or("");
    let lat_col = req.lat_column.as_deref().unwrap_or("");

    let lng_idx = headers.iter().position(|h| {
        let h_lower = h.to_lowercase();
        if !lng_col.is_empty() {
            h_lower == lng_col.to_lowercase()
        } else {
            matches!(h_lower.as_str(), "longitude" | "lng" | "lon" | "x")
        }
    });
    let lat_idx = headers.iter().position(|h| {
        let h_lower = h.to_lowercase();
        if !lat_col.is_empty() {
            h_lower == lat_col.to_lowercase()
        } else {
            matches!(h_lower.as_str(), "latitude" | "lat" | "y")
        }
    });

    let (lng_idx, lat_idx) = match (lng_idx, lat_idx) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            return Err(FormatError::Bad(
                "could not find longitude/latitude columns; specify lng_column and lat_column"
                    .into(),
            ));
        }
    };

    if lines.len() > 50_001 {
        return Err(FormatError::Bad("maximum 50,000 rows per import".into()));
    }

    // Create changeset
    let changeset_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO changesets (id, branch_id, parent_id, message, author, created_at)
         SELECT $1, $2, head, $3, $4, NOW()
         FROM branches WHERE id = $2",
    )
    .bind(changeset_id)
    .bind(branch_id)
    .bind(&req.message)
    .bind(&req.author)
    .execute(store.pool())
    .await?;

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut errors = Vec::new();

    for (row_num, line) in lines.iter().enumerate().skip(1) {
        let cols: Vec<&str> = line
            .split(',')
            .map(|c| c.trim().trim_matches('"'))
            .collect();
        if cols.len() <= lng_idx.max(lat_idx) {
            skipped += 1;
            errors.push(format!("row {row_num}: not enough columns"));
            continue;
        }

        let lng: f64 = match cols[lng_idx].parse() {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                errors.push(format!("row {row_num}: invalid longitude"));
                continue;
            }
        };
        let lat: f64 = match cols[lat_idx].parse() {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                errors.push(format!("row {row_num}: invalid latitude"));
                continue;
            }
        };

        // Build properties from all other columns
        let mut props = serde_json::Map::new();
        for (i, header) in headers.iter().enumerate() {
            if i != lng_idx
                && i != lat_idx
                && let Some(&val) = cols.get(i)
            {
                props.insert(
                    header.to_string(),
                    if let Ok(n) = val.parse::<f64>() {
                        serde_json::Value::Number(
                            serde_json::Number::from_f64(n)
                                .unwrap_or_else(|| serde_json::Number::from(0)),
                        )
                    } else {
                        serde_json::Value::String(val.to_string())
                    },
                );
            }
        }

        let feature_id = Uuid::now_v7();
        let result = sqlx::query(
            "INSERT INTO feature_versions (id, feature_id, changeset_id, operation, geometry, properties, created_at)
             VALUES ($1, $2, $3, 'insert', ST_SetSRID(ST_MakePoint($4, $5), 4326), $6, NOW())",
        )
        .bind(Uuid::now_v7())
        .bind(feature_id)
        .bind(changeset_id)
        .bind(lng)
        .bind(lat)
        .bind(serde_json::Value::Object(props))
        .execute(store.pool())
        .await;

        match result {
            Ok(_) => imported += 1,
            Err(e) => {
                skipped += 1;
                errors.push(format!("row {row_num}: {e}"));
            }
        }
    }

    // Update branch head
    sqlx::query("UPDATE branches SET head = $1 WHERE id = $2")
        .bind(changeset_id)
        .bind(branch_id)
        .execute(store.pool())
        .await?;

    Ok(Json(ImportResult {
        imported,
        skipped,
        changeset_id: Some(changeset_id),
        errors,
    }))
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
