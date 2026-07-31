// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! An ArcGIS FeatureServer (Geoservices REST) frontend, so an Esri client — an
//! ArcGIS JS API web map, QGIS's ArcGIS REST connector, verne's extractor — can
//! point at ptolemy without knowing what it is talking to.
//!
//! One dataset is one single-layer service and the layer is always id 0. That
//! is the shape a hosted feature layer has, and it leaves no layer id that
//! could move when datasets come and go.
//!
//! Reads and writes always run on the dataset's `main` branch: nothing in the
//! protocol can name a branch, so there is one answer and it is the obvious one.
//!
//! `applyEdits` is the one route here that writes. Every edit in one request
//! becomes one commit through the same store path `/api/v1` commits take, so a
//! batch either lands whole or does not land: see `apply_edits` for what that
//! costs a client that expects Esri's per-row results.
//!
//! Every route resolves its dataset through the same visibility rule the other
//! read frontends use, so this widens nothing. A URL that names a dataset by
//! uuid is also caught by the visibility layer, which answers its own 404 before
//! any handler runs: a private dataset is refused either way, but only the
//! name-addressed URL is refused in the Geoservices error shape.
//!
//! Query parameters that carry meaning and cannot be honored are refused rather
//! than ignored: a filter that silently did not apply is worse than an error,
//! because the client believes the rows it got back are the rows it asked for.
//!
//! `where` is the one parameter with a grammar behind it rather than a value.
//! The `where_clause` module parses the SQL-92 subset Esri clients send and
//! renders it as SQL with every literal bound, and refuses the rest by name for
//! the reason above.

use axum::{
    Json, Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ptolemy_core::diff::DiffOp;
use ptolemy_core::schema::{FieldDef, FieldType};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

mod where_clause;

pub fn arcgis_routes() -> Router<AppState> {
    Router::new()
        .route("/arcgis/rest/services", get(catalog))
        .route(
            "/arcgis/rest/services/{service}/FeatureServer",
            get(service_root),
        )
        .route(
            "/arcgis/rest/services/{service}/FeatureServer/{layer}",
            get(layer_metadata),
        )
        .route(
            "/arcgis/rest/services/{service}/FeatureServer/{layer}/query",
            get(query_get).post(query_post),
        )
        .route(
            "/arcgis/rest/services/{service}/FeatureServer/{layer}/applyEdits",
            post(apply_edits),
        )
        // scoped to the facade, not the whole API: an org's web maps run on
        // their own origins, and the ArcGIS JS API refuses a server that
        // does not answer CORS, which real ArcGIS servers do
        .layer(tower_http::cors::CorsLayer::permissive())
}

/// The REST version a client is told it is talking to. Clients gate features on
/// it, so it names a real release rather than ptolemy's own version.
const CURRENT_VERSION: f64 = 11.2;

/// The most features one query answers with. A client reads it off the layer and
/// pages by it, and `resultRecordCount` is clamped to it.
const MAX_RECORD_COUNT: i64 = 1000;

/// The only layer id a dataset's service has.
const LAYER_ID: &str = "0";

/// Every geometry this service stores is in EPSG:4326, and a response says its
/// reference once rather than once per geometry.
fn spatial_reference() -> Value {
    json!({"wkid": 4326, "latestWkid": 4326})
}

/// The reference a Web Mercator answer declares: Esri's own 102100 name first,
/// because that is the id a web client asked with and compares against.
fn mercator_reference() -> Value {
    json!({"wkid": 102100, "latestWkid": 3857})
}

/// The srid a client-supplied spatial reference names, normalised to the two
/// this service speaks: 4326 as stored, 3857 for 3857 or its 102100 alias.
/// `None` is a reference this service will not guess at.
fn known_srid(value: &str) -> Option<i32> {
    let wkid = match value.parse::<i64>() {
        Ok(code) => Some(code),
        Err(_) => serde_json::from_str::<Value>(value).ok().and_then(|v| {
            v.get("latestWkid")
                .or_else(|| v.get("wkid"))
                .and_then(Value::as_i64)
        }),
    }?;
    match wkid {
        4326 => Some(4326),
        3857 | 102100 => Some(3857),
        _ => None,
    }
}

// ─── Errors ─────────────────────────────────────────────────────────

/// Geoservices reports a refused request as HTTP 200 with an `error` object in
/// the body. A client reads the body and never the status, so answering 400
/// would look like a broken server rather than a rejected request. `code` is the
/// HTTP-shaped code the request would have got.
#[derive(Debug)]
struct EsriError {
    code: u16,
    message: String,
}

impl EsriError {
    fn bad_request(message: impl Into<String>) -> Self {
        EsriError {
            code: 400,
            message: message.into(),
        }
    }

    /// The only thing a caller is told about a database failure. The cause goes
    /// to the log, as everywhere else in this crate.
    fn internal(context: &str, error: impl std::fmt::Display) -> Self {
        tracing::error!("arcgis {context}: {error}");
        EsriError {
            code: 500,
            message: "internal error".into(),
        }
    }

    /// A store refusal as the code the rest of the API answers it with, so a
    /// denial reads as a denial rather than as an internal error. Used on the
    /// write path, where a refusal is the caller's answer. A read's store error
    /// is a bug here and stays a 500 through [`From`].
    fn refused(error: ptolemy_storage::StoreError) -> Self {
        let (status, message) = crate::errors::store_error_status(&error);
        EsriError {
            code: status.as_u16(),
            message,
        }
    }
}

/// A Geoservices `error` body, as the response it is served in.
///
/// Shared with the auth layer, which has to refuse a request on this facade in
/// this shape: an Esri client reads the body and never the status, so a 401 with
/// a different body reads as a broken server and the client never asks its user
/// for credentials.
pub(crate) fn error_response(code: u16, message: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "error": {
                "code": code,
                "message": message,
                "details": [],
            }
        })),
    )
        .into_response()
}

impl From<sqlx::Error> for EsriError {
    fn from(e: sqlx::Error) -> Self {
        EsriError::internal("query", e)
    }
}

impl From<ptolemy_storage::StoreError> for EsriError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        EsriError::internal("store", e)
    }
}

impl IntoResponse for EsriError {
    fn into_response(self) -> Response {
        error_response(self.code, &self.message)
    }
}

// ─── Parameters ─────────────────────────────────────────────────────

/// The request's parameters, from the query string on a GET and the form body on
/// a POST. Kept as pairs rather than deserialized into a struct so that a
/// parameter this facade does not implement can still be seen and refused.
struct Params(Vec<(String, String)>);

impl Params {
    /// Esri's own services match parameter names without regard to case, and
    /// clients rely on it.
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(held, _)| held.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Absent, empty, `false` and `0` all mean "not asked for". Anything else
    /// that is not `true` or `1` is a client bug, not a default.
    fn flag(&self, name: &str) -> Result<bool, EsriError> {
        match self.get(name).map(str::trim) {
            None | Some("") => Ok(false),
            Some(v) if v.eq_ignore_ascii_case("false") || v == "0" => Ok(false),
            Some(v) if v.eq_ignore_ascii_case("true") || v == "1" => Ok(true),
            Some(other) => Err(EsriError::bad_request(format!(
                "{name} must be true or false, not '{other}'"
            ))),
        }
    }

    /// Present with a value that asks for something. A falsy value asks for
    /// nothing, so it is not a refusal.
    fn asks_for(&self, name: &str) -> bool {
        matches!(self.flag(name), Ok(true) | Err(_))
    }
}

/// Parameters that change which rows or which values a query answers with. None
/// of them can be honored here, and answering as though the parameter were
/// absent would hand back the wrong rows under the client's own filter, so each
/// is refused by name.
const UNSUPPORTED: [&str; 15] = [
    "outStatistics",
    "groupByFieldsForStatistics",
    "having",
    "returnDistinctValues",
    "returnExtentOnly",
    "returnZ",
    "returnM",
    "gdbVersion",
    "historicMoment",
    "time",
    "distance",
    "units",
    "quantizationParameters",
    "geometryPrecision",
    "datumTransformation",
];

/// The response encoding a query answers in. `pjson` is `json` a browser can
/// read; nothing downstream distinguishes them, so it is an alias.
///
/// Absent means esriJSON. Esri's own default is `html`, which a browser gets and
/// nothing else wants; every client here sends `f` anyway, and a client that
/// forgets is better served JSON than an error.
#[derive(Clone, Copy)]
enum Format {
    Esri,
    GeoJson,
}

fn format_of(params: &Params) -> Result<Format, EsriError> {
    match params.get("f").map(str::trim).unwrap_or("json") {
        "json" | "pjson" | "" => Ok(Format::Esri),
        other if other.eq_ignore_ascii_case("geojson") => Ok(Format::GeoJson),
        other => Err(EsriError::bad_request(format!(
            "unsupported format '{other}'; this service answers f=json, f=pjson and f=geojson"
        ))),
    }
}

/// Metadata has one encoding: `geoJSON` describes features, and a catalog, a
/// service root and a layer definition are not features.
fn require_esri_json(params: &Params) -> Result<(), EsriError> {
    match format_of(params)? {
        Format::Esri => Ok(()),
        Format::GeoJson => Err(EsriError::bad_request(
            "f=geojson describes features; this resource answers f=json and f=pjson",
        )),
    }
}

// ─── Fields ─────────────────────────────────────────────────────────

/// How a declared field's value is read out of a feature's properties.
///
/// The mapping loses information in three places, and each loss is what the
/// declared Esri type forces:
///   - a boolean becomes the string `"true"` or `"false"`; Esri has no boolean
///     field type, and an integer 0/1 would read as a number to a client
///   - an array or an object becomes its JSON text; Esri has no nested value
///   - a float is a double, so an integer-valued float comes back as a double
///
/// A field with no ptolemy schema behind it is a string, whatever its values look
/// like: nothing declared a type, and guessing one from a sample would make the
/// layer's schema depend on which rows exist.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Oid,
    Text,
    Integer,
    Double,
    BooleanText,
    JsonText,
}

impl Kind {
    fn esri(self) -> &'static str {
        match self {
            Kind::Oid => "esriFieldTypeOID",
            Kind::Integer => "esriFieldTypeInteger",
            Kind::Double => "esriFieldTypeDouble",
            Kind::Text | Kind::BooleanText | Kind::JsonText => "esriFieldTypeString",
        }
    }

    fn of(field_type: &FieldType) -> Kind {
        match field_type {
            FieldType::String => Kind::Text,
            FieldType::Integer => Kind::Integer,
            FieldType::Float => Kind::Double,
            FieldType::Boolean => Kind::BooleanText,
            FieldType::Array | FieldType::Object => Kind::JsonText,
        }
    }
}

struct Field {
    name: String,
    alias: String,
    kind: Kind,
}

impl Field {
    /// `editable` is the layer's answer, not the field's: an editable layer
    /// declares every field but the object id editable, and the id never is,
    /// because it is the key a client holds the feature by.
    fn declaration(&self, editable: bool) -> Value {
        let mut out = json!({
            "name": self.name,
            "type": self.kind.esri(),
            "alias": self.alias,
            "nullable": self.kind != Kind::Oid,
            "editable": editable && self.kind != Kind::Oid,
            "domain": Value::Null,
            "defaultValue": Value::Null,
        });
        if self.kind.esri() == "esriFieldTypeString" {
            out["length"] = json!(2048);
        }
        out
    }

    /// The field's value for one feature, as the declared type.
    fn value(&self, properties: &Value) -> Value {
        let held = properties.get(&self.name);
        match (self.kind, held) {
            (_, None) | (_, Some(Value::Null)) => Value::Null,
            (Kind::Oid, Some(v)) => v.clone(),
            (Kind::Integer, Some(v)) => match v {
                Value::Number(_) => v.clone(),
                Value::String(s) => s.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
                _ => Value::Null,
            },
            (Kind::Double, Some(v)) => match v {
                Value::Number(_) => v.clone(),
                Value::String(s) => s.parse::<f64>().map(Value::from).unwrap_or(Value::Null),
                _ => Value::Null,
            },
            (Kind::BooleanText, Some(Value::Bool(b))) => Value::from(b.to_string()),
            (Kind::Text, Some(Value::String(s))) => Value::from(s.clone()),
            // a value that is not what the declared type expects still has to
            // come out as that type, so it comes out as its JSON text
            (_, Some(v)) => Value::from(text_of(v)),
        }
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Where a layer's OBJECTID values come from.
enum Oid {
    /// A ptolemy schema field named `objectid` in any case, of integer type.
    /// Migrated ArcGIS data always carries one and verne records it, so the ids
    /// a client sees are the source's own.
    ///
    /// A feature that has no value for it gets a null id: nothing can be done
    /// about that, and dropping the feature would make the layer's count
    /// disagree with the dataset's. A non-integer value there fails the query
    /// loudly rather than being guessed at.
    Property(String),
    /// No such field, so the id is `ROW_NUMBER()` over feature id order.
    ///
    /// Stable only while no feature is deleted: a delete renumbers everything
    /// after it, so an id a client wrote down last week may name a different
    /// feature today. Read-only v1 accepts that. Nothing but a real objectid
    /// column can fix it.
    RowNumber,
}

impl Oid {
    fn name(&self) -> &str {
        match self {
            Oid::Property(name) => name,
            Oid::RowNumber => "objectid",
        }
    }

    /// SQL for the id, over the alias `f`.
    fn sql(&self) -> String {
        match self {
            Oid::Property(name) => {
                format!("(f.properties->>'{}')::bigint", name.replace('\'', "''"))
            }
            Oid::RowNumber => "ROW_NUMBER() OVER (ORDER BY f.id)".to_string(),
        }
    }
}

// ─── Resolution ─────────────────────────────────────────────────────

/// Everything the metadata and the query both need about the one layer a URL
/// names.
struct Layer {
    name: String,
    /// `esriGeometry*`.
    geometry: &'static str,
    dataset_id: Uuid,
    branch_id: Uuid,
    /// The object id field first, then the dataset's own.
    fields: Vec<Field>,
    oid: Oid,
}

impl Layer {
    fn field(&self, name: &str) -> Option<&Field> {
        self.fields
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// Whether this layer takes edits, which is whether its object ids name a
    /// feature for longer than one delete. The metadata says so too, and a client
    /// hides its edit tools when it says no. See [`Oid::RowNumber`].
    fn editable(&self) -> bool {
        matches!(self.oid, Oid::Property(_))
    }

    fn capabilities(&self) -> &'static str {
        if self.editable() {
            "Query,Create,Update,Delete"
        } else {
            "Query"
        }
    }
}

/// ptolemy's geometry type as an Esri layer type.
///
/// `linestring` and `multilinestring` both become a polyline, and `polygon` and
/// `multipolygon` both a polygon: Esri has one layer type per pair and its
/// encoding is the multi-part one either way, so a single-part feature comes
/// back as a one-part multi.
///
/// `geometry` and `geometrycollection` have none. A layer declares exactly one
/// geometryType and every client draws every feature with it, so there is
/// nothing honest to declare for a dataset whose features differ.
fn esri_geometry_type(stored: &str) -> Option<&'static str> {
    match stored {
        "point" => Some("esriGeometryPoint"),
        "multipoint" => Some("esriGeometryMultipoint"),
        "linestring" | "multilinestring" => Some("esriGeometryPolyline"),
        "polygon" | "multipolygon" => Some("esriGeometryPolygon"),
        _ => None,
    }
}

const MIXED_GEOMETRY: &str = "the dataset holds features of differing geometry types, and an Esri feature layer \
     declares exactly one geometryType for all of them, so it has no layer type here. \
     Read it through /api/v1/ogc or the native API instead.";

/// The dataset a `{service}` segment names: by id when it is a uuid, otherwise
/// by exact name. Both are unique, and the id wins so a dataset cannot be
/// shadowed by another whose name is a uuid.
///
/// A dataset the caller may not read is simply absent, exactly as it is from
/// every other listing.
async fn resolve(
    store: &AppState,
    actor: &Actor,
    service: &str,
    layer: &str,
) -> Result<Layer, EsriError> {
    if layer != LAYER_ID {
        return Err(EsriError::bad_request(format!(
            "the service has one layer, id {LAYER_ID}; there is no layer {layer}"
        )));
    }

    let reader = actor.reader();
    let visible = ptolemy_storage::visible_datasets_sql("d", 1, 2);
    let row = sqlx::query(&format!(
        "SELECT d.id, d.name, d.geometry_type, b.id AS branch_id
           FROM datasets d
           LEFT JOIN branches b ON b.dataset_id = d.id AND b.name = 'main'
          WHERE {visible} AND (d.id IS NOT DISTINCT FROM $3::uuid OR d.name = $4)
          ORDER BY (d.id IS NOT DISTINCT FROM $3::uuid) DESC
          LIMIT 1"
    ))
    .bind(reader.bypass)
    .bind(reader.id.as_deref())
    .bind(Uuid::parse_str(service).ok())
    .bind(service)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or_else(|| EsriError::bad_request(format!("no feature service named '{service}'")))?;

    let dataset_id: Uuid = row.get("id");
    let name: String = row.get("name");
    let stored: String = row.get("geometry_type");
    let geometry = esri_geometry_type(&stored).ok_or_else(|| {
        EsriError::bad_request(format!(
            "'{name}' has geometry_type {stored}: {MIXED_GEOMETRY}"
        ))
    })?;
    let branch_id: Option<Uuid> = row.get("branch_id");
    let branch_id = branch_id.ok_or_else(|| {
        EsriError::bad_request(format!(
            "'{name}' has no branch named 'main', and this service reads that branch"
        ))
    })?;

    let (oid, fields) = fields_of(store, dataset_id, branch_id).await?;
    Ok(Layer {
        name,
        geometry,
        dataset_id,
        branch_id,
        fields,
        oid,
    })
}

/// The layer's fields, from the dataset's schema when it has one and from the
/// property keys its features actually carry when it does not.
///
/// The object id field is always first and always declared, so a client always
/// has a key to page and pair rows by.
async fn fields_of(
    store: &AppState,
    dataset_id: Uuid,
    branch_id: Uuid,
) -> Result<(Oid, Vec<Field>), EsriError> {
    let declared: Vec<FieldDef> = match store.get_dataset_schema(dataset_id).await? {
        Some(schema) => schema.fields,
        None => derived_fields(store, branch_id).await?,
    };

    let oid = declared
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("objectid") && f.field_type == FieldType::Integer)
        .map(|f| Oid::Property(f.name.clone()))
        .unwrap_or(Oid::RowNumber);

    let mut fields = vec![Field {
        name: oid.name().to_string(),
        alias: oid.name().to_string(),
        kind: Kind::Oid,
    }];
    // exactly one field is named objectid: a synthesized id takes that name, so
    // a property of the same name that is not an integer id cannot also have it
    for def in declared {
        if def.name.eq_ignore_ascii_case(oid.name()) {
            continue;
        }
        fields.push(Field {
            alias: def.alias.clone().unwrap_or_else(|| def.name.clone()),
            name: def.name,
            kind: Kind::of(&def.field_type),
        });
    }
    Ok((oid, fields))
}

/// The property keys the branch's features carry, for a dataset with no schema.
/// Every one is a string: see [`Kind`].
async fn derived_fields(store: &AppState, branch_id: Uuid) -> Result<Vec<FieldDef>, EsriError> {
    let (external, source) = store.features_source_at(branch_id, "$1").await?;
    let rows = sqlx::query(&format!(
        "SELECT DISTINCT jsonb_object_keys(f.properties) AS key FROM {source} f ORDER BY key"
    ))
    .bind(branch_id)
    .fetch_all(store.source_pool(external.as_ref()).await?)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| FieldDef {
            name: row.get("key"),
            field_type: FieldType::String,
            required: false,
            alias: None,
            allowed_values: Vec::new(),
            min: None,
            max: None,
        })
        .collect())
}

// ─── Catalog ────────────────────────────────────────────────────────

/// The absolute root a client should build service URLs from. Behind a proxy the
/// forwarded scheme is the real one; the host header is the name the client
/// used, which is the only name it can reach us by.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if host.is_empty() {
        return String::new();
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

fn service_url(base: &str, name: &str) -> String {
    format!(
        "{base}/arcgis/rest/services/{}/FeatureServer",
        urlencoding::encode(name)
    )
}

async fn catalog(
    State(store): State<AppState>,
    actor: Actor,
    headers: HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;

    let reader = actor.reader();
    let visible = ptolemy_storage::visible_datasets_sql("d", 1, 2);
    let rows = sqlx::query(&format!(
        "SELECT d.name, d.geometry_type FROM datasets d WHERE {visible} ORDER BY d.name"
    ))
    .bind(reader.bypass)
    .bind(reader.id.as_deref())
    .fetch_all(store.read_pool())
    .await?;

    let base = base_url(&headers);
    // a mixed-geometry dataset has no Esri layer type, so it is not offered as a
    // service rather than offered as a lie; its own URL says why
    let services: Vec<Value> = rows
        .iter()
        .filter(|row| esri_geometry_type(row.get::<String, _>("geometry_type").as_str()).is_some())
        .map(|row| {
            let name: String = row.get("name");
            json!({
                "name": name,
                "type": "FeatureServer",
                "url": service_url(&base, &name),
            })
        })
        .collect();

    Ok(Json(json!({
        "currentVersion": CURRENT_VERSION,
        "folders": [],
        "services": services,
    })))
}

// ─── Service root ───────────────────────────────────────────────────

async fn service_root(
    State(store): State<AppState>,
    actor: Actor,
    Path(service): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    let layer = resolve(&store, &actor, &service, LAYER_ID).await?;

    Ok(Json(json!({
        "currentVersion": CURRENT_VERSION,
        "serviceDescription": format!("ptolemy dataset '{}', branch main", layer.name),
        "description": "",
        "copyrightText": "",
        "hasVersionedData": false,
        "supportsDisconnectedEditing": false,
        "syncEnabled": false,
        "hasStaticData": false,
        "allowGeometryUpdates": layer.editable(),
        "units": "esriDecimalDegrees",
        "maxRecordCount": MAX_RECORD_COUNT,
        "supportedQueryFormats": "JSON,geoJSON",
        "useStandardizedQueries": true,
        "capabilities": layer.capabilities(),
        "spatialReference": spatial_reference(),
        "layers": [{
            "id": 0,
            "name": layer.name,
            "type": "Feature Layer",
            "geometryType": layer.geometry,
            "parentLayerId": -1,
            "defaultVisibility": true,
            "subLayerIds": Value::Null,
            "minScale": 0,
            "maxScale": 0,
        }],
        "tables": [],
    })))
}

// ─── Layer metadata ─────────────────────────────────────────────────

async fn layer_metadata(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    let layer = resolve(&store, &actor, &service, &layer_id).await?;

    let (external, source) = store.features_source_at(layer.branch_id, "$1").await?;
    let bounds = sqlx::query(&format!(
        "SELECT ST_XMin(ST_Extent(f.geometry)) AS min_x,
                ST_YMin(ST_Extent(f.geometry)) AS min_y,
                ST_XMax(ST_Extent(f.geometry)) AS max_x,
                ST_YMax(ST_Extent(f.geometry)) AS max_y
           FROM {source} f"
    ))
    .bind(layer.branch_id)
    .fetch_one(store.source_pool(external.as_ref()).await?)
    .await?;

    // an empty layer has no extent to state, so the bounds are null and the
    // reference still is not: a client that asks what projection the layer is in
    // gets an answer whether or not there is anything in it yet
    let extent = json!({
        "xmin": bounds.get::<Option<f64>, _>("min_x"),
        "ymin": bounds.get::<Option<f64>, _>("min_y"),
        "xmax": bounds.get::<Option<f64>, _>("max_x"),
        "ymax": bounds.get::<Option<f64>, _>("max_y"),
        "spatialReference": spatial_reference(),
    });

    // what a client puts on a popup: the first field that holds readable text,
    // and the id when there is none
    let display = layer
        .fields
        .iter()
        .find(|f| f.kind == Kind::Text)
        .unwrap_or(&layer.fields[0]);

    Ok(Json(json!({
        "currentVersion": CURRENT_VERSION,
        "id": 0,
        "name": layer.name,
        "type": "Feature Layer",
        "description": "",
        "copyrightText": "",
        "geometryType": layer.geometry,
        "objectIdField": layer.oid.name(),
        "globalIdField": "",
        "displayField": display.name,
        "fields": layer.fields.iter().map(|f| f.declaration(layer.editable())).collect::<Vec<_>>(),
        "extent": extent,
        "maxRecordCount": MAX_RECORD_COUNT,
        "standardMaxRecordCount": MAX_RECORD_COUNT,
        "supportedQueryFormats": "JSON,geoJSON",
        "useStandardizedQueries": true,
        "capabilities": layer.capabilities(),
        "advancedQueryCapabilities": {
            "supportsPagination": true,
            "supportsOrderBy": true,
            "supportsStatistics": false,
            "supportsDistinct": false,
            "supportsQueryAttachments": false,
        },
        "hasAttachments": false,
        "hasZ": false,
        "hasM": false,
        "isDataVersioned": false,
        "allowGeometryUpdates": layer.editable(),
        "defaultVisibility": true,
        "minScale": 0,
        "maxScale": 0,
        "relationships": [],
        "types": [],
        "templates": [],
    })))
}

// ─── Query ──────────────────────────────────────────────────────────

async fn query_get(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    run_query(store, actor, service, layer_id, Params(params)).await
}

async fn query_post(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id)): Path<(String, String)>,
    Form(params): Form<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    run_query(store, actor, service, layer_id, Params(params)).await
}

/// A filter's bind value, so a filter is SQL plus a bind rather than
/// interpolated request data. Every value a `where` clause compares against is
/// one of these: see [`where_clause`].
#[derive(Debug, PartialEq)]
enum Bind {
    Ids(Vec<i64>),
    Number(f64),
    /// `None` is a `NULL` a client wrote, which every comparison answers unknown.
    Text(Option<String>),
    Numbers(Vec<f64>),
    Texts(Vec<Option<String>>),
}

/// The branch as `$1` and then each filter's value in the order the filters were
/// built, which is the order they numbered their placeholders in.
fn bound<'q>(
    sql: &'q str,
    branch_id: Uuid,
    binds: &[Bind],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    let mut query = sqlx::query(sql).bind(branch_id);
    for bind in binds {
        query = match bind {
            Bind::Ids(ids) => query.bind(ids.clone()),
            Bind::Number(v) => query.bind(*v),
            Bind::Text(v) => query.bind(v.clone()),
            Bind::Numbers(v) => query.bind(v.clone()),
            Bind::Texts(v) => query.bind(v.clone()),
        };
    }
    query
}

async fn run_query(
    store: AppState,
    actor: Actor,
    service: String,
    layer_id: String,
    params: Params,
) -> Result<Json<Value>, EsriError> {
    let format = format_of(&params)?;
    for name in UNSUPPORTED {
        if params.asks_for(name) {
            return Err(EsriError::bad_request(format!(
                "{name} is not supported in this version of the service"
            )));
        }
    }
    let layer = resolve(&store, &actor, &service, &layer_id).await?;

    // the id field is the only order the store can promise, and it is the order
    // paging already runs in, so asking for it changes nothing and asking for
    // anything else would be answered in the wrong order
    if let Some(order) = params.get("orderByFields").map(str::trim)
        && !order.is_empty()
    {
        let (field, direction) = match order.split_once(char::is_whitespace) {
            Some((field, rest)) => (field, rest.trim()),
            None => (order, ""),
        };
        if !field.eq_ignore_ascii_case(layer.oid.name())
            || !(direction.is_empty() || direction.eq_ignore_ascii_case("asc"))
        {
            return Err(EsriError::bad_request(format!(
                "orderByFields is not supported in this version of the service, except as \
                 '{}' or '{} ASC': '{order}'",
                layer.oid.name(),
                layer.oid.name()
            )));
        }
    }

    let out_srid = sr_srid(&params, "outSR")?.unwrap_or(4326);
    // RFC 7946 says GeoJSON is 4326, so a mercator FeatureCollection would be
    // a lie whichever reference it claimed
    if out_srid != 4326 && matches!(format, Format::GeoJson) {
        return Err(EsriError::bad_request(
            "f=geojson serves EPSG:4326 only; ask for f=json to get Web Mercator",
        ));
    }

    let return_geometry = match params.get("returnGeometry") {
        None => true,
        Some(_) => params.flag("returnGeometry")?,
    };
    let count_only = params.flag("returnCountOnly")?;
    let ids_only = params.flag("returnIdsOnly")?;

    let out_fields = out_fields(&layer, params.get("outFields"))?;

    let mut filters: Vec<String> = Vec::new();
    let mut binds: Vec<Bind> = Vec::new();
    let mut next = 2;

    // the clause is parsed against the layer's own fields and rendered with
    // every literal bound, so nothing a client wrote reaches the SQL
    if let Some(clause) = params
        .get("where")
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
    {
        let predicate = where_clause::parse(clause, &layer).map_err(|why| {
            EsriError::bad_request(format!(
                "where '{}' is not supported in this version of the service: {why}",
                where_clause::shown(clause)
            ))
        })?;
        filters.push(predicate.sql(&mut next, &mut binds));
    }

    if let Some(list) = params.get("objectIds").map(str::trim)
        && !list.is_empty()
    {
        let ids: Result<Vec<i64>, _> = list.split(',').map(|s| s.trim().parse::<i64>()).collect();
        let ids = ids.map_err(|_| {
            EsriError::bad_request(format!(
                "objectIds must be a comma-separated list of integers: '{list}'"
            ))
        })?;
        filters.push(format!("oid = ANY(${next}::bigint[])"));
        binds.push(Bind::Ids(ids));
        next += 1;
    }

    if let Some((envelope, in_srid)) = envelope(&params)? {
        // a mercator envelope is transformed to the stored reference instead
        // of the other way round: it is four numbers against a whole column
        filters.push(if in_srid == 4326 {
            format!(
                "geometry && ST_MakeEnvelope(${}, ${}, ${}, ${}, 4326)",
                next,
                next + 1,
                next + 2,
                next + 3
            )
        } else {
            format!(
                "geometry && ST_Transform(ST_MakeEnvelope(${}, ${}, ${}, ${}, {in_srid}), 4326)",
                next,
                next + 1,
                next + 2,
                next + 3
            )
        });
        binds.extend(envelope.into_iter().map(Bind::Number));
        next += 4;
    }

    let (external, source) = store.features_source_at(layer.branch_id, "$1").await?;
    let pool = store.source_pool(external.as_ref()).await?;
    // the id is computed over the whole branch before any filter, so a filtered
    // query and an unfiltered one name the same feature by the same id
    let rows = format!(
        "WITH numbered AS (
             SELECT f.id, f.geometry, f.properties, {} AS oid FROM {source} f
         )",
        layer.oid.sql()
    );
    let predicate = if filters.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", filters.join(" AND "))
    };

    if count_only {
        let sql = format!("{rows} SELECT count(*)::bigint AS n FROM numbered{predicate}");
        let n: i64 = bound(&sql, layer.branch_id, &binds)
            .fetch_one(pool)
            .await?
            .get("n");
        return Ok(Json(json!({"count": n})));
    }

    if ids_only {
        // a feature with no value for a real objectid column has no id to name,
        // so it cannot appear in a list of ids
        let sql = format!(
            "{rows} SELECT oid FROM numbered{predicate}{} oid IS NOT NULL ORDER BY oid",
            if predicate.is_empty() {
                " WHERE"
            } else {
                " AND"
            }
        );
        let ids: Vec<i64> = bound(&sql, layer.branch_id, &binds)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|row| row.get::<i64, _>("oid"))
            .collect();
        return Ok(Json(json!({
            "objectIdFieldName": layer.oid.name(),
            "objectIds": ids,
        })));
    }

    let limit = params
        .get("resultRecordCount")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>().map_err(|_| {
                EsriError::bad_request(format!("resultRecordCount must be an integer: '{s}'"))
            })
        })
        .transpose()?
        .unwrap_or(MAX_RECORD_COUNT)
        .clamp(1, MAX_RECORD_COUNT);
    let offset = params
        .get("resultOffset")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>().map_err(|_| {
                EsriError::bad_request(format!("resultOffset must be an integer: '{s}'"))
            })
        })
        .transpose()?
        .unwrap_or(0)
        .max(0);

    // one row past the page, which is what says whether there is another page.
    // Cheaper than counting the whole filtered set on every page.
    let shape = if out_srid == 4326 {
        "ST_AsGeoJSON(geometry)::jsonb".to_string()
    } else {
        format!("ST_AsGeoJSON(ST_Transform(geometry, {out_srid}))::jsonb")
    };
    let sql = format!(
        "{rows} SELECT oid, {shape} AS geojson, properties
           FROM numbered{predicate}
          ORDER BY oid NULLS LAST, id
          LIMIT ${} OFFSET ${}",
        next,
        next + 1
    );
    let mut page = bound(&sql, layer.branch_id, &binds)
        .bind(limit + 1)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let exceeded = page.len() as i64 > limit;
    page.truncate(limit as usize);

    let features: Vec<Value> = page
        .iter()
        .map(|row| {
            let properties: Value = row.get("properties");
            let oid: Option<i64> = row.get("oid");
            let mut attributes = serde_json::Map::new();
            for field in &out_fields {
                let value = match field.kind {
                    Kind::Oid => oid.map(Value::from).unwrap_or(Value::Null),
                    _ => field.value(&properties),
                };
                attributes.insert(field.name.clone(), value);
            }
            let geometry: Option<Value> = row.get("geojson");
            match format {
                Format::Esri => {
                    let mut feature = json!({"attributes": attributes});
                    if return_geometry
                        && let Some(shape) = geometry.as_ref().and_then(esri_geometry)
                    {
                        feature["geometry"] = shape;
                    }
                    feature
                }
                Format::GeoJson => json!({
                    "type": "Feature",
                    "id": oid,
                    "properties": attributes,
                    "geometry": if return_geometry { geometry } else { None },
                }),
            }
        })
        .collect();

    Ok(Json(match format {
        Format::Esri => json!({
            "objectIdFieldName": layer.oid.name(),
            "globalIdFieldName": "",
            "geometryType": layer.geometry,
            "spatialReference": if out_srid == 4326 { spatial_reference() } else { mercator_reference() },
            "hasZ": false,
            "hasM": false,
            "fields": out_fields.iter().map(|f| f.declaration(layer.editable())).collect::<Vec<_>>(),
            "features": features,
            "exceededTransferLimit": exceeded,
        }),
        Format::GeoJson => json!({
            "type": "FeatureCollection",
            "features": features,
            "exceededTransferLimit": exceeded,
        }),
    }))
}

/// The fields a query answers with. The object id is always among them whatever
/// was asked for: a client pages and pairs rows by it, and Esri's own services
/// return it unasked for the same reason.
fn out_fields<'a>(layer: &'a Layer, asked: Option<&str>) -> Result<Vec<&'a Field>, EsriError> {
    let asked = asked.map(str::trim).unwrap_or_default();
    if asked.is_empty() || asked == "*" {
        return Ok(layer.fields.iter().collect());
    }
    let mut wanted: Vec<&Field> = vec![&layer.fields[0]];
    for name in asked.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let field = layer
            .field(name)
            .ok_or_else(|| EsriError::bad_request(format!("the layer has no field '{name}'")))?;
        if !wanted.iter().any(|held| held.name == field.name) {
            wanted.push(field);
        }
    }
    Ok(wanted)
}

/// `inSR` and `outSR` may be a wkid or the whole spatial reference object.
/// The store holds 4326 and PostGIS transforms to or from Web Mercator, which
/// every web client renders in; any other reference is refused rather than
/// guessed at.
fn sr_srid(params: &Params, name: &str) -> Result<Option<i32>, EsriError> {
    let Some(value) = params.get(name).map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    known_srid(value).map(Some).ok_or_else(|| {
        EsriError::bad_request(format!(
            "{name} '{value}' is not supported in this version of the service, \
             which serves and accepts EPSG:4326 and Web Mercator (3857/102100) only"
        ))
    })
}

/// The envelope a spatial filter names, as `[xmin, ymin, xmax, ymax]`.
///
/// An envelope intersection is the only spatial filter here, because it is the
/// only one the store's own bbox read implements. Any other geometry type or
/// relation is refused: running it as an envelope intersection would answer
/// with rows the client did not ask for.
fn envelope(params: &Params) -> Result<Option<([f64; 4], i32)>, EsriError> {
    let Some(raw) = params
        .get("geometry")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    match params
        .get("geometryType")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None | Some("esriGeometryEnvelope") => {}
        Some(other) => {
            return Err(EsriError::bad_request(format!(
                "geometryType {other} is not supported in this version of the service, \
                 which filters by esriGeometryEnvelope only"
            )));
        }
    }
    match params
        .get("spatialRel")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None | Some("esriSpatialRelIntersects") => {}
        Some(other) => {
            return Err(EsriError::bad_request(format!(
                "spatialRel {other} is not supported in this version of the service, \
                 which filters by esriSpatialRelIntersects only"
            )));
        }
    }
    let mut in_srid = sr_srid(params, "inSR")?.unwrap_or(4326);

    // both forms a client may send: the comma list, and the envelope object
    let bounds = if raw.starts_with('{') {
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| EsriError::bad_request(format!("geometry is not valid JSON: {e}")))?;
        // a reference on the envelope itself wins over inSR, as it does on
        // Esri's own services
        if let Some(reference) = value.get("spatialReference") {
            in_srid = known_srid(&reference.to_string()).ok_or_else(|| {
                EsriError::bad_request(format!(
                    "the envelope's spatialReference {reference} is not supported in this \
                     version of the service, which accepts EPSG:4326 and Web Mercator \
                     (3857/102100) only"
                ))
            })?;
        }
        let read = |key: &str| value.get(key).and_then(Value::as_f64);
        match (read("xmin"), read("ymin"), read("xmax"), read("ymax")) {
            (Some(a), Some(b), Some(c), Some(d)) => [a, b, c, d],
            _ => {
                return Err(EsriError::bad_request(
                    "an envelope needs numeric xmin, ymin, xmax and ymax",
                ));
            }
        }
    } else {
        let parts: Result<Vec<f64>, _> = raw.split(',').map(|s| s.trim().parse::<f64>()).collect();
        let parts = parts.map_err(|_| {
            EsriError::bad_request(format!(
                "geometry must be xmin,ymin,xmax,ymax or an envelope object: '{raw}'"
            ))
        })?;
        if parts.len() != 4 {
            return Err(EsriError::bad_request(format!(
                "an envelope is four numbers, xmin,ymin,xmax,ymax: '{raw}'"
            )));
        }
        [parts[0], parts[1], parts[2], parts[3]]
    };
    Ok(Some((bounds, in_srid)))
}

// ─── Geometry ───────────────────────────────────────────────────────

/// PostGIS GeoJSON as esriJSON.
///
/// The reference is stated once on the response rather than on every geometry,
/// which is what an Esri client expects and reads.
///
/// Z and M are dropped: the layer declares hasZ and hasM false, so a client is
/// told what it is getting rather than finding a third ordinate it was not
/// promised.
///
/// A GeometryCollection has no esriJSON form, so it becomes no geometry at all.
/// A dataset that could hold one has no Esri layer type either; this is here for
/// the single feature that disagrees with its dataset's declared type.
fn esri_geometry(geojson: &Value) -> Option<Value> {
    let kind = geojson.get("type")?.as_str()?;
    let coordinates = geojson.get("coordinates")?;
    match kind {
        "Point" => {
            let [x, y] = position(coordinates)?;
            Some(json!({"x": x, "y": y}))
        }
        "MultiPoint" => Some(json!({"points": positions(coordinates)?})),
        "LineString" => Some(json!({"paths": [positions(coordinates)?]})),
        "MultiLineString" => Some(json!({
            "paths": coordinates.as_array()?.iter().map(positions).collect::<Option<Vec<_>>>()?
        })),
        "Polygon" => Some(json!({"rings": rings(coordinates)?})),
        "MultiPolygon" => {
            // Esri has no multipolygon: one polygon carries every ring of every
            // part, and a client puts the parts back together by containment,
            // which is what the winding is for
            let mut all = Vec::new();
            for part in coordinates.as_array()? {
                all.extend(rings(part)?);
            }
            Some(json!({"rings": all}))
        }
        _ => None,
    }
}

fn position(value: &Value) -> Option<[f64; 2]> {
    let pair = value.as_array()?;
    Some([pair.first()?.as_f64()?, pair.get(1)?.as_f64()?])
}

fn positions(value: &Value) -> Option<Vec<[f64; 2]>> {
    value.as_array()?.iter().map(position).collect()
}

/// A polygon's rings, wound the way Esri reads them: the exterior clockwise and
/// every hole counter-clockwise, which is the reverse of what GeoJSON asks for.
///
/// The winding is computed rather than assumed. PostGIS does not promise the
/// ring order RFC 7946 asks for — `ST_AsGeoJSON` emits whatever orientation the
/// stored geometry has — so reversing every ring on the assumption that the
/// input was canonical would leave half of them wrong.
fn rings(value: &Value) -> Option<Vec<Vec<[f64; 2]>>> {
    let mut out = Vec::new();
    for (index, ring) in value.as_array()?.iter().enumerate() {
        let mut ring = positions(ring)?;
        let exterior = index == 0;
        // clockwise is a negative shoelace sum, so the exterior wants negative
        // and a hole wants positive
        if (shoelace(&ring) < 0.0) != exterior {
            ring.reverse();
        }
        out.push(ring);
    }
    Some(out)
}

/// Twice the signed area, positive counter-clockwise. Closed or not: the wrap
/// from the last vertex to the first is always added, and it contributes nothing
/// when the ring already closes.
fn shoelace(ring: &[[f64; 2]]) -> f64 {
    let mut sum = 0.0;
    for pair in ring.windows(2) {
        sum += pair[0][0] * pair[1][1] - pair[1][0] * pair[0][1];
    }
    if let (Some(last), Some(first)) = (ring.last(), ring.first()) {
        sum += last[0] * first[1] - first[0] * last[1];
    }
    sum
}

// ─── Edits ──────────────────────────────────────────────────────────

/// Parameters an edit request may carry that change what the edit means. Neither
/// can be honored, and a client that sent one believes something about the write
/// that is not true, so each is refused by name.
const UNSUPPORTED_EDITS: [&str; 2] = ["gdbVersion", "sessionId"];

/// The message a layer with no real object id column is refused with. Row
/// numbers shift when a feature is deleted, so an id a client wrote down does
/// not name the same feature afterwards and an edit aimed by one would land on
/// whatever moved into its place. See [`Oid::RowNumber`].
const NEEDS_A_REAL_OID: &str = "has no objectid column, so its object ids are row numbers over \
     feature order: a delete renumbers every feature after it, and an edit aimed by such an id \
     would land on a different feature. Declare an integer 'objectid' field on the dataset's \
     schema to make the layer editable.";

async fn apply_edits(
    State(store): State<AppState>,
    actor: Actor,
    Path((service, layer_id)): Path<(String, String)>,
    Form(params): Form<Vec<(String, String)>>,
) -> Result<Json<Value>, EsriError> {
    let params = Params(params);
    require_esri_json(&params)?;
    for name in UNSUPPORTED_EDITS {
        if params.asks_for(name) {
            return Err(EsriError::bad_request(format!(
                "{name} is not supported in this version of the service"
            )));
        }
    }
    // every edit in one request is one commit, so the whole batch already
    // succeeds or fails together and there is nothing to ask for here
    if params
        .get("rollbackOnFailure")
        .map(str::trim)
        .is_some_and(|v| !v.is_empty())
        && !params.flag("rollbackOnFailure")?
    {
        return Err(EsriError::bad_request(
            "rollbackOnFailure=false is not supported: every edit in one request is one commit, \
             so a failure refuses all of them",
        ));
    }
    if params.flag("useGlobalIds")? {
        return Err(EsriError::bad_request(
            "useGlobalIds is not supported: this layer has no global id field",
        ));
    }

    let layer = resolve(&store, &actor, &service, &layer_id).await?;
    let Oid::Property(oid_field) = &layer.oid else {
        return Err(EsriError::bad_request(format!(
            "'{}' {NEEDS_A_REAL_OID}",
            layer.name
        )));
    };

    // the same ladder every /api/v1 write runs. The write layer cannot run it
    // for this route: the target arrives as a service name, and the layer
    // resolves a target by reading a uuid out of the path. store.commit runs the
    // ladder again on the way in, so neither is the only guard.
    crate::visibility::ensure_writable(&store, &actor, layer.dataset_id)
        .await
        .map_err(EsriError::refused)?;

    let adds = features_param(&params, "adds")?;
    let updates = features_param(&params, "updates")?;
    let deletes = delete_ids(&params)?;
    if adds.is_empty() && updates.is_empty() && deletes.is_empty() {
        // an empty batch is answered rather than refused, but it is not
        // committed: a changeset with no operations is history that says nothing
        return Ok(Json(json!({
            "addResults": [],
            "updateResults": [],
            "deleteResults": [],
        })));
    }

    // an update names its feature by the object id in its own attributes
    let update_ids: Vec<i64> = updates
        .iter()
        .map(|feature| oid_of(feature, oid_field))
        .collect::<Result<_, _>>()?;
    let mut named: Vec<i64> = update_ids.clone();
    named.extend(deletes.iter().copied());
    let held = features_by_oid(&store, &layer, &named).await?;

    let mut ops: Vec<DiffOp> = Vec::new();
    let mut add_ids: Vec<i64> = Vec::new();

    if !adds.is_empty() {
        let mut next = max_oid(&store, &layer).await? + 1;
        for feature in &adds {
            let mut properties = attributes_of(feature)?;
            // the service assigns the id: a client-supplied one on an add would
            // either collide with a feature that exists or renumber the layer
            properties.retain(|key, _| !key.eq_ignore_ascii_case(oid_field));
            properties.insert(oid_field.clone(), json!(next));
            let shape = geometry_of(feature)
                .ok_or_else(|| EsriError::bad_request("an added feature needs a geometry"))?;
            ops.push(DiffOp::Insert {
                feature_id: Uuid::now_v7(),
                geometry_wkb: wkb_of(shape, layer.geometry)?,
                properties: Value::Object(properties),
                native: None,
                valid_from: None,
                valid_to: None,
            });
            add_ids.push(next);
            next += 1;
        }
    }

    for (feature, oid) in updates.iter().zip(&update_ids) {
        let (feature_id, stored) = held.get(oid).ok_or_else(|| unknown_oid(&layer, *oid))?;
        // an Esri update carries the attributes the client edited and no others,
        // while the store replaces the whole properties object, so the edit is
        // merged over what the feature holds now
        let mut properties = stored.as_object().cloned().unwrap_or_default();
        for (key, value) in attributes_of(feature)? {
            // the id stays as stored: it is the key a client holds the feature
            // by, and an update that changed it would rename the feature
            if key.eq_ignore_ascii_case(oid_field) {
                continue;
            }
            properties.insert(key, value);
        }
        // no geometry means the client edited attributes only, and `None` is how
        // the store is told to carry the previous version's geometry across
        let geometry_wkb = match geometry_of(feature) {
            Some(shape) => Some(wkb_of(shape, layer.geometry)?),
            None => None,
        };
        ops.push(DiffOp::Update {
            feature_id: *feature_id,
            geometry_wkb,
            properties: Some(Value::Object(properties)),
            native: None,
            valid_from: None,
            valid_to: None,
        });
    }

    for oid in &deletes {
        let (feature_id, _) = held.get(oid).ok_or_else(|| unknown_oid(&layer, *oid))?;
        ops.push(DiffOp::Delete {
            feature_id: *feature_id,
        });
    }

    let errors = store.validate_commit(layer.dataset_id, &ops).await?;
    if !errors.is_empty() {
        return Err(EsriError::bad_request(format!(
            "schema validation failed: {} error(s): {}",
            errors.len(),
            errors
                .iter()
                .take(5)
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }

    store
        .commit(
            layer.branch_id,
            "arcgis applyEdits",
            actor.or_body("arcgis"),
            &ops,
            &actor.writer(),
        )
        .await
        .map_err(EsriError::refused)?;

    Ok(Json(json!({
        "addResults": results(&add_ids),
        "updateResults": results(&update_ids),
        "deleteResults": results(&deletes),
    })))
}

/// One result per input edit, in input order.
///
/// Esri reports an edit per row, so a batch of three can come back with two
/// successes and one failure. Here it cannot: the batch is one commit, so any
/// failure refuses all of it and is answered as an `error` object naming the
/// cause instead of as a per-row outcome. A client that reads results rather
/// than the error sees every row succeed or no results at all, which is what the
/// store actually did.
fn results(ids: &[i64]) -> Vec<Value> {
    ids.iter()
        .map(|oid| json!({"objectId": oid, "success": true}))
        .collect()
}

fn unknown_oid(layer: &Layer, oid: i64) -> EsriError {
    EsriError::bad_request(format!(
        "'{}' has no feature with {} {oid}",
        layer.name,
        layer.oid.name()
    ))
}

/// The features an `adds` or `updates` parameter names. Absent or empty is no
/// features rather than an error: a request may carry any one of the three lists.
fn features_param(params: &Params, name: &str) -> Result<Vec<Value>, EsriError> {
    let Some(raw) = params.get(name).map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| EsriError::bad_request(format!("{name} is not valid JSON: {e}")))?;
    match value {
        Value::Array(features) => Ok(features),
        // a bare feature rather than a list of one, which clients do send
        Value::Object(_) => Ok(vec![value]),
        other => Err(EsriError::bad_request(format!(
            "{name} must be a JSON array of features, not {other}"
        ))),
    }
}

/// The object ids `deletes` names, as the comma list a client sends in a URL or
/// the JSON array it sends in a body.
fn delete_ids(params: &Params) -> Result<Vec<i64>, EsriError> {
    let Some(raw) = params
        .get("deletes")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(Vec::new());
    };
    let bad = || {
        EsriError::bad_request(format!(
            "deletes must be a comma-separated list of object ids or a JSON array of them: '{raw}'"
        ))
    };
    let parts: Vec<String> = if raw.starts_with('[') {
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| EsriError::bad_request(format!("deletes is not valid JSON: {e}")))?;
        match value {
            Value::Array(items) => items.iter().map(text_of).collect(),
            _ => return Err(bad()),
        }
    } else {
        raw.split(',').map(|s| s.trim().to_string()).collect()
    };
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<i64>().map_err(|_| bad()))
        .collect()
}

/// A feature's attributes as the properties they become. Absent is no
/// attributes: a dataset whose schema requires nothing takes a feature that
/// carries only a geometry.
fn attributes_of(feature: &Value) -> Result<serde_json::Map<String, Value>, EsriError> {
    match feature.get("attributes") {
        None | Some(Value::Null) => Ok(serde_json::Map::new()),
        Some(Value::Object(attributes)) => Ok(attributes.clone()),
        Some(other) => Err(EsriError::bad_request(format!(
            "a feature's attributes must be an object, not {other}"
        ))),
    }
}

fn geometry_of(feature: &Value) -> Option<&Value> {
    feature.get("geometry").filter(|shape| !shape.is_null())
}

/// The object id an update names, out of its own attributes. An update that
/// names no feature is refused rather than guessed at from its geometry.
fn oid_of(feature: &Value, field: &str) -> Result<i64, EsriError> {
    let attributes = attributes_of(feature)?;
    let held = attributes
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(field))
        .map(|(_, value)| value)
        .ok_or_else(|| {
            EsriError::bad_request(format!(
                "an updated feature needs its '{field}' in attributes, to say which feature it is"
            ))
        })?;
    match held {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
    .ok_or_else(|| {
        EsriError::bad_request(format!(
            "'{field}' must be an integer object id, not {held}"
        ))
    })
}

/// The highest object id on the branch, or 0 when nothing carries one, so the
/// next add is 1.
///
/// This is read before the commit rather than inside it, so two edit requests
/// racing each other can pick the same next id. A duplicate id can never make a
/// later edit hit the wrong feature: [`features_by_oid`] refuses an id that
/// names more than one feature. A layer whose ids must be unique under
/// concurrent writes needs a sequence, which the store does not have.
async fn max_oid(store: &AppState, layer: &Layer) -> Result<i64, EsriError> {
    let (external, source) = store.features_source_at(layer.branch_id, "$1").await?;
    let sql = format!(
        "WITH numbered AS (SELECT {} AS oid FROM {source} f)
         SELECT max(oid) AS top FROM numbered",
        layer.oid.sql()
    );
    let top: Option<i64> = sqlx::query(&sql)
        .bind(layer.branch_id)
        .fetch_one(store.source_pool(external.as_ref()).await?)
        .await?
        .get("top");
    Ok(top.unwrap_or(0))
}

/// Every feature the batch names, by object id: the feature id to edit and the
/// properties it holds now, which is what a partial update is merged over.
///
/// The id comes from the same numbered read the query answers with, so a client
/// edits the feature it saw.
async fn features_by_oid(
    store: &AppState,
    layer: &Layer,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, (Uuid, Value)>, EsriError> {
    let mut out = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let (external, source) = store.features_source_at(layer.branch_id, "$1").await?;
    let sql = format!(
        "WITH numbered AS (
             SELECT f.id, f.properties, {} AS oid FROM {source} f
         )
         SELECT id, oid, properties FROM numbered WHERE oid = ANY($2::bigint[])",
        layer.oid.sql()
    );
    let rows = sqlx::query(&sql)
        .bind(layer.branch_id)
        .bind(ids)
        .fetch_all(store.source_pool(external.as_ref()).await?)
        .await?;
    for row in rows {
        let oid: i64 = row.get("oid");
        let feature_id: Uuid = row.get("id");
        let properties: Value = row.get("properties");
        // nothing makes an objectid column unique, and an id that names two
        // features names neither: there is no feature for the edit to be aimed at
        if out.insert(oid, (feature_id, properties)).is_some() {
            return Err(EsriError::bad_request(format!(
                "{} {oid} names more than one feature in '{}', so it cannot name one to edit",
                layer.oid.name(),
                layer.name
            )));
        }
    }
    Ok(out)
}

// ─── Geometry input ─────────────────────────────────────────────────

/// esriJSON as the WKB the store is committed in.
fn wkb_of(shape: &Value, family: &str) -> Result<Vec<u8>, EsriError> {
    let geojson = geojson_of(shape, family)?;
    ptolemy_core::geoconvert::geojson_to_wkb(&geojson)
        .map_err(|e| EsriError::bad_request(format!("the geometry cannot be read: {e}")))
}

/// esriJSON as GeoJSON in EPSG:4326, the reference the store holds.
///
/// Which of the four shapes a value is, is which key it carries: esriJSON names
/// the geometry type on the layer and on a feature set, never on the geometry
/// itself. A shape from another family than the layer's is refused, because a
/// feature layer draws every feature as the one type it declares.
fn geojson_of(shape: &Value, family: &str) -> Result<Value, EsriError> {
    let object = shape
        .as_object()
        .ok_or_else(|| EsriError::bad_request(format!("a geometry is an object, not {shape}")))?;

    // absent means the reference the layer is in, which is what a client that
    // read the layer definition is sending
    let srid = match object.get("spatialReference") {
        None | Some(Value::Null) => 4326,
        Some(reference) => known_srid(&reference.to_string()).ok_or_else(|| {
            EsriError::bad_request(format!(
                "the geometry's spatialReference {reference} is not supported in this version of \
                 the service, which accepts EPSG:4326 and Web Mercator (3857/102100) only"
            ))
        })?,
    };

    if object.contains_key("x") || object.contains_key("y") {
        require_family(family, "esriGeometryPoint", "a point")?;
        let read = |key: &str| object.get(key).and_then(Value::as_f64);
        let (Some(x), Some(y)) = (read("x"), read("y")) else {
            return Err(EsriError::bad_request("a point needs numeric x and y"));
        };
        let [x, y] = to_4326([x, y], srid);
        return Ok(json!({"type": "Point", "coordinates": [x, y]}));
    }

    if let Some(points) = object.get("points") {
        require_family(family, "esriGeometryMultipoint", "a multipoint")?;
        let points = vertices(points, srid, "a multipoint's points")?;
        if points.is_empty() {
            return Err(EsriError::bad_request(
                "a multipoint needs at least one point",
            ));
        }
        return Ok(json!({"type": "MultiPoint", "coordinates": points}));
    }

    if let Some(paths) = object.get("paths") {
        require_family(family, "esriGeometryPolyline", "a polyline")?;
        let mut parts = Vec::new();
        for path in as_parts(paths, "a polyline's paths")? {
            let path = vertices(path, srid, "a path")?;
            if path.len() < 2 {
                return Err(EsriError::bad_request("a path needs at least two vertices"));
            }
            parts.push(path);
        }
        return match parts.len() {
            0 => Err(EsriError::bad_request("a polyline needs at least one path")),
            1 => Ok(json!({"type": "LineString", "coordinates": parts.swap_remove(0)})),
            _ => Ok(json!({"type": "MultiLineString", "coordinates": parts})),
        };
    }

    if let Some(rings) = object.get("rings") {
        require_family(family, "esriGeometryPolygon", "a polygon")?;
        let mut all = Vec::new();
        for ring in as_parts(rings, "a polygon's rings")? {
            let ring = vertices(ring, srid, "a ring")?;
            if ring.len() < 3 {
                return Err(EsriError::bad_request(
                    "a ring needs at least three vertices",
                ));
            }
            all.push(ring);
        }
        if all.is_empty() {
            return Err(EsriError::bad_request("a polygon needs at least one ring"));
        }
        let mut parts: Vec<Vec<Vec<[f64; 2]>>> = assemble(all)
            .into_iter()
            .map(|polygon| {
                polygon
                    .into_iter()
                    .enumerate()
                    .map(|(index, ring)| geojson_ring(ring, index == 0))
                    .collect()
            })
            .collect();
        return if parts.len() == 1 {
            Ok(json!({"type": "Polygon", "coordinates": parts.swap_remove(0)}))
        } else {
            Ok(json!({"type": "MultiPolygon", "coordinates": parts}))
        };
    }

    Err(EsriError::bad_request(
        "a geometry is {x,y}, {points}, {paths} or {rings}",
    ))
}

fn require_family(family: &str, wanted: &str, sent: &str) -> Result<(), EsriError> {
    if family == wanted {
        return Ok(());
    }
    Err(EsriError::bad_request(format!(
        "the layer is {family} and the geometry sent is {sent}: a feature layer holds one \
         geometry type, so the feature cannot go in it"
    )))
}

fn as_parts<'a>(value: &'a Value, what: &str) -> Result<&'a Vec<Value>, EsriError> {
    value
        .as_array()
        .ok_or_else(|| EsriError::bad_request(format!("{what} must be an array")))
}

/// A vertex list in the reference it was sent in, as 4326 positions. A third or
/// fourth ordinate is dropped: the layer declares hasZ and hasM false, so a
/// client is told what the service keeps.
fn vertices(value: &Value, srid: i32, what: &str) -> Result<Vec<[f64; 2]>, EsriError> {
    as_parts(value, what)?
        .iter()
        .map(|vertex| {
            let pair = vertex.as_array().filter(|pair| pair.len() >= 2);
            let read = |at: usize| pair.and_then(|pair| pair.get(at)).and_then(Value::as_f64);
            match (read(0), read(1)) {
                (Some(x), Some(y)) => Ok(to_4326([x, y], srid)),
                _ => Err(EsriError::bad_request(format!(
                    "{what} holds {vertex}, and a vertex is at least two numbers"
                ))),
            }
        })
        .collect()
}

/// Web Mercator metres as degrees, closed form on the sphere the projection is
/// defined on, so accepting what a web client sends needs no PROJ here. Anything
/// but mercator is already 4326: [`known_srid`] admits nothing else.
fn to_4326([x, y]: [f64; 2], srid: i32) -> [f64; 2] {
    if srid == 4326 {
        return [x, y];
    }
    /// The sphere Web Mercator is defined on.
    const RADIUS: f64 = 6378137.0;
    let degrees = 180.0 / std::f64::consts::PI;
    [x / RADIUS * degrees, (y / RADIUS).sinh().atan() * degrees]
}

/// The flat esriJSON ring list as polygons: each exterior ring with the holes
/// that fall inside it.
///
/// esriJSON winds an exterior ring clockwise and a hole counter-clockwise and
/// says nothing about which exterior a hole belongs to, so a hole goes in the
/// exterior ring that contains its first vertex. A hole no exterior contains is
/// kept as an exterior rather than dropped: a wrong winding is more often sloppy
/// data than a hole. Ported from verne's reader, which solved this first.
fn assemble(rings: Vec<Vec<[f64; 2]>>) -> Vec<Vec<Vec<[f64; 2]>>> {
    let mut exteriors: Vec<Vec<Vec<[f64; 2]>>> = Vec::new();
    let mut holes: Vec<Vec<[f64; 2]>> = Vec::new();
    for ring in rings {
        // clockwise is a negative shoelace sum, and clockwise is Esri's exterior
        if shoelace(&ring) <= 0.0 {
            exteriors.push(vec![ring]);
        } else {
            holes.push(ring);
        }
    }
    // every ring wound like a hole, so there is no exterior to put them in and
    // they are taken as they are
    if exteriors.is_empty() {
        return holes.into_iter().map(|ring| vec![ring]).collect();
    }
    for hole in holes {
        let Some(first) = hole.first().copied() else {
            continue;
        };
        match exteriors
            .iter_mut()
            .find(|polygon| contains(&polygon[0], first))
        {
            Some(polygon) => polygon.push(hole),
            None => exteriors.push(vec![hole]),
        }
    }
    exteriors
}

/// Ray cast along x: an odd number of crossings means inside.
fn contains(ring: &[[f64; 2]], point: [f64; 2]) -> bool {
    let Some(mut previous) = ring.last() else {
        return false;
    };
    let mut inside = false;
    for held in ring {
        if (held[1] > point[1]) != (previous[1] > point[1]) {
            let cross =
                (previous[0] - held[0]) * (point[1] - held[1]) / (previous[1] - held[1]) + held[0];
            if point[0] < cross {
                inside = !inside;
            }
        }
        previous = held;
    }
    inside
}

/// A ring the way GeoJSON asks for it: closed, exterior counter-clockwise and a
/// hole clockwise. The read side computes the winding it emits rather than
/// trusting what is stored, so this is hygiene rather than something a client
/// depends on.
fn geojson_ring(mut ring: Vec<[f64; 2]>, exterior: bool) -> Vec<[f64; 2]> {
    if (shoelace(&ring) > 0.0) != exterior {
        ring.reverse();
    }
    if let (Some(first), Some(last)) = (ring.first().copied(), ring.last().copied())
        && first != last
    {
        ring.push(first);
    }
    ring
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_becomes_x_and_y() {
        let shape = esri_geometry(&json!({"type": "Point", "coordinates": [1.5, -2.5]})).unwrap();
        assert_eq!(shape, json!({"x": 1.5, "y": -2.5}));
    }

    #[test]
    fn linestring_becomes_one_path() {
        let shape =
            esri_geometry(&json!({"type": "LineString", "coordinates": [[0, 0], [1, 1]]})).unwrap();
        assert_eq!(shape, json!({"paths": [[[0.0, 0.0], [1.0, 1.0]]]}));
    }

    #[test]
    fn multipoint_becomes_points() {
        let shape =
            esri_geometry(&json!({"type": "MultiPoint", "coordinates": [[0, 1], [2, 3]]})).unwrap();
        assert_eq!(shape, json!({"points": [[0.0, 1.0], [2.0, 3.0]]}));
    }

    /// A GeoJSON exterior ring is counter-clockwise and an Esri one is
    /// clockwise, so the vertex order comes out reversed.
    #[test]
    fn exterior_ring_is_wound_clockwise() {
        let counter_clockwise =
            json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]]});
        let shape = esri_geometry(&counter_clockwise).unwrap();
        assert_eq!(
            shape,
            json!({"rings": [[[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]]})
        );
        let rings = shape["rings"].as_array().unwrap();
        assert!(shoelace(&positions(&rings[0]).unwrap()) < 0.0);
    }

    /// A ring that already reads clockwise is left where it is: the winding is
    /// computed, not assumed from the format.
    #[test]
    fn clockwise_exterior_ring_is_left_alone() {
        let clockwise =
            json!({"type": "Polygon", "coordinates": [[[0, 0], [0, 1], [1, 1], [1, 0], [0, 0]]]});
        let shape = esri_geometry(&clockwise).unwrap();
        assert_eq!(
            shape,
            json!({"rings": [[[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]]})
        );
    }

    #[test]
    fn hole_is_wound_counter_clockwise() {
        let with_hole = json!({"type": "Polygon", "coordinates": [
            [[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]],
            [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]]
        ]});
        let shape = esri_geometry(&with_hole).unwrap();
        let rings = shape["rings"].as_array().unwrap();
        assert_eq!(rings.len(), 2);
        assert!(shoelace(&positions(&rings[0]).unwrap()) < 0.0, "exterior");
        assert!(shoelace(&positions(&rings[1]).unwrap()) > 0.0, "hole");
    }

    #[test]
    fn multipolygon_flattens_to_one_ring_list() {
        let two_parts = json!({"type": "MultiPolygon", "coordinates": [
            [[[0, 0], [1, 0], [1, 1], [0, 0]]],
            [[[5, 5], [6, 5], [6, 6], [5, 5]]]
        ]});
        let shape = esri_geometry(&two_parts).unwrap();
        assert_eq!(shape["rings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn geometry_collection_has_no_esri_form() {
        assert!(esri_geometry(&json!({"type": "GeometryCollection", "geometries": []})).is_none());
    }

    #[test]
    fn mixed_geometry_datasets_have_no_layer_type() {
        assert_eq!(esri_geometry_type("point"), Some("esriGeometryPoint"));
        assert_eq!(
            esri_geometry_type("multilinestring"),
            Some("esriGeometryPolyline")
        );
        assert_eq!(
            esri_geometry_type("multipolygon"),
            Some("esriGeometryPolygon")
        );
        assert_eq!(esri_geometry_type("geometry"), None);
        assert_eq!(esri_geometry_type("geometrycollection"), None);
    }

    #[test]
    fn flags_take_only_the_documented_spellings() {
        let params = Params(vec![
            ("a".into(), "true".into()),
            ("b".into(), "FALSE".into()),
            ("c".into(), "yes".into()),
        ]);
        assert!(params.flag("a").unwrap());
        assert!(!params.flag("b").unwrap());
        assert!(!params.flag("missing").unwrap());
        assert!(params.flag("c").is_err());
    }

    #[test]
    fn sr_accepts_4326_and_mercator_as_a_code_or_an_object() {
        let object = Params(vec![("outSR".into(), r#"{"wkid":4326}"#.into())]);
        assert_eq!(sr_srid(&object, "outSR").unwrap(), Some(4326));
        let code = Params(vec![("outSR".into(), "4326".into())]);
        assert_eq!(sr_srid(&code, "outSR").unwrap(), Some(4326));
        // both names a web client uses for mercator normalise to postgis's
        let epsg = Params(vec![("outSR".into(), "3857".into())]);
        assert_eq!(sr_srid(&epsg, "outSR").unwrap(), Some(3857));
        let esri = Params(vec![("outSR".into(), "102100".into())]);
        assert_eq!(sr_srid(&esri, "outSR").unwrap(), Some(3857));
        let other = Params(vec![("outSR".into(), "27700".into())]);
        assert!(sr_srid(&other, "outSR").is_err());
    }

    #[test]
    fn envelope_reads_both_forms_and_refuses_other_relations() {
        let list = Params(vec![("geometry".into(), "1,2,3,4".into())]);
        assert_eq!(envelope(&list).unwrap(), Some(([1.0, 2.0, 3.0, 4.0], 4326)));

        let object = Params(vec![(
            "geometry".into(),
            r#"{"xmin":1,"ymin":2,"xmax":3,"ymax":4,"spatialReference":{"wkid":102100}}"#.into(),
        )]);
        assert_eq!(
            envelope(&object).unwrap(),
            Some(([1.0, 2.0, 3.0, 4.0], 3857))
        );

        let polygon = Params(vec![
            ("geometry".into(), "1,2,3,4".into()),
            ("geometryType".into(), "esriGeometryPolygon".into()),
        ]);
        assert!(envelope(&polygon).is_err());

        let contains = Params(vec![
            ("geometry".into(), "1,2,3,4".into()),
            ("spatialRel".into(), "esriSpatialRelContains".into()),
        ]);
        assert!(envelope(&contains).is_err());
    }

    // ─── Geometry input ─────────────────────────────────────────────

    /// The forward spherical mercator, so a test can project a coordinate the
    /// way a web client does and ask for it back.
    fn to_mercator(lon: f64, lat: f64) -> (f64, f64) {
        const RADIUS: f64 = 6378137.0;
        let radians = std::f64::consts::PI / 180.0;
        (
            lon * radians * RADIUS,
            ((lat * radians / 2.0 + std::f64::consts::FRAC_PI_4).tan()).ln() * RADIUS,
        )
    }

    #[test]
    fn a_point_becomes_geojson_in_4326() {
        let shape = json!({"x": 1.5, "y": -2.5});
        assert_eq!(
            geojson_of(&shape, "esriGeometryPoint").unwrap(),
            json!({"type": "Point", "coordinates": [1.5, -2.5]})
        );
        // a point with no numbers is a client bug, not an empty geometry
        assert!(geojson_of(&json!({"x": "NaN", "y": "NaN"}), "esriGeometryPoint").is_err());
    }

    /// Mercator in, degrees out: the closed form, so the crate needs no PROJ to
    /// take what a web client sends.
    #[test]
    fn a_mercator_point_comes_back_as_the_degrees_it_was_projected_from() {
        let (x, y) = to_mercator(-71.06, 42.36);
        let shape = json!({"x": x, "y": y, "spatialReference": {"wkid": 102100}});
        let geojson = geojson_of(&shape, "esriGeometryPoint").unwrap();
        let held = geojson["coordinates"].as_array().unwrap();
        assert!(
            (held[0].as_f64().unwrap() - -71.06).abs() < 1e-9,
            "{geojson}"
        );
        assert!(
            (held[1].as_f64().unwrap() - 42.36).abs() < 1e-9,
            "{geojson}"
        );
    }

    #[test]
    fn a_reference_this_service_cannot_speak_is_refused() {
        let shape = json!({"x": 1.0, "y": 2.0, "spatialReference": {"wkid": 27700}});
        assert!(geojson_of(&shape, "esriGeometryPoint").is_err());
    }

    #[test]
    fn one_path_is_a_linestring_and_two_are_a_multilinestring() {
        let one = json!({"paths": [[[0, 0], [1, 1]]]});
        assert_eq!(
            geojson_of(&one, "esriGeometryPolyline").unwrap(),
            json!({"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]})
        );
        let two = json!({"paths": [[[0, 0], [1, 1]], [[5, 5], [6, 6]]]});
        let shape = geojson_of(&two, "esriGeometryPolyline").unwrap();
        assert_eq!(shape["type"], "MultiLineString", "{shape}");
        assert_eq!(shape["coordinates"].as_array().unwrap().len(), 2, "{shape}");
        // a path of one vertex is not a line
        assert!(geojson_of(&json!({"paths": [[[0, 0]]]}), "esriGeometryPolyline").is_err());
    }

    #[test]
    fn points_become_a_multipoint() {
        let shape = json!({"points": [[0, 1], [2, 3]]});
        assert_eq!(
            geojson_of(&shape, "esriGeometryMultipoint").unwrap(),
            json!({"type": "MultiPoint", "coordinates": [[0.0, 1.0], [2.0, 3.0]]})
        );
    }

    /// esriJSON winds an exterior clockwise, but real data arrives both ways
    /// round, so the winding is classified rather than assumed and either input
    /// gives the same polygon.
    #[test]
    fn a_polygon_is_read_in_either_winding() {
        let clockwise = json!({"rings": [[[0, 0], [0, 4], [4, 4], [4, 0], [0, 0]]]});
        let counter_clockwise = json!({"rings": [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]]]});
        for shape in [clockwise, counter_clockwise] {
            let geojson = geojson_of(&shape, "esriGeometryPolygon").unwrap();
            assert_eq!(geojson["type"], "Polygon", "{geojson}");
            let rings = geojson["coordinates"].as_array().unwrap();
            assert_eq!(rings.len(), 1, "{geojson}");
            // GeoJSON winds an exterior counter-clockwise
            assert!(shoelace(&positions(&rings[0]).unwrap()) > 0.0, "{geojson}");
        }
    }

    /// One flat ring list, and the parts come back out of it: a hole lands in
    /// the exterior that contains it, and a second exterior is a second part.
    #[test]
    fn rings_are_assembled_into_parts_by_containment() {
        let with_hole = json!({"rings": [
            [[0, 0], [0, 10], [10, 10], [10, 0], [0, 0]],
            [[2, 2], [8, 2], [8, 8], [2, 8], [2, 2]]
        ]});
        let geojson = geojson_of(&with_hole, "esriGeometryPolygon").unwrap();
        assert_eq!(geojson["type"], "Polygon", "{geojson}");
        let rings = geojson["coordinates"].as_array().unwrap();
        assert_eq!(rings.len(), 2, "{geojson}");
        assert!(shoelace(&positions(&rings[0]).unwrap()) > 0.0, "exterior");
        assert!(shoelace(&positions(&rings[1]).unwrap()) < 0.0, "hole");

        let two_parts = json!({"rings": [
            [[0, 0], [0, 1], [1, 1], [1, 0], [0, 0]],
            [[5, 5], [5, 6], [6, 6], [6, 5], [5, 5]]
        ]});
        let geojson = geojson_of(&two_parts, "esriGeometryPolygon").unwrap();
        assert_eq!(geojson["type"], "MultiPolygon", "{geojson}");
        assert_eq!(
            geojson["coordinates"].as_array().unwrap().len(),
            2,
            "{geojson}"
        );
    }

    /// An unclosed ring is closed on the way in, because a polygon ring has to
    /// end where it started.
    #[test]
    fn an_unclosed_ring_is_closed() {
        let open = json!({"rings": [[[0, 0], [0, 4], [4, 4], [4, 0]]]});
        let geojson = geojson_of(&open, "esriGeometryPolygon").unwrap();
        let ring = geojson["coordinates"][0].as_array().unwrap();
        assert_eq!(ring.len(), 5, "{geojson}");
        assert_eq!(ring[0], ring[4], "{geojson}");
    }

    /// A layer declares one geometry type and draws every feature with it, so a
    /// shape from another family cannot go in.
    #[test]
    fn a_geometry_of_the_wrong_family_is_refused() {
        assert!(geojson_of(&json!({"x": 1, "y": 2}), "esriGeometryPolygon").is_err());
        assert!(
            geojson_of(
                &json!({"rings": [[[0, 0], [1, 0], [1, 1], [0, 0]]]}),
                "esriGeometryPoint"
            )
            .is_err()
        );
        assert!(geojson_of(&json!({"paths": [[[0, 0], [1, 1]]]}), "esriGeometryPolygon").is_err());
        // no key names a shape at all
        assert!(geojson_of(&json!({"curveRings": []}), "esriGeometryPolygon").is_err());
    }

    #[test]
    fn a_geometry_becomes_wkb_the_store_can_take() {
        let wkb = wkb_of(&json!({"x": 1.0, "y": 2.0}), "esriGeometryPoint").unwrap();
        // little endian, type 1 (point), then two doubles
        assert_eq!(wkb.len(), 1 + 4 + 16, "{wkb:?}");
        assert_eq!(wkb[0], 1);
        assert_eq!(wkb[1], 1);
    }

    // ─── Edit parameters ────────────────────────────────────────────

    #[test]
    fn adds_and_updates_read_a_list_or_a_bare_feature() {
        let list = Params(vec![(
            "adds".into(),
            r#"[{"attributes":{"name":"a"}},{"attributes":{"name":"b"}}]"#.into(),
        )]);
        assert_eq!(features_param(&list, "adds").unwrap().len(), 2);

        let bare = Params(vec![(
            "adds".into(),
            r#"{"attributes":{"name":"a"}}"#.into(),
        )]);
        assert_eq!(features_param(&bare, "adds").unwrap().len(), 1);

        // absent and empty are no features, not an error: a request may carry
        // any one of the three lists
        assert!(features_param(&Params(vec![]), "adds").unwrap().is_empty());
        let empty = Params(vec![("adds".into(), "  ".into())]);
        assert!(features_param(&empty, "adds").unwrap().is_empty());

        let broken = Params(vec![("adds".into(), "[{".into())]);
        assert!(features_param(&broken, "adds").is_err());
        let wrong = Params(vec![("adds".into(), "7".into())]);
        assert!(features_param(&wrong, "adds").is_err());
    }

    #[test]
    fn deletes_read_a_comma_list_or_a_json_array() {
        let list = Params(vec![("deletes".into(), "1, 2,3".into())]);
        assert_eq!(delete_ids(&list).unwrap(), vec![1, 2, 3]);
        let array = Params(vec![("deletes".into(), "[4,5]".into())]);
        assert_eq!(delete_ids(&array).unwrap(), vec![4, 5]);
        assert!(delete_ids(&Params(vec![])).unwrap().is_empty());
        let words = Params(vec![("deletes".into(), "two".into())]);
        assert!(delete_ids(&words).is_err());
    }

    #[test]
    fn an_update_names_its_feature_by_the_object_id_in_its_attributes() {
        let numeric = json!({"attributes": {"OBJECTID": 12, "name": "a"}});
        assert_eq!(oid_of(&numeric, "OBJECTID").unwrap(), 12);
        // the field is matched without regard to case, as every parameter is
        let cased = json!({"attributes": {"objectid": 12}});
        assert_eq!(oid_of(&cased, "OBJECTID").unwrap(), 12);
        // a form-encoded client may send it as text
        let text = json!({"attributes": {"OBJECTID": "12"}});
        assert_eq!(oid_of(&text, "OBJECTID").unwrap(), 12);

        assert!(oid_of(&json!({"attributes": {"name": "a"}}), "OBJECTID").is_err());
        assert!(oid_of(&json!({"geometry": {"x": 1, "y": 2}}), "OBJECTID").is_err());
        assert!(oid_of(&json!({"attributes": {"OBJECTID": "x"}}), "OBJECTID").is_err());
    }

    #[test]
    fn attributes_are_optional_and_have_to_be_an_object() {
        assert!(attributes_of(&json!({})).unwrap().is_empty());
        assert!(
            attributes_of(&json!({"attributes": Value::Null}))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            attributes_of(&json!({"attributes": {"a": 1}}))
                .unwrap()
                .len(),
            1
        );
        assert!(attributes_of(&json!({"attributes": [1, 2]})).is_err());
    }

    /// The id field is not editable even on an editable layer: it is the key a
    /// client holds the feature by.
    #[test]
    fn only_a_non_id_field_of_an_editable_layer_is_declared_editable() {
        let oid = Field {
            name: "objectid".into(),
            alias: "objectid".into(),
            kind: Kind::Oid,
        };
        let name = Field {
            name: "name".into(),
            alias: "name".into(),
            kind: Kind::Text,
        };
        assert_eq!(oid.declaration(true)["editable"], false);
        assert_eq!(name.declaration(true)["editable"], true);
        assert_eq!(name.declaration(false)["editable"], false);
    }

    #[test]
    fn boolean_and_json_values_come_out_as_their_declared_string() {
        let boolean = Field {
            name: "flag".into(),
            alias: "flag".into(),
            kind: Kind::BooleanText,
        };
        assert_eq!(
            boolean.value(&json!({"flag": true})),
            Value::from("true".to_string())
        );
        let nested = Field {
            name: "tags".into(),
            alias: "tags".into(),
            kind: Kind::JsonText,
        };
        assert_eq!(
            nested.value(&json!({"tags": ["a", "b"]})),
            Value::from(r#"["a","b"]"#.to_string())
        );
        assert_eq!(nested.value(&json!({})), Value::Null);
    }
}
