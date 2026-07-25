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
    // Parse CQL2-JSON filter into SQL WHERE clause. branch_id is $1, so the
    // filter's own values start at $2 and limit/offset come after them.
    let (where_clause, binds) = cql2_to_sql(&req.filter, 1)?;
    let limit = req.limit.unwrap_or(100);
    let offset = req.offset.unwrap_or(0);
    let limit_param = binds.len() + 2;
    let offset_param = binds.len() + 3;

    let query = format!(
        "SELECT id, dataset_id, properties, ST_AsGeoJSON(geometry)::jsonb as geojson
         FROM features
         WHERE branch_id = $1 AND ({where_clause})
         LIMIT ${limit_param} OFFSET ${offset_param}"
    );

    let mut q = sqlx::query(&query).bind(branch_id);
    for value in &binds {
        q = q.bind(value.as_str());
    }
    let rows = q.bind(limit).bind(offset).fetch_all(store.pool()).await?;

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

/// Collects the bind values for a WHERE fragment. Placeholders continue the
/// caller's numbering, so `offset` is how many parameters it already bound.
struct Binds {
    values: Vec<String>,
    offset: usize,
}

impl Binds {
    fn new(offset: usize) -> Self {
        Binds {
            values: Vec::new(),
            offset,
        }
    }

    /// Take a value and return the placeholder that stands for it, e.g. "$2".
    fn add(&mut self, value: impl Into<String>) -> String {
        self.values.push(value.into());
        format!("${}", self.offset + self.values.len())
    }
}

/// Rows whose text isn't numeric must yield NULL rather than fail the cast.
/// Not attacker data: this is only ever interpolated as a constant.
const NUMERIC_TEXT_RE: &str = r"^-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?$";

/// Convert CQL2-JSON filter to a SQL WHERE clause plus the values to bind.
/// Supports: eq, lt, gt, lte, gte, like, between, in, and, or, not, s_intersects, s_within.
/// Nothing from the request is interpolated into the SQL: property names,
/// literals and GeoJSON all travel as text bind parameters, numbered from
/// `params_bound` + 1.
fn cql2_to_sql(
    filter: &serde_json::Value,
    params_bound: usize,
) -> Result<(String, Vec<String>), Cql2Error> {
    let mut binds = Binds::new(params_bound);
    let sql = filter_to_sql(filter, &mut binds)?;
    Ok((sql, binds.values))
}

fn filter_to_sql(filter: &serde_json::Value, binds: &mut Binds) -> Result<String, Cql2Error> {
    match filter.get("op").and_then(|v| v.as_str()) {
        Some(op @ ("and" | "or")) => {
            let args = get_args(filter, op, 1)?;
            let clauses: Result<Vec<String>, _> =
                args.iter().map(|a| filter_to_sql(a, binds)).collect();
            let sep = if op == "and" { " AND " } else { " OR " };
            Ok(format!("({})", clauses?.join(sep)))
        }
        Some("not") => {
            let args = get_args(filter, "not", 1)?;
            let inner = filter_to_sql(&args[0], binds)?;
            Ok(format!("NOT ({})", inner))
        }
        Some(op @ ("=" | "eq")) => binary_op(filter, op, "=", binds),
        Some(op @ ("<" | "lt")) => binary_op(filter, op, "<", binds),
        Some(op @ (">" | "gt")) => binary_op(filter, op, ">", binds),
        Some(op @ ("<=" | "lte")) => binary_op(filter, op, "<=", binds),
        Some(op @ (">=" | "gte")) => binary_op(filter, op, ">=", binds),
        Some(op @ ("!=" | "neq")) => binary_op(filter, op, "!=", binds),
        Some("like") => {
            let args = get_args(filter, "like", 2)?;
            let prop = binds.add(extract_property(&args[0])?);
            let pattern = binds.add(extract_literal(&args[1])?);
            Ok(format!("properties->>{prop} LIKE {pattern}"))
        }
        Some("between") => {
            let args = get_args(filter, "between", 3)?;
            let prop = binds.add(extract_property(&args[0])?);
            let low = binds.add(numeric_literal(&args[1], "between")?);
            let high = binds.add(numeric_literal(&args[2], "between")?);
            // same guarded cast as binary_op: non-numeric text yields NULL, not an error
            Ok(format!(
                "{lhs} BETWEEN {low}::numeric AND {high}::numeric",
                lhs = numeric_lhs(&prop)
            ))
        }
        Some("in") => {
            let args = get_args(filter, "in", 2)?;
            let prop = binds.add(extract_property(&args[0])?);
            let values: Vec<String> = args[1..]
                .iter()
                .map(|v| extract_literal(v).map(|s| binds.add(s)))
                .collect::<Result<_, _>>()?;
            Ok(format!("properties->>{prop} IN ({})", values.join(", ")))
        }
        Some(op @ ("s_intersects" | "s_within" | "s_contains")) => {
            let args = get_args(filter, op, 2)?;
            require_geometry_column(&args[0], op)?;
            let geom = binds.add(geojson_geometry(&args[1], op)?.to_string());
            let func = match op {
                "s_intersects" => "ST_Intersects",
                "s_within" => "ST_Within",
                _ => "ST_Contains",
            };
            Ok(format!("{func}(geometry, ST_GeomFromGeoJSON({geom}))"))
        }
        Some("isNull") => {
            let args = get_args(filter, "isNull", 1)?;
            let prop = binds.add(extract_property(&args[0])?);
            Ok(format!("properties->>{prop} IS NULL"))
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

/// Fetch 'args' and check the arity up front, since indexing a short array
/// would panic on a request an attacker controls.
fn get_args(
    filter: &serde_json::Value,
    op: &str,
    min: usize,
) -> Result<Vec<serde_json::Value>, Cql2Error> {
    let args = filter
        .get("args")
        .and_then(|a| a.as_array())
        .cloned()
        .ok_or_else(|| Cql2Error::Bad(format!("'{op}' requires an 'args' array")))?;
    if args.len() < min {
        return Err(Cql2Error::Bad(format!(
            "'{op}' requires at least {min} argument(s), got {}",
            args.len()
        )));
    }
    Ok(args)
}

/// jsonb ->> yields text; numeric literals need a cast on the property side.
/// Guard the cast so rows holding non-numeric text yield NULL (excluded)
/// instead of erroring the whole query.
fn numeric_lhs(prop: &str) -> String {
    format!(
        "CASE WHEN properties->>{prop} ~ '{NUMERIC_TEXT_RE}' \
         THEN (properties->>{prop})::numeric END"
    )
}

fn binary_op(
    filter: &serde_json::Value,
    op: &str,
    sql_op: &str,
    binds: &mut Binds,
) -> Result<String, Cql2Error> {
    let args = get_args(filter, op, 2)?;
    let prop = binds.add(extract_property(&args[0])?);
    let val = binds.add(extract_literal(&args[1])?);
    if args[1].is_number() {
        Ok(format!(
            "{lhs} {sql_op} {val}::numeric",
            lhs = numeric_lhs(&prop)
        ))
    } else {
        Ok(format!("(properties->>{prop}) {sql_op} {val}"))
    }
}

/// between compares numerically, so reject non-numeric bounds here instead of
/// letting the cast fail halfway through the query.
fn numeric_literal(v: &serde_json::Value, op: &str) -> Result<String, Cql2Error> {
    let literal = extract_literal(v)?;
    if literal.parse::<f64>().is_ok() {
        Ok(literal)
    } else {
        Err(Cql2Error::Bad(format!("'{op}' bounds must be numbers")))
    }
}

/// The features view has one geometry column, so args[0] of a spatial op must
/// name it. Anything else is a client mistake; don't silently filter on the
/// wrong column.
fn require_geometry_column(v: &serde_json::Value, op: &str) -> Result<(), Cql2Error> {
    let prop = extract_property(v)?;
    if prop.eq_ignore_ascii_case("geometry") || prop.eq_ignore_ascii_case("geom") {
        Ok(())
    } else {
        Err(Cql2Error::Bad(format!(
            "'{op}' first argument must reference the 'geometry' column, got '{prop}'"
        )))
    }
}

/// Check that the value is a GeoJSON geometry and rebuild it from the parsed
/// JSON, so only recognised members reach PostGIS. Dropping the (RFC 7946
/// removed) "crs" member also keeps the result in 4326, matching the column.
fn geojson_geometry(v: &serde_json::Value, op: &str) -> Result<serde_json::Value, Cql2Error> {
    let obj = v.as_object().ok_or_else(|| {
        Cql2Error::Bad(format!(
            "'{op}' second argument must be a GeoJSON geometry object"
        ))
    })?;
    let geom_type = obj
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| Cql2Error::Bad(format!("'{op}' geometry has no 'type' member")))?;
    // PostGIS matches the type case-insensitively, so accept the client's spelling
    match geom_type.to_ascii_lowercase().as_str() {
        "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" => {
            let coords = obj
                .get("coordinates")
                .filter(|c| c.is_array())
                .ok_or_else(|| {
                    Cql2Error::Bad(format!("'{op}' geometry has no 'coordinates' array"))
                })?;
            Ok(serde_json::json!({"type": geom_type, "coordinates": coords}))
        }
        "geometrycollection" => {
            let geometries = obj
                .get("geometries")
                .and_then(|g| g.as_array())
                .ok_or_else(|| {
                    Cql2Error::Bad(format!("'{op}' geometry has no 'geometries' array"))
                })?;
            let inner: Result<Vec<serde_json::Value>, _> =
                geometries.iter().map(|g| geojson_geometry(g, op)).collect();
            Ok(serde_json::json!({"type": geom_type, "geometries": inner?}))
        }
        _ => Err(Cql2Error::Bad(format!(
            "'{op}' unsupported GeoJSON geometry type"
        ))),
    }
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
