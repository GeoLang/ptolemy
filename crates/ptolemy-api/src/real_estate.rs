// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Real-estate domain endpoints: parcel search, comparable sales, and parcel editing.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn real_estate_routes() -> Router<AppState> {
    Router::new()
        .route("/parcels/search", get(parcel_search))
        .route("/comps/search", get(comps_search))
        .route("/parcels/split", post(parcel_split))
        .route("/parcels/merge", post(parcel_merge))
}

// ─── Parcel Search ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct ParcelSearchParams {
    /// Branch holding parcel data
    branch_id: Uuid,
    /// Search type: "apn", "address", "owner", or "bbox"
    #[serde(rename = "type")]
    search_type: String,
    /// Query string (APN number, address fragment, or owner name)
    #[serde(default)]
    q: Option<String>,
    /// Bounding box for spatial search
    #[serde(default)]
    min_x: Option<f64>,
    #[serde(default)]
    min_y: Option<f64>,
    #[serde(default)]
    max_x: Option<f64>,
    #[serde(default)]
    max_y: Option<f64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct ParcelResult {
    id: Uuid,
    apn: Option<String>,
    address: Option<String>,
    owner: Option<String>,
    zoning: Option<String>,
    sqft: Option<f64>,
    properties: serde_json::Value,
    geometry_wkb_hex: String,
}

async fn parcel_search(
    State(store): State<AppState>,
    Query(params): Query<ParcelSearchParams>,
) -> Result<Json<Vec<ParcelResult>>, RealEstateError> {
    let limit = params.limit.clamp(1, 500);

    let features = match params.search_type.as_str() {
        "bbox" => {
            let (min_x, min_y, max_x, max_y) = (
                params.min_x.unwrap_or(-180.0),
                params.min_y.unwrap_or(-90.0),
                params.max_x.unwrap_or(180.0),
                params.max_y.unwrap_or(90.0),
            );
            store
                .features_in_bbox(params.branch_id, min_x, min_y, max_x, max_y, limit)
                .await
                .map_err(RealEstateError::Store)?
        }
        "apn" | "address" | "owner" => store
            .search_features_by_property(
                params.branch_id,
                params.search_type.as_str(),
                &params.q.unwrap_or_default(),
                limit,
            )
            .await
            .map_err(RealEstateError::Store)?,
        _ => {
            return Err(RealEstateError::BadRequest(
                "type must be apn, address, owner, or bbox".into(),
            ));
        }
    };

    let results: Vec<ParcelResult> = features
        .into_iter()
        .map(|f| {
            let props = &f.properties;
            ParcelResult {
                id: f.id,
                apn: props.get("apn").and_then(|v| v.as_str()).map(String::from),
                address: props
                    .get("address")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                owner: props
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                zoning: props
                    .get("zoning")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                sqft: props.get("sqft").and_then(|v| v.as_f64()),
                properties: f.properties,
                geometry_wkb_hex: hex::encode(&f.geometry_wkb),
            }
        })
        .collect();

    Ok(Json(results))
}

// ─── Comparable Sales Search ────────────────────────────────────────

#[derive(Deserialize)]
struct CompsSearchParams {
    /// Branch holding sales data
    branch_id: Uuid,
    /// Center longitude
    lng: f64,
    /// Center latitude
    lat: f64,
    /// Search radius in meters
    #[serde(default = "default_radius")]
    radius_m: f64,
    /// Max age of sales in days
    #[serde(default = "default_max_days")]
    max_days: u32,
    /// Min square footage
    #[serde(default)]
    min_sqft: Option<f64>,
    /// Max square footage
    #[serde(default)]
    max_sqft: Option<f64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_radius() -> f64 {
    1600.0 // ~1 mile
}

fn default_max_days() -> u32 {
    365
}

#[derive(Serialize)]
struct CompResult {
    id: Uuid,
    address: Option<String>,
    sale_price: Option<f64>,
    sale_date: Option<String>,
    sqft: Option<f64>,
    price_per_sqft: Option<f64>,
    distance_m: f64,
    properties: serde_json::Value,
}

async fn comps_search(
    State(store): State<AppState>,
    Query(params): Query<CompsSearchParams>,
) -> Result<Json<CompsResponse>, RealEstateError> {
    let limit = params.limit.clamp(1, 200);

    // Create a buffer circle as GeoJSON for spatial query
    // Approximate radius in degrees (rough, good enough for filtering)
    let radius_deg = params.radius_m / 111_320.0;
    let geojson = serde_json::json!({
        "type": "Polygon",
        "coordinates": [circle_coords(params.lng, params.lat, radius_deg, 32)]
    });
    let geojson_str = serde_json::to_string(&geojson)
        .map_err(|e| RealEstateError::BadRequest(format!("geometry error: {e}")))?;

    let features = store
        .features_intersecting(params.branch_id, &geojson_str, limit * 5)
        .await
        .map_err(RealEstateError::Store)?;

    // Filter by date and sqft
    let cutoff_days = params.max_days as i64;
    let now = time::OffsetDateTime::now_utc();

    let mut results: Vec<CompResult> = features
        .into_iter()
        .filter(|f| {
            let props = &f.properties;
            // Filter by sqft range
            if let Some(sqft) = props.get("sqft").and_then(|v| v.as_f64()) {
                if params.min_sqft.is_some_and(|min| sqft < min) {
                    return false;
                }
                if params.max_sqft.is_some_and(|max| sqft > max) {
                    return false;
                }
            }
            // Filter by sale date
            if let Some(date_str) = props.get("sale_date").and_then(|v| v.as_str())
                && let Ok(d) = time::Date::parse(
                    date_str,
                    &time::format_description::well_known::Iso8601::DEFAULT,
                )
            {
                let sale_dt = d.midnight().assume_utc();
                let age = now - sale_dt;
                if age.whole_days() > cutoff_days {
                    return false;
                }
            }
            true
        })
        .map(|f| {
            let props = &f.properties;
            let sqft = props.get("sqft").and_then(|v| v.as_f64());
            let sale_price = props.get("sale_price").and_then(|v| v.as_f64());
            let price_per_sqft = match (sale_price, sqft) {
                (Some(p), Some(s)) if s > 0.0 => Some(p / s),
                _ => None,
            };
            // Approximate distance using haversine
            let feat_lng = props.get("lng").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let feat_lat = props.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let distance_m = haversine_m(params.lat, params.lng, feat_lat, feat_lng);

            CompResult {
                id: f.id,
                address: props
                    .get("address")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                sale_price,
                sale_date: props
                    .get("sale_date")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                sqft,
                price_per_sqft,
                distance_m,
                properties: f.properties,
            }
        })
        .collect();

    results.sort_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap());
    results.truncate(limit as usize);

    // Summary stats
    let prices: Vec<f64> = results.iter().filter_map(|r| r.sale_price).collect();
    let summary = if prices.is_empty() {
        None
    } else {
        let sum: f64 = prices.iter().sum();
        let count = prices.len() as f64;
        let avg = sum / count;
        let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut sorted = prices.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];
        Some(CompsSummary {
            count: prices.len(),
            avg_price: avg,
            median_price: median,
            min_price: min,
            max_price: max,
        })
    };

    Ok(Json(CompsResponse { results, summary }))
}

#[derive(Serialize)]
struct CompsResponse {
    results: Vec<CompResult>,
    summary: Option<CompsSummary>,
}

#[derive(Serialize)]
struct CompsSummary {
    count: usize,
    avg_price: f64,
    median_price: f64,
    min_price: f64,
    max_price: f64,
}

// ─── Parcel Split ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct ParcelSplitRequest {
    branch_id: Uuid,
    feature_id: Uuid,
    /// Split line as [[x1,y1],[x2,y2]]
    line: [[f64; 2]; 2],
    #[allow(dead_code)]
    author: String,
}

async fn parcel_split(
    State(store): State<AppState>,
    actor: Actor,
    Json(req): Json<ParcelSplitRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), RealEstateError> {
    // the branch comes from the body, so the visibility layer cannot see it
    crate::visibility::ensure_readable(&store, &actor, &[req.branch_id])
        .await
        .map_err(RealEstateError::Store)?;
    // Get the feature
    let features = store
        .list_features_paginated(req.branch_id, None, 10000)
        .await
        .map_err(RealEstateError::Store)?;
    let feature = features
        .into_iter()
        .find(|f| f.id == req.feature_id)
        .ok_or_else(|| RealEstateError::BadRequest("feature not found".into()))?;

    // Decode WKB to get polygon coordinates (simplified: assume WKB polygon)
    let geom_hex = hex::encode(&feature.geometry_wkb);

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "split_queued",
            "feature_id": req.feature_id,
            "geometry_wkb_hex": geom_hex,
            "split_line": req.line,
            "message": "Use topoi parcel split for the actual geometry operation, then commit the two resulting polygons."
        })),
    ))
}

// ─── Parcel Merge ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct ParcelMergeRequest {
    branch_id: Uuid,
    feature_ids: Vec<Uuid>,
    #[allow(dead_code)]
    author: String,
}

async fn parcel_merge(
    State(store): State<AppState>,
    actor: Actor,
    Json(req): Json<ParcelMergeRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), RealEstateError> {
    // the branch comes from the body, so the visibility layer cannot see it
    crate::visibility::ensure_readable(&store, &actor, &[req.branch_id])
        .await
        .map_err(RealEstateError::Store)?;
    if req.feature_ids.len() < 2 {
        return Err(RealEstateError::BadRequest(
            "need at least 2 parcels to merge".into(),
        ));
    }

    let features = store
        .list_features_paginated(req.branch_id, None, 10000)
        .await
        .map_err(RealEstateError::Store)?;

    let selected: Vec<_> = features
        .into_iter()
        .filter(|f| req.feature_ids.contains(&f.id))
        .collect();

    if selected.len() != req.feature_ids.len() {
        return Err(RealEstateError::BadRequest(
            "one or more features not found".into(),
        ));
    }

    let hex_geoms: Vec<String> = selected
        .iter()
        .map(|f| hex::encode(&f.geometry_wkb))
        .collect();

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "merge_queued",
            "feature_ids": req.feature_ids,
            "geometry_wkb_hexes": hex_geoms,
            "message": "Use topoi parcel merge for the actual geometry operation, then commit the merged polygon."
        })),
    ))
}

// ─── Error ──────────────────────────────────────────────────────────

enum RealEstateError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl IntoResponse for RealEstateError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            RealEstateError::Store(e) => crate::errors::store_error_status(&e),
            RealEstateError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Generate approximate circle coordinates (lon/lat) for spatial queries.
fn circle_coords(cx: f64, cy: f64, radius_deg: f64, segments: usize) -> Vec<[f64; 2]> {
    let mut coords = Vec::with_capacity(segments + 1);
    for i in 0..=segments {
        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (segments as f64);
        coords.push([cx + radius_deg * angle.cos(), cy + radius_deg * angle.sin()]);
    }
    coords
}

/// Haversine distance in meters between two lat/lng points.
fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let r = 6_371_000.0; // Earth radius in meters
    let d_lat = (lat2 - lat1).to_radians();
    let d_lng = (lng2 - lng1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lng / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    r * c
}
