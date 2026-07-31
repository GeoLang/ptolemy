// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! A read-only ArcGIS FeatureServer (Geoservices REST) frontend, so an Esri
//! client — an ArcGIS JS API web map, QGIS's ArcGIS REST connector, verne's
//! extractor — can point at ptolemy without knowing what it is talking to.
//!
//! One dataset is one single-layer service and the layer is always id 0. That
//! is the shape a hosted feature layer has, and it leaves no layer id that
//! could move when datasets come and go.
//!
//! Reads always come from the dataset's `main` branch: nothing in the protocol
//! can name a branch, so there is one answer and it is the obvious one.
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

use axum::{
    Json, Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use ptolemy_core::schema::{FieldDef, FieldType};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

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
        (
            StatusCode::OK,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                    "details": [],
                }
            })),
        )
            .into_response()
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
    fn declaration(&self) -> Value {
        let mut out = json!({
            "name": self.name,
            "type": self.kind.esri(),
            "alias": self.alias,
            "nullable": self.kind != Kind::Oid,
            "editable": false,
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
        "allowGeometryUpdates": false,
        "units": "esriDecimalDegrees",
        "maxRecordCount": MAX_RECORD_COUNT,
        "supportedQueryFormats": "JSON,geoJSON",
        "capabilities": "Query",
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
        "fields": layer.fields.iter().map(Field::declaration).collect::<Vec<_>>(),
        "extent": extent,
        "maxRecordCount": MAX_RECORD_COUNT,
        "standardMaxRecordCount": MAX_RECORD_COUNT,
        "supportedQueryFormats": "JSON,geoJSON",
        "capabilities": "Query",
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
        "allowGeometryUpdates": false,
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

/// A filter's bind value. Only these two shapes occur, so a filter is SQL plus a
/// bind rather than interpolated request data.
enum Bind {
    Ids(Vec<i64>),
    Coord(f64),
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
            Bind::Coord(v) => query.bind(*v),
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

    match params.get("where").map(str::trim) {
        None | Some("") | Some("1=1") | Some("1 = 1") => {}
        Some(clause) => {
            return Err(EsriError::bad_request(format!(
                "where is not supported in this version of the service, except as 1=1: '{clause}'"
            )));
        }
    }

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
        binds.extend(envelope.into_iter().map(Bind::Coord));
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
            "fields": out_fields.iter().map(|f| f.declaration()).collect::<Vec<_>>(),
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
