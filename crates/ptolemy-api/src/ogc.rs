// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! OGC API - Features compliant endpoints and audit log.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use ptolemy_storage::AuditEntry;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn ogc_routes() -> Router<AppState> {
    Router::new()
        // OGC API - Features landing
        .route("/ogc", get(landing))
        .route("/ogc/conformance", get(conformance))
        .route("/ogc/collections", get(collections))
        .route("/ogc/collections/{id}", get(collection_info))
        .route("/ogc/collections/{id}/items", get(items))
        .route("/ogc/collections/{id}/items/{fid}", get(item))
        // Audit log
        .route("/audit", get(list_audit))
}

// ─── OGC API - Features ─────────────────────────────────────────────

#[derive(Serialize)]
struct LandingPage {
    title: String,
    description: String,
    links: Vec<Link>,
}

#[derive(Serialize)]
struct Link {
    href: String,
    rel: String,
    #[serde(rename = "type")]
    media_type: String,
    title: String,
}

async fn landing() -> Json<LandingPage> {
    Json(LandingPage {
        title: "Ptolemy OGC API".into(),
        description: "OGC API - Features compliant interface to Ptolemy versioned GIS database"
            .into(),
        links: vec![
            Link {
                href: "/api/v1/ogc".into(),
                rel: "self".into(),
                media_type: "application/json".into(),
                title: "This document".into(),
            },
            Link {
                href: "/api/v1/ogc/conformance".into(),
                rel: "conformance".into(),
                media_type: "application/json".into(),
                title: "Conformance classes".into(),
            },
            Link {
                href: "/api/v1/ogc/collections".into(),
                rel: "data".into(),
                media_type: "application/json".into(),
                title: "Collections".into(),
            },
        ],
    })
}

#[derive(Serialize)]
struct Conformance {
    #[serde(rename = "conformsTo")]
    conforms_to: Vec<String>,
}

async fn conformance() -> Json<Conformance> {
    Json(Conformance {
        conforms_to: vec![
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core".into(),
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson".into(),
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30".into(),
            CRS_CONFORMANCE_CLASS.into(),
        ],
    })
}

// ─── CRS by reference ───────────────────────────────────────────────

const CRS_CONFORMANCE_CLASS: &str = "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs";
const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const EPSG_URI_PREFIX: &str = "http://www.opengis.net/def/crs/EPSG/0/";

/// Offered for every collection, whatever the dataset was declared in.
const ALWAYS_SUPPORTED_SRIDS: [i32; 2] = [4326, 3857];

const STORAGE_SRID: i32 = 4326;

fn epsg_uri(srid: i32) -> String {
    format!("{EPSG_URI_PREFIX}{srid}")
}

fn supported_crs_uris(dataset_srid: i32) -> Vec<String> {
    let mut uris = vec![CRS84_URI.to_string()];
    uris.extend(ALWAYS_SUPPORTED_SRIDS.iter().map(|srid| epsg_uri(*srid)));
    if !ALWAYS_SUPPORTED_SRIDS.contains(&dataset_srid) {
        uris.push(epsg_uri(dataset_srid));
    }
    uris
}

/// A CRS a request asked for, resolved against `spatial_ref_sys`.
struct RequestedCrs {
    uri: String,
    srid: i32,
    /// A geographic EPSG code is served latitude first, so its coordinates are
    /// the reverse of the x, y order PostGIS works in. CRS84 is longitude first
    /// and never swaps.
    swap_axes: bool,
}

fn default_crs() -> RequestedCrs {
    RequestedCrs {
        uri: CRS84_URI.to_string(),
        srid: STORAGE_SRID,
        swap_axes: false,
    }
}

/// The one place the CRS URI grammar lives. `parameter` names the query
/// parameter the URI came from, so a rejection says which one was wrong.
async fn resolve_crs(
    store: &AppState,
    dataset_srid: i32,
    parameter: &str,
    uri: Option<&str>,
) -> Result<RequestedCrs, OgcError> {
    let Some(uri) = uri else {
        return Ok(default_crs());
    };
    if uri == CRS84_URI {
        return Ok(default_crs());
    }
    let unsupported = || OgcError::BadRequest(format!("unsupported {parameter} value: {uri}"));
    if !supported_crs_uris(dataset_srid).iter().any(|u| u == uri) {
        return Err(unsupported());
    }
    let code: i32 = uri
        .strip_prefix(EPSG_URI_PREFIX)
        .and_then(|c| c.parse().ok())
        .ok_or_else(unsupported)?;
    let row = sqlx::query("SELECT srtext FROM spatial_ref_sys WHERE srid = $1")
        .bind(code)
        .fetch_optional(store.read_pool())
        .await?
        .ok_or_else(unsupported)?;
    let srtext: String = row.get("srtext");
    Ok(RequestedCrs {
        uri: uri.to_string(),
        srid: code,
        swap_axes: srtext.trim_start().starts_with("GEOGCS"),
    })
}

/// SQL for a stored 4326 geometry as the request asked to see it.
fn geometry_expression(column: &str, crs: &RequestedCrs) -> String {
    let transformed = if crs.srid == STORAGE_SRID {
        column.to_string()
    } else {
        format!("ST_Transform({column}, {})", crs.srid)
    };
    if crs.swap_axes {
        return format!("ST_SwapOrdinates({transformed}, 'xy')");
    }
    transformed
}

fn content_crs_header(crs: &RequestedCrs) -> [(&'static str, String); 1] {
    [("content-crs", format!("<{}>", crs.uri))]
}

/// The dataset's declared srid, read only when a request names a CRS: all it
/// can do is widen the supported list.
async fn dataset_srid(store: &AppState, dataset_id: Uuid) -> Result<i32, OgcError> {
    let row = sqlx::query("SELECT srid FROM datasets WHERE id = $1")
        .bind(dataset_id)
        .fetch_optional(store.read_pool())
        .await?
        .ok_or_else(|| OgcError::NotFound("collection not found".into()))?;
    Ok(row.get("srid"))
}

#[derive(Serialize)]
struct Collection {
    id: String,
    title: String,
    description: String,
    extent: Option<serde_json::Value>,
    crs: Vec<String>,
    #[serde(rename = "storageCrs")]
    storage_crs: String,
    links: Vec<Link>,
}

#[derive(Serialize)]
struct Collections {
    collections: Vec<Collection>,
}

async fn collections(
    State(store): State<AppState>,
    actor: Actor,
) -> Result<Json<Collections>, OgcError> {
    let reader = actor.reader();
    let visible = ptolemy_storage::visible_datasets_sql("d", 1, 2);
    let rows = sqlx::query(&format!(
        "SELECT d.id, d.name, d.srid FROM datasets d WHERE {visible} ORDER BY d.name"
    ))
    .bind(reader.bypass)
    .bind(reader.id.as_deref())
    .fetch_all(store.read_pool())
    .await?;

    let cols: Vec<Collection> = rows
        .into_iter()
        .map(|row| {
            let id: Uuid = row.get("id");
            let name: String = row.get("name");
            let srid: i32 = row.get("srid");
            Collection {
                id: id.to_string(),
                title: name.clone(),
                description: format!("Dataset: {name}"),
                extent: None,
                crs: supported_crs_uris(srid),
                storage_crs: CRS84_URI.to_string(),
                links: vec![Link {
                    href: format!("/api/v1/ogc/collections/{id}/items"),
                    rel: "items".into(),
                    media_type: "application/geo+json".into(),
                    title: "Items".into(),
                }],
            }
        })
        .collect();

    Ok(Json(Collections { collections: cols }))
}

async fn collection_info(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Collection>, OgcError> {
    let row = sqlx::query("SELECT id, name, srid FROM datasets WHERE id = $1")
        .bind(id)
        .fetch_optional(store.read_pool())
        .await?
        .ok_or_else(|| OgcError::NotFound("collection not found".into()))?;

    let name: String = row.get("name");
    let srid: i32 = row.get("srid");
    Ok(Json(Collection {
        id: id.to_string(),
        title: name.clone(),
        description: format!("Dataset: {name}"),
        extent: None,
        crs: supported_crs_uris(srid),
        storage_crs: CRS84_URI.to_string(),
        links: vec![Link {
            href: format!("/api/v1/ogc/collections/{id}/items"),
            rel: "items".into(),
            media_type: "application/geo+json".into(),
            title: "Items".into(),
        }],
    }))
}

#[derive(Deserialize)]
struct ItemsQuery {
    #[serde(default = "default_items_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    /// branch to query (defaults to main)
    branch: Option<Uuid>,
    /// bbox filter: minx,miny,maxx,maxy
    bbox: Option<String>,
    /// CRS URI the geometry is returned in
    crs: Option<String>,
    /// CRS URI the bbox is given in
    #[serde(rename = "bbox-crs")]
    bbox_crs: Option<String>,
}

fn default_items_limit() -> i64 {
    100
}

#[derive(Serialize)]
struct FeatureCollection {
    #[serde(rename = "type")]
    fc_type: String,
    features: Vec<serde_json::Value>,
    #[serde(rename = "numberMatched")]
    number_matched: i64,
    #[serde(rename = "numberReturned")]
    number_returned: usize,
}

/// The branch a collection request reads: the one asked for, else `main`. Shared
/// so the item and the listing can never disagree about which branch they serve.
async fn collection_branch(
    store: &AppState,
    dataset_id: Uuid,
    requested: Option<Uuid>,
) -> Result<Uuid, OgcError> {
    if let Some(b) = requested {
        return Ok(b);
    }
    let row =
        sqlx::query("SELECT id FROM branches WHERE dataset_id = $1 AND name = 'main' LIMIT 1")
            .bind(dataset_id)
            .fetch_optional(store.read_pool())
            .await?
            .ok_or_else(|| OgcError::NotFound("no main branch".into()))?;
    Ok(row.get("id"))
}

async fn items(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Query(q): Query<ItemsQuery>,
) -> Result<impl IntoResponse, OgcError> {
    let branch_id = collection_branch(&store, dataset_id, q.branch).await?;
    let dataset_srid = match q.crs.is_some() || q.bbox_crs.is_some() {
        true => dataset_srid(&store, dataset_id).await?,
        false => STORAGE_SRID,
    };
    let output_crs = resolve_crs(&store, dataset_srid, "crs", q.crs.as_deref()).await?;
    let bbox_crs = resolve_crs(&store, dataset_srid, "bbox-crs", q.bbox_crs.as_deref()).await?;

    // Parse the bbox first: an external source pushes it down onto the relation's
    // own geometry column, so the source depends on whether there is one.
    let bbox = match &q.bbox {
        None => None,
        Some(bbox_str) => {
            let parts: Vec<f64> = bbox_str.split(',').filter_map(|s| s.parse().ok()).collect();
            if parts.len() != 4 {
                return Err(OgcError::BadRequest("invalid bbox format".into()));
            }
            let ordered = match bbox_crs.swap_axes {
                true => vec![parts[1], parts[0], parts[3], parts[2]],
                false => parts,
            };
            Some(ordered)
        }
    };

    let envelope = format!("ST_MakeEnvelope($2, $3, $4, $5, {})", bbox_crs.srid);
    let envelope_4326 = match bbox_crs.srid == STORAGE_SRID {
        true => envelope,
        false => format!("ST_Transform({envelope}, {STORAGE_SRID})"),
    };
    let geometry = geometry_expression("geometry", &output_crs);

    // an external dataset swaps in a derived table over the team's relation;
    // the ordinary changeset-chain query is untouched
    let (external, prelude, source) = store
        .latest_source_overlapping(
            branch_id,
            ptolemy_storage::LATEST_COLUMNS,
            bbox.as_ref().map(|_| envelope_4326.as_str()),
        )
        .await?;
    let pool = store.source_pool(external.as_ref()).await?;

    let features = if let Some(parts) = &bbox {
        sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, ST_AsGeoJSON({geometry})::jsonb as geojson, properties
            FROM {source}
            WHERE operation != 'delete'
              AND geometry IS NOT NULL
              AND geometry && {envelope_4326}
            LIMIT $6 OFFSET $7"
        ))
        .bind(branch_id)
        .bind(parts[0])
        .bind(parts[1])
        .bind(parts[2])
        .bind(parts[3])
        .bind(q.limit)
        .bind(q.offset)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, ST_AsGeoJSON({geometry})::jsonb as geojson, properties
            FROM {source}
            WHERE operation != 'delete'
            LIMIT $2 OFFSET $3"
        ))
        .bind(branch_id)
        .bind(q.limit)
        .bind(q.offset)
        .fetch_all(pool)
        .await?
    };

    let geojson_features: Vec<serde_json::Value> = features
        .iter()
        .map(|row| {
            let fid: Uuid = row.get("feature_id");
            let geom: Option<serde_json::Value> = row.get("geojson");
            let props: serde_json::Value = row.get("properties");
            serde_json::json!({
                "type": "Feature",
                "id": fid.to_string(),
                "geometry": geom,
                "properties": props
            })
        })
        .collect();

    let count = geojson_features.len();
    Ok((
        content_crs_header(&output_crs),
        Json(FeatureCollection {
            fc_type: "FeatureCollection".into(),
            features: geojson_features,
            number_matched: count as i64,
            number_returned: count,
        }),
    ))
}

async fn item(
    State(store): State<AppState>,
    Path((dataset_id, feature_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<ItemsQuery>,
) -> Result<impl IntoResponse, OgcError> {
    // this read used to take the newest feature_versions row for the id in the
    // whole database, ignoring branch and dataset, so two branches holding
    // different values both answered with whichever was written last. It now
    // resolves through the branch's ancestor chain like the listing does.
    let branch_id = collection_branch(&store, dataset_id, q.branch).await?;
    let dataset_srid = match q.crs.is_some() {
        true => dataset_srid(&store, dataset_id).await?,
        false => STORAGE_SRID,
    };
    let output_crs = resolve_crs(&store, dataset_srid, "crs", q.crs.as_deref()).await?;
    let geometry = geometry_expression("f.geometry", &output_crs);

    let (external, source) = store.features_source_at(branch_id, "$2").await?;
    let row = sqlx::query(&format!(
        "SELECT f.id as feature_id, ST_AsGeoJSON({geometry})::jsonb as geojson, f.properties
         FROM {source} f WHERE f.id = $1"
    ))
    .bind(feature_id)
    .bind(branch_id)
    .fetch_optional(store.source_pool(external.as_ref()).await?)
    .await?
    .ok_or_else(|| OgcError::NotFound("feature not found".into()))?;

    let geom: Option<serde_json::Value> = row.get("geojson");
    let props: serde_json::Value = row.get("properties");

    Ok((
        content_crs_header(&output_crs),
        Json(serde_json::json!({
            "type": "Feature",
            "id": feature_id.to_string(),
            "geometry": geom,
            "properties": props
        })),
    ))
}

// ─── Audit Log ──────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: i64,
    actor: Option<String>,
}

fn default_audit_limit() -> i64 {
    100
}

async fn list_audit(
    State(store): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEntry>>, OgcError> {
    let entries = store.list_audit_log(q.limit, q.actor.as_deref()).await?;
    Ok(Json(entries))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum OgcError {
    Store(sqlx::Error),
    StoreErr(ptolemy_storage::StoreError),
    NotFound(String),
    BadRequest(String),
}

impl From<sqlx::Error> for OgcError {
    fn from(e: sqlx::Error) -> Self {
        OgcError::Store(e)
    }
}

impl From<ptolemy_storage::StoreError> for OgcError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        OgcError::StoreErr(e)
    }
}

impl IntoResponse for OgcError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            OgcError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            OgcError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            OgcError::Store(e) => {
                crate::errors::log_db_error("ogc", &e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            OgcError::StoreErr(e) => crate::errors::store_error_status(&e),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
