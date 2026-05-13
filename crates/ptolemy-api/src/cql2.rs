// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! CQL2 (Common Query Language) filter parser and OGC Tiles API.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn cql2_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/features/filter", post(cql2_filter))
        .route("/tiles/tileMatrixSets", get(tile_matrix_sets))
        .route("/tiles/tileMatrixSets/{tms}", get(tile_matrix_set))
        .route("/datasets/{id}/tiles/{tms}/{z}/{x}/{y}", get(ogc_tile))
}

// ─── CQL2 Filter ────────────────────────────────────────────────────

/// CQL2 filter request — accepts a CQL2-JSON or CQL2-Text filter expression.
#[derive(Deserialize)]
struct Cql2FilterRequest {
    /// CQL2-JSON filter object
    filter: serde_json::Value,
    #[serde(default = "default_filter_lang")]
    #[allow(dead_code)]
    filter_lang: String,
    limit: Option<i64>,
    offset: Option<i64>,
}
fn default_filter_lang() -> String {
    "cql2-json".into()
}

async fn cql2_filter(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<Cql2FilterRequest>,
) -> Result<Json<serde_json::Value>, Cql2Error> {
    // Parse CQL2-JSON filter into SQL WHERE clause
    let where_clause = cql2_to_sql(&req.filter)?;
    let limit = req.limit.unwrap_or(100);
    let offset = req.offset.unwrap_or(0);

    let query = format!(
        "SELECT id, dataset_id, properties, ST_AsGeoJSON(geometry)::jsonb as geojson
         FROM features
         WHERE branch_id = $1 AND ({where_clause})
         LIMIT $2 OFFSET $3",
        where_clause = where_clause
    );

    let rows = sqlx::query(&query)
        .bind(branch_id)
        .bind(limit)
        .bind(offset)
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

    Ok(Json(serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
        "numberReturned": features.len(),
    })))
}

/// Convert CQL2-JSON filter to SQL WHERE clause.
/// Supports: eq, lt, gt, lte, gte, like, between, in, and, or, not, s_intersects, s_within.
fn cql2_to_sql(filter: &serde_json::Value) -> Result<String, Cql2Error> {
    match filter.get("op").and_then(|v| v.as_str()) {
        Some("and") => {
            let args = filter
                .get("args")
                .and_then(|a| a.as_array())
                .ok_or(Cql2Error::Bad("'and' requires 'args' array".into()))?;
            let clauses: Result<Vec<String>, _> = args.iter().map(cql2_to_sql).collect();
            Ok(format!("({})", clauses?.join(" AND ")))
        }
        Some("or") => {
            let args = filter
                .get("args")
                .and_then(|a| a.as_array())
                .ok_or(Cql2Error::Bad("'or' requires 'args' array".into()))?;
            let clauses: Result<Vec<String>, _> = args.iter().map(cql2_to_sql).collect();
            Ok(format!("({})", clauses?.join(" OR ")))
        }
        Some("not") => {
            let args = filter
                .get("args")
                .and_then(|a| a.as_array())
                .ok_or(Cql2Error::Bad("'not' requires 'args' array".into()))?;
            let inner = cql2_to_sql(&args[0])?;
            Ok(format!("NOT ({})", inner))
        }
        Some("=" | "eq") => binary_op(filter, "="),
        Some("<" | "lt") => binary_op(filter, "<"),
        Some(">" | "gt") => binary_op(filter, ">"),
        Some("<=" | "lte") => binary_op(filter, "<="),
        Some(">=" | "gte") => binary_op(filter, ">="),
        Some("!=" | "neq") => binary_op(filter, "!="),
        Some("like") => {
            let args = get_args(filter)?;
            let prop = extract_property(&args[0])?;
            let pattern = extract_literal(&args[1])?;
            Ok(format!(
                "properties->>'{}' LIKE {}",
                sanitize_field(&prop),
                sanitize_value(&pattern)
            ))
        }
        Some("between") => {
            let args = get_args(filter)?;
            let prop = extract_property(&args[0])?;
            let low = extract_literal(&args[1])?;
            let high = extract_literal(&args[2])?;
            Ok(format!(
                "(properties->>'{prop}')::float BETWEEN {low} AND {high}",
                prop = sanitize_field(&prop),
                low = sanitize_value(&low),
                high = sanitize_value(&high)
            ))
        }
        Some("in") => {
            let args = get_args(filter)?;
            let prop = extract_property(&args[0])?;
            let values: Vec<String> = args[1..]
                .iter()
                .map(|v| extract_literal(v).map(|s| sanitize_value(&s)))
                .collect::<Result<_, _>>()?;
            Ok(format!(
                "properties->>'{}' IN ({})",
                sanitize_field(&prop),
                values.join(", ")
            ))
        }
        Some("s_intersects") => {
            let args = get_args(filter)?;
            let geom = &args[1];
            Ok(format!(
                "ST_Intersects(geometry, ST_GeomFromGeoJSON('{}'))",
                serde_json::to_string(geom)
                    .unwrap_or_default()
                    .replace('\'', "''")
            ))
        }
        Some("s_within") => {
            let args = get_args(filter)?;
            let geom = &args[1];
            Ok(format!(
                "ST_Within(geometry, ST_GeomFromGeoJSON('{}'))",
                serde_json::to_string(geom)
                    .unwrap_or_default()
                    .replace('\'', "''")
            ))
        }
        Some("s_contains") => {
            let args = get_args(filter)?;
            let geom = &args[1];
            Ok(format!(
                "ST_Contains(geometry, ST_GeomFromGeoJSON('{}'))",
                serde_json::to_string(geom)
                    .unwrap_or_default()
                    .replace('\'', "''")
            ))
        }
        Some("isNull") => {
            let args = get_args(filter)?;
            let prop = extract_property(&args[0])?;
            Ok(format!("properties->>'{}' IS NULL", sanitize_field(&prop)))
        }
        Some(unknown) => Err(Cql2Error::Bad(format!(
            "unsupported CQL2 operator: {unknown}"
        ))),
        None => {
            // Might be a simple equality shorthand: {"property": "value"}
            Err(Cql2Error::Bad("filter must have an 'op' field".into()))
        }
    }
}

fn get_args(filter: &serde_json::Value) -> Result<Vec<serde_json::Value>, Cql2Error> {
    filter
        .get("args")
        .and_then(|a| a.as_array())
        .cloned()
        .ok_or(Cql2Error::Bad("missing 'args' array".into()))
}

fn binary_op(filter: &serde_json::Value, sql_op: &str) -> Result<String, Cql2Error> {
    let args = get_args(filter)?;
    let prop = extract_property(&args[0])?;
    let val = extract_literal(&args[1])?;
    Ok(format!(
        "(properties->>'{prop}') {sql_op} {val}",
        prop = sanitize_field(&prop),
        sql_op = sql_op,
        val = sanitize_value(&val)
    ))
}

fn extract_property(v: &serde_json::Value) -> Result<String, Cql2Error> {
    if let Some(prop) = v.get("property").and_then(|p| p.as_str()) {
        Ok(prop.to_string())
    } else if let Some(s) = v.as_str() {
        Ok(s.to_string())
    } else {
        Err(Cql2Error::Bad("expected property reference".into()))
    }
}

fn extract_literal(v: &serde_json::Value) -> Result<String, Cql2Error> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        _ => Err(Cql2Error::Bad("expected literal value".into())),
    }
}

/// Sanitize field names (prevent SQL injection in property names).
fn sanitize_field(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// Sanitize literal values.
fn sanitize_value(val: &str) -> String {
    if val.parse::<f64>().is_ok() || val == "true" || val == "false" {
        val.to_string()
    } else {
        format!("'{}'", val.replace('\'', "''"))
    }
}

// ─── OGC Tiles ──────────────────────────────────────────────────────

async fn tile_matrix_sets() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "tileMatrixSets": [
            {
                "id": "WebMercatorQuad",
                "title": "Google Maps Compatible",
                "uri": "http://www.opengis.net/def/tilematrixset/OGC/1.0/WebMercatorQuad",
                "crs": "http://www.opengis.net/def/crs/EPSG/0/3857"
            },
            {
                "id": "WorldCRS84Quad",
                "title": "CRS84 for the World",
                "uri": "http://www.opengis.net/def/tilematrixset/OGC/1.0/WorldCRS84Quad",
                "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
            }
        ]
    }))
}

async fn tile_matrix_set(Path(tms): Path<String>) -> Result<Json<serde_json::Value>, Cql2Error> {
    match tms.as_str() {
        "WebMercatorQuad" => Ok(Json(serde_json::json!({
            "id": "WebMercatorQuad",
            "title": "Google Maps Compatible for the World",
            "crs": "http://www.opengis.net/def/crs/EPSG/0/3857",
            "wellKnownScaleSet": "http://www.opengis.net/def/wkss/OGC/1.0/GoogleMapsCompatible",
            "tileMatrices": (0..23).map(|z| serde_json::json!({
                "id": z.to_string(),
                "scaleDenominator": 559082264.0 / (1u64 << z) as f64,
                "cellSize": 156543.03392804097 / (1u64 << z) as f64,
                "tileWidth": 256,
                "tileHeight": 256,
                "matrixWidth": 1u64 << z,
                "matrixHeight": 1u64 << z,
            })).collect::<Vec<_>>(),
        }))),
        "WorldCRS84Quad" => Ok(Json(serde_json::json!({
            "id": "WorldCRS84Quad",
            "title": "CRS84 for the World",
            "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
            "tileMatrices": (0..18).map(|z| serde_json::json!({
                "id": z.to_string(),
                "scaleDenominator": 279541132.0 / (1u64 << z) as f64,
                "tileWidth": 256,
                "tileHeight": 256,
            })).collect::<Vec<_>>(),
        }))),
        _ => Err(Cql2Error::Bad(format!("unknown tile matrix set: {tms}"))),
    }
}

/// Serve an OGC vector tile (MVT format).
#[derive(Deserialize)]
#[allow(dead_code)]
struct TileParams {
    id: Uuid,
    tms: String,
    z: i32,
    x: i32,
    y: i32,
}

async fn ogc_tile(
    State(store): State<AppState>,
    Path((dataset_id, _tms, z, x, y)): Path<(Uuid, String, i32, i32, i32)>,
) -> Result<axum::response::Response, Cql2Error> {
    let row = sqlx::query(
        "SELECT ST_AsMVT(tile, 'default', 4096, 'geom') as mvt
         FROM (
            SELECT ST_AsMVTGeom(
                f.geometry,
                ST_TileEnvelope($2, $3, $4),
                4096, 64, true
            ) as geom, f.properties
            FROM features f
            JOIN branches b ON f.branch_id = b.id
            WHERE b.dataset_id = $1
              AND ST_Intersects(f.geometry, ST_TileEnvelope($2, $3, $4))
         ) tile",
    )
    .bind(dataset_id)
    .bind(z)
    .bind(x)
    .bind(y)
    .fetch_one(store.pool())
    .await?;

    let mvt: Vec<u8> = row.get("mvt");
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/vnd.mapbox-vector-tile"),
            ("cache-control", "public, max-age=3600"),
        ],
        mvt,
    )
        .into_response())
}

enum Cql2Error {
    Db(sqlx::Error),
    Bad(String),
}
impl From<sqlx::Error> for Cql2Error {
    fn from(e: sqlx::Error) -> Self {
        Cql2Error::Db(e)
    }
}
impl IntoResponse for Cql2Error {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            Cql2Error::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            Cql2Error::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
