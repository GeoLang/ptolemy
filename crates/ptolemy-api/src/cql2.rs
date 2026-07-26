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

/// Most rows one filter call will return. A caller that wants more has to page
/// with `offset`; without a ceiling a single request can pull the whole branch.
const MAX_LIMIT: i64 = 10_000;

/// CQL2 filter request. Only CQL2-JSON is parsed, see [`check_filter_lang`].
#[derive(Deserialize)]
struct Cql2FilterRequest {
    /// CQL2-JSON filter object
    filter: serde_json::Value,
    #[serde(default = "default_filter_lang")]
    filter_lang: String,
    limit: Option<i64>,
    offset: Option<i64>,
}
fn default_filter_lang() -> String {
    "cql2-json".into()
}

/// The body is parsed as JSON, so a cql2-text filter would fail somewhere deep
/// in the parser with a confusing message. Say so up front instead.
fn check_filter_lang(lang: &str) -> Result<(), Cql2Error> {
    if lang.eq_ignore_ascii_case("cql2-json") {
        Ok(())
    } else {
        Err(Cql2Error::Bad(format!(
            "filter_lang '{lang}' is not supported, only cql2-json is"
        )))
    }
}

/// Reject out-of-range paging before it reaches PostgreSQL, which rejects a
/// negative LIMIT with a query error that would surface as a 500.
fn check_paging(limit: i64, offset: i64) -> Result<(), Cql2Error> {
    if limit < 0 || offset < 0 {
        return Err(Cql2Error::Bad(
            "limit and offset must not be negative".into(),
        ));
    }
    if limit > MAX_LIMIT {
        return Err(Cql2Error::Bad(format!(
            "limit {limit} exceeds the maximum of {MAX_LIMIT}, page with offset instead"
        )));
    }
    Ok(())
}

async fn cql2_filter(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<Cql2FilterRequest>,
) -> Result<Json<serde_json::Value>, Cql2Error> {
    check_filter_lang(&req.filter_lang)?;
    let limit = req.limit.unwrap_or(100);
    let offset = req.offset.unwrap_or(0);
    check_paging(limit, offset)?;

    // Parse CQL2-JSON filter into SQL WHERE clause. branch_id is $1, so the
    // filter's own values start at $2 and limit/offset come after them.
    let (where_clause, binds) = cql2_to_sql(&req.filter, 1)?;
    let limit_param = binds.len() + 2;
    let offset_param = binds.len() + 3;

    // an external dataset swaps the view for a derived table in the same shape,
    // so the filter SQL built above needs no special case
    let (external, source) = store.features_source(branch_id).await?;

    let query = format!(
        "SELECT id, dataset_id, properties, ST_AsGeoJSON(geometry)::jsonb as geojson
         FROM {source} f
         WHERE branch_id = $1 AND ({where_clause})
         LIMIT ${limit_param} OFFSET ${offset_param}"
    );

    let mut q = sqlx::query(&query).bind(branch_id);
    for value in &binds {
        q = q.bind(value.as_str());
    }
    let rows = q
        .bind(limit)
        .bind(offset)
        .fetch_all(store.read_pool(external.as_ref()).await?)
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
            let property = extract_property(&args[0])?;
            // the spec form is args: [prop, [a, b]]; the flat form
            // args: [prop, a, b] is what this endpoint accepted first
            let items: Vec<serde_json::Value> = match args[1].as_array() {
                Some(list) if args.len() == 2 => list.clone(),
                _ => args[1..].to_vec(),
            };
            // nothing is in an empty set, and IN () is a syntax error. Return
            // before binding anything, so the bind list still matches the SQL.
            if items.is_empty() {
                return Ok("FALSE".into());
            }
            let prop = binds.add(property);
            let values: Vec<String> = items
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
    let lower = geom_type.to_ascii_lowercase();
    match lower.as_str() {
        "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" => {
            let coords = obj
                .get("coordinates")
                .filter(|c| c.is_array())
                .ok_or_else(|| {
                    Cql2Error::Bad(format!("'{op}' geometry has no 'coordinates' array"))
                })?;
            check_coords(coords, coord_depth(&lower), op)?;
            check_rings(coords, &lower, op)?;
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

/// How deeply a type nests its positions: 0 is a bare position, 1 an array of
/// positions, and so on.
fn coord_depth(lower_type: &str) -> usize {
    match lower_type {
        "point" => 0,
        "linestring" | "multipoint" => 1,
        "polygon" | "multilinestring" => 2,
        _ => 3, // multipolygon
    }
}

/// Check the coordinate nesting before PostGIS sees it. `ST_GeomFromGeoJSON`
/// reports a malformed array as a query error, which would surface as a 500
/// for what is a bad request.
fn check_coords(v: &serde_json::Value, depth: usize, op: &str) -> Result<(), Cql2Error> {
    let bad = |what: &str| Cql2Error::Bad(format!("'{op}' geometry has {what}"));
    let arr = v
        .as_array()
        .ok_or_else(|| bad("a coordinates entry that is not an array"))?;
    if depth == 0 {
        if !(2..=3).contains(&arr.len()) {
            return Err(bad("a position that is not 2 or 3 numbers"));
        }
        if !arr.iter().all(|n| n.as_f64().is_some_and(f64::is_finite)) {
            return Err(bad("a position with a non-numeric ordinate"));
        }
        return Ok(());
    }
    if arr.is_empty() {
        return Err(bad("an empty coordinates array"));
    }
    for inner in arr {
        check_coords(inner, depth - 1, op)?;
    }
    Ok(())
}

/// Polygon rings must be closed and hold at least four positions. Runs after
/// [`check_coords`], so every level is known to be an array by now.
fn check_rings(coords: &serde_json::Value, lower_type: &str, op: &str) -> Result<(), Cql2Error> {
    let rings: Vec<&serde_json::Value> = match lower_type {
        "polygon" => coords.as_array().into_iter().flatten().collect(),
        "multipolygon" => coords
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_array())
            .flatten()
            .collect(),
        _ => return Ok(()),
    };
    for ring in rings {
        let positions = ring.as_array().map_or(0, Vec::len);
        let closed = ring.as_array().is_some_and(|r| r.first() == r.last());
        if positions < 4 || !closed {
            return Err(Cql2Error::Bad(format!(
                "'{op}' polygon ring must be closed and have at least 4 positions"
            )));
        }
    }
    Ok(())
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
    // the branch join lives in ptolemy's own database, so an external dataset
    // gets a query that only touches the team's relation
    let external = store.external_for_dataset(dataset_id).await?;
    let from_clause = match &external {
        None => {
            "features f JOIN branches b ON f.branch_id = b.id WHERE b.dataset_id = $1".to_string()
        }
        Some(ext) => format!(
            "{} f WHERE f.dataset_id = $1",
            ext.features_subquery("NULL")
        ),
    };
    let sql = format!(
        "SELECT ST_AsMVT(tile, 'default', 4096, 'geom') as mvt
         FROM (
            SELECT ST_AsMVTGeom(
                f.geometry,
                ST_TileEnvelope($2, $3, $4),
                4096, 64, true
            ) as geom, f.properties
            FROM {from_clause}
              AND ST_Intersects(f.geometry, ST_TileEnvelope($2, $3, $4))
         ) tile"
    );
    let row = sqlx::query(&sql)
        .bind(dataset_id)
        .bind(z)
        .bind(x)
        .bind(y)
        .fetch_one(store.read_pool(external.as_ref()).await?)
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
    Store(ptolemy_storage::StoreError),
    Bad(String),
}
impl From<sqlx::Error> for Cql2Error {
    fn from(e: sqlx::Error) -> Self {
        Cql2Error::Db(e)
    }
}
impl From<ptolemy_storage::StoreError> for Cql2Error {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        Cql2Error::Store(e)
    }
}
impl IntoResponse for Cql2Error {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            Cql2Error::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            Cql2Error::Store(e) => crate::errors::store_error_status(&e),
            Cql2Error::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
