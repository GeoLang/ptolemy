// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Industry vertical endpoints: environmental, construction, agriculture, telecom, emergency.

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

pub fn vertical_routes() -> Router<AppState> {
    Router::new()
        // Environmental
        .route("/sensors", get(list_sensors))
        .route("/sensors/readings", get(sensor_readings))
        // Construction
        .route("/surveys/compare", post(survey_compare))
        .route("/construction/surveys", get(list_surveys))
        .route("/construction/milestones", get(list_milestones))
        // Agriculture
        .route("/fields", get(list_fields))
        .route("/fields/ndvi", get(field_ndvi))
        // Telecom
        .route("/towers", get(list_towers))
        .route("/coverage/simulate", post(coverage_simulate))
        // Emergency
        .route("/incidents", get(list_incidents))
        .route("/incidents", post(create_incident))
        .route("/incidents/evacuate", post(evacuation_route))
}

// ═══════════════════════════════════════════════════════════════════
// Environmental — IoT sensor management
// ═══════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct SensorListParams {
    branch_id: Uuid,
    #[serde(default)]
    sensor_type: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct SensorInfo {
    id: Uuid,
    name: Option<String>,
    sensor_type: Option<String>,
    lat: Option<f64>,
    lng: Option<f64>,
    status: Option<String>,
    properties: serde_json::Value,
}

async fn list_sensors(
    State(store): State<AppState>,
    Query(params): Query<SensorListParams>,
) -> Result<Json<Vec<SensorInfo>>, VerticalError> {
    let limit = params.limit.clamp(1, 500);
    let features = store
        .list_features_paginated(params.branch_id, None, limit)
        .await
        .map_err(VerticalError::Store)?;

    let sensors: Vec<SensorInfo> = features
        .into_iter()
        .filter(|f| {
            if let Some(ref t) = params.sensor_type {
                f.properties
                    .get("sensor_type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == t.as_str())
            } else {
                true
            }
        })
        .map(|f| {
            let p = &f.properties;
            SensorInfo {
                id: f.id,
                name: p.get("name").and_then(|v| v.as_str()).map(String::from),
                sensor_type: p
                    .get("sensor_type")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                lat: p.get("lat").and_then(|v| v.as_f64()),
                lng: p.get("lng").and_then(|v| v.as_f64()),
                status: p.get("status").and_then(|v| v.as_str()).map(String::from),
                properties: f.properties,
            }
        })
        .collect();

    Ok(Json(sensors))
}

#[derive(Deserialize)]
struct SensorReadingsParams {
    branch_id: Uuid,
    sensor_id: Uuid,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct SensorReading {
    timestamp: Option<String>,
    value: Option<f64>,
    unit: Option<String>,
    properties: serde_json::Value,
}

async fn sensor_readings(
    State(store): State<AppState>,
    Query(params): Query<SensorReadingsParams>,
) -> Result<Json<Vec<SensorReading>>, VerticalError> {
    let limit = params.limit.clamp(1, 1000);
    let features = store
        .list_features_paginated(params.branch_id, None, limit)
        .await
        .map_err(VerticalError::Store)?;

    let readings: Vec<SensorReading> = features
        .into_iter()
        .filter(|f| {
            f.properties
                .get("sensor_id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == params.sensor_id.to_string())
        })
        .map(|f| {
            let p = &f.properties;
            SensorReading {
                timestamp: p
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                value: p.get("value").and_then(|v| v.as_f64()),
                unit: p.get("unit").and_then(|v| v.as_str()).map(String::from),
                properties: f.properties,
            }
        })
        .collect();

    Ok(Json(readings))
}

// ═══════════════════════════════════════════════════════════════════
// Construction — survey comparison, milestones
// ═══════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct SurveyCompareRequest {
    branch_id: Uuid,
    /// Feature IDs of two surveys to compare
    survey_a: Uuid,
    survey_b: Uuid,
}

#[derive(Serialize)]
struct SurveyCompareResponse {
    survey_a: Uuid,
    survey_b: Uuid,
    point_count_a: usize,
    point_count_b: usize,
    elevation_diff_stats: Option<ElevationStats>,
}

#[derive(Serialize)]
struct ElevationStats {
    mean_diff: f64,
    max_cut: f64,
    max_fill: f64,
    net_volume_m3: f64,
}

async fn survey_compare(
    State(store): State<AppState>,
    Json(req): Json<SurveyCompareRequest>,
) -> Result<Json<SurveyCompareResponse>, VerticalError> {
    // Fetch both surveys
    let features = store
        .list_features_paginated(req.branch_id, None, 10000)
        .await
        .map_err(VerticalError::Store)?;

    let survey_a = features.iter().find(|f| f.id == req.survey_a);
    let survey_b = features.iter().find(|f| f.id == req.survey_b);

    let (count_a, count_b) = (
        survey_a
            .and_then(|f| f.properties.get("point_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        survey_b
            .and_then(|f| f.properties.get("point_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
    );

    // If both surveys have elevation data, compute diff stats
    let stats = match (survey_a, survey_b) {
        (Some(a), Some(b)) => {
            let mean_a = a
                .properties
                .get("mean_elevation")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let mean_b = b
                .properties
                .get("mean_elevation")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let diff = mean_b - mean_a;
            Some(ElevationStats {
                mean_diff: diff,
                max_cut: diff.min(0.0).abs(),
                max_fill: diff.max(0.0),
                net_volume_m3: diff * (count_b as f64), // simplified estimate
            })
        }
        _ => None,
    };

    Ok(Json(SurveyCompareResponse {
        survey_a: req.survey_a,
        survey_b: req.survey_b,
        point_count_a: count_a,
        point_count_b: count_b,
        elevation_diff_stats: stats,
    }))
}

#[derive(Deserialize)]
struct SurveyListParams {
    branch_id: Uuid,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct SurveyInfo {
    id: Uuid,
    name: Option<String>,
    date: Option<String>,
    point_count: Option<u64>,
    mean_elevation: Option<f64>,
}

async fn list_surveys(
    State(store): State<AppState>,
    Query(params): Query<SurveyListParams>,
) -> Result<Json<Vec<SurveyInfo>>, VerticalError> {
    let features = store
        .list_features_paginated(params.branch_id, None, params.limit.clamp(1, 500))
        .await
        .map_err(VerticalError::Store)?;

    let surveys: Vec<SurveyInfo> = features
        .into_iter()
        .filter(|f| f.properties.get("survey_name").is_some())
        .map(|f| {
            let p = &f.properties;
            SurveyInfo {
                id: f.id,
                name: p
                    .get("survey_name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                date: p
                    .get("survey_date")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                point_count: p.get("point_count").and_then(|v| v.as_u64()),
                mean_elevation: p.get("mean_elevation").and_then(|v| v.as_f64()),
            }
        })
        .collect();

    Ok(Json(surveys))
}

#[derive(Deserialize)]
struct MilestoneParams {
    branch_id: Uuid,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct Milestone {
    id: Uuid,
    name: Option<String>,
    status: Option<String>,
    due_date: Option<String>,
    completion_pct: Option<f64>,
    planned_pct: Option<f64>,
}

async fn list_milestones(
    State(store): State<AppState>,
    Query(params): Query<MilestoneParams>,
) -> Result<Json<Vec<Milestone>>, VerticalError> {
    let features = store
        .list_features_paginated(params.branch_id, None, params.limit.clamp(1, 200))
        .await
        .map_err(VerticalError::Store)?;

    let milestones: Vec<Milestone> = features
        .into_iter()
        .filter(|f| f.properties.get("milestone_name").is_some())
        .map(|f| {
            let p = &f.properties;
            Milestone {
                id: f.id,
                name: p
                    .get("milestone_name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                status: p.get("status").and_then(|v| v.as_str()).map(String::from),
                due_date: p.get("due_date").and_then(|v| v.as_str()).map(String::from),
                completion_pct: p.get("completion_pct").and_then(|v| v.as_f64()),
                planned_pct: p.get("planned_pct").and_then(|v| v.as_f64()),
            }
        })
        .collect();

    Ok(Json(milestones))
}

// ═══════════════════════════════════════════════════════════════════
// Agriculture — field management, NDVI
// ═══════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct FieldListParams {
    branch_id: Uuid,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct FieldInfo {
    id: Uuid,
    name: Option<String>,
    crop: Option<String>,
    area_ha: Option<f64>,
    soil_type: Option<String>,
    properties: serde_json::Value,
}

async fn list_fields(
    State(store): State<AppState>,
    Query(params): Query<FieldListParams>,
) -> Result<Json<Vec<FieldInfo>>, VerticalError> {
    let features = store
        .list_features_paginated(params.branch_id, None, params.limit.clamp(1, 500))
        .await
        .map_err(VerticalError::Store)?;

    let fields: Vec<FieldInfo> = features
        .into_iter()
        .map(|f| {
            let p = &f.properties;
            FieldInfo {
                id: f.id,
                name: p.get("name").and_then(|v| v.as_str()).map(String::from),
                crop: p.get("crop").and_then(|v| v.as_str()).map(String::from),
                area_ha: p.get("area_ha").and_then(|v| v.as_f64()),
                soil_type: p
                    .get("soil_type")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                properties: f.properties,
            }
        })
        .collect();

    Ok(Json(fields))
}

#[derive(Deserialize)]
struct NdviParams {
    branch_id: Uuid,
    field_id: Uuid,
}

#[derive(Serialize)]
struct NdviResponse {
    field_id: Uuid,
    mean_ndvi: Option<f64>,
    min_ndvi: Option<f64>,
    max_ndvi: Option<f64>,
    timestamp: Option<String>,
    health_classification: String,
}

async fn field_ndvi(
    State(store): State<AppState>,
    Query(params): Query<NdviParams>,
) -> Result<Json<NdviResponse>, VerticalError> {
    let features = store
        .list_features_paginated(params.branch_id, None, 10000)
        .await
        .map_err(VerticalError::Store)?;

    let field = features
        .into_iter()
        .find(|f| f.id == params.field_id)
        .ok_or_else(|| VerticalError::BadRequest("field not found".into()))?;

    let p = &field.properties;
    let mean = p.get("ndvi_mean").and_then(|v| v.as_f64());
    let health = match mean {
        Some(v) if v > 0.6 => "healthy",
        Some(v) if v > 0.3 => "moderate_stress",
        Some(_) => "severe_stress",
        None => "no_data",
    };

    Ok(Json(NdviResponse {
        field_id: params.field_id,
        mean_ndvi: mean,
        min_ndvi: p.get("ndvi_min").and_then(|v| v.as_f64()),
        max_ndvi: p.get("ndvi_max").and_then(|v| v.as_f64()),
        timestamp: p
            .get("ndvi_timestamp")
            .and_then(|v| v.as_str())
            .map(String::from),
        health_classification: health.into(),
    }))
}

// ═══════════════════════════════════════════════════════════════════
// Telecom — tower inventory, RF coverage simulation
// ═══════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct TowerListParams {
    branch_id: Uuid,
    #[serde(default)]
    technology: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct TowerInfo {
    id: Uuid,
    name: Option<String>,
    technology: Option<String>,
    height_m: Option<f64>,
    frequency_mhz: Option<f64>,
    lat: Option<f64>,
    lng: Option<f64>,
    properties: serde_json::Value,
}

async fn list_towers(
    State(store): State<AppState>,
    Query(params): Query<TowerListParams>,
) -> Result<Json<Vec<TowerInfo>>, VerticalError> {
    let features = store
        .list_features_paginated(params.branch_id, None, params.limit.clamp(1, 500))
        .await
        .map_err(VerticalError::Store)?;

    let towers: Vec<TowerInfo> = features
        .into_iter()
        .filter(|f| {
            if let Some(ref tech) = params.technology {
                f.properties
                    .get("technology")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == tech.as_str())
            } else {
                true
            }
        })
        .map(|f| {
            let p = &f.properties;
            TowerInfo {
                id: f.id,
                name: p.get("name").and_then(|v| v.as_str()).map(String::from),
                technology: p
                    .get("technology")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                height_m: p.get("height_m").and_then(|v| v.as_f64()),
                frequency_mhz: p.get("frequency_mhz").and_then(|v| v.as_f64()),
                lat: p.get("lat").and_then(|v| v.as_f64()),
                lng: p.get("lng").and_then(|v| v.as_f64()),
                properties: f.properties,
            }
        })
        .collect();

    Ok(Json(towers))
}

#[derive(Deserialize)]
struct CoverageSimRequest {
    tower_lat: f64,
    tower_lng: f64,
    height_m: f64,
    frequency_mhz: f64,
    power_dbm: f64,
    /// Simulation radius in meters
    #[serde(default = "default_coverage_radius")]
    radius_m: f64,
}

fn default_coverage_radius() -> f64 {
    5000.0
}

#[derive(Serialize)]
struct CoverageSimResponse {
    tower: [f64; 2],
    radius_m: f64,
    estimated_coverage_area_km2: f64,
    signal_at_edge_dbm: f64,
    /// Simplified coverage polygon (circle approximation)
    coverage_geojson: serde_json::Value,
}

async fn coverage_simulate(
    Json(req): Json<CoverageSimRequest>,
) -> Result<Json<CoverageSimResponse>, VerticalError> {
    // Simplified Hata model for signal propagation
    // Path loss (dB) = 69.55 + 26.16*log10(f) - 13.82*log10(h) + (44.9 - 6.55*log10(h))*log10(d)
    let f = req.frequency_mhz;
    let h = req.height_m.max(1.0);
    let d_km = req.radius_m / 1000.0;

    let path_loss =
        69.55 + 26.16 * f.log10() - 13.82 * h.log10() + (44.9 - 6.55 * h.log10()) * d_km.log10();
    let signal_at_edge = req.power_dbm - path_loss;

    let area_km2 = std::f64::consts::PI * d_km * d_km;

    // Generate coverage circle as GeoJSON
    let radius_deg = req.radius_m / 111_320.0;
    let coords: Vec<[f64; 2]> = (0..=32)
        .map(|i| {
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / 32.0;
            [
                req.tower_lng + radius_deg * angle.cos(),
                req.tower_lat + radius_deg * angle.sin(),
            ]
        })
        .collect();

    let geojson = serde_json::json!({
        "type": "Feature",
        "geometry": {
            "type": "Polygon",
            "coordinates": [coords]
        },
        "properties": {
            "signal_at_edge_dbm": signal_at_edge,
            "frequency_mhz": f,
            "height_m": h
        }
    });

    Ok(Json(CoverageSimResponse {
        tower: [req.tower_lng, req.tower_lat],
        radius_m: req.radius_m,
        estimated_coverage_area_km2: area_km2,
        signal_at_edge_dbm: signal_at_edge,
        coverage_geojson: geojson,
    }))
}

// ═══════════════════════════════════════════════════════════════════
// Emergency — incident management, evacuation
// ═══════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct IncidentListParams {
    branch_id: Uuid,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incident_type: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct IncidentInfo {
    id: Uuid,
    incident_type: Option<String>,
    severity: Option<String>,
    status: Option<String>,
    lat: Option<f64>,
    lng: Option<f64>,
    reported_at: Option<String>,
    description: Option<String>,
    properties: serde_json::Value,
}

async fn list_incidents(
    State(store): State<AppState>,
    Query(params): Query<IncidentListParams>,
) -> Result<Json<Vec<IncidentInfo>>, VerticalError> {
    let features = store
        .list_features_paginated(params.branch_id, None, params.limit.clamp(1, 500))
        .await
        .map_err(VerticalError::Store)?;

    let incidents: Vec<IncidentInfo> = features
        .into_iter()
        .filter(|f| {
            let p = &f.properties;
            let status_ok = params.status.as_ref().is_none_or(|s| {
                p.get("status")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| v == s.as_str())
            });
            let type_ok = params.incident_type.as_ref().is_none_or(|t| {
                p.get("incident_type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| v == t.as_str())
            });
            status_ok && type_ok
        })
        .map(|f| {
            let p = &f.properties;
            IncidentInfo {
                id: f.id,
                incident_type: p
                    .get("incident_type")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                severity: p.get("severity").and_then(|v| v.as_str()).map(String::from),
                status: p.get("status").and_then(|v| v.as_str()).map(String::from),
                lat: p.get("lat").and_then(|v| v.as_f64()),
                lng: p.get("lng").and_then(|v| v.as_f64()),
                reported_at: p
                    .get("reported_at")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                description: p
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                properties: f.properties,
            }
        })
        .collect();

    Ok(Json(incidents))
}

#[derive(Deserialize)]
struct CreateIncidentRequest {
    branch_id: Uuid,
    incident_type: String,
    severity: String,
    lat: f64,
    lng: f64,
    description: String,
    author: String,
}

async fn create_incident(
    State(store): State<AppState>,
    actor: Actor,
    Json(req): Json<CreateIncidentRequest>,
) -> Result<(StatusCode, Json<IncidentInfo>), VerticalError> {
    use ptolemy_core::diff::DiffOp;

    let feature_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let properties = serde_json::json!({
        "incident_type": req.incident_type,
        "severity": req.severity,
        "status": "active",
        "lat": req.lat,
        "lng": req.lng,
        "description": req.description,
        "reported_at": now,
    });

    // Create a point WKB (EWKB with SRID 4326)
    let wkb = point_to_ewkb(req.lng, req.lat);

    let ops = vec![DiffOp::Insert {
        feature_id,
        geometry_wkb: wkb,
        properties: properties.clone(),
    }];

    store
        .commit(
            req.branch_id,
            "new incident",
            actor.or_body(&req.author),
            &ops,
        )
        .await
        .map_err(VerticalError::Store)?;

    Ok((
        StatusCode::CREATED,
        Json(IncidentInfo {
            id: feature_id,
            incident_type: Some(req.incident_type),
            severity: Some(req.severity),
            status: Some("active".into()),
            lat: Some(req.lat),
            lng: Some(req.lng),
            reported_at: Some(now),
            description: Some(req.description),
            properties,
        }),
    ))
}

#[derive(Deserialize)]
struct EvacuationRequest {
    incident_lat: f64,
    incident_lng: f64,
    /// Radius of danger zone in meters
    radius_m: f64,
    /// Safe assembly points
    assembly_points: Vec<AssemblyPoint>,
}

#[derive(Deserialize)]
struct AssemblyPoint {
    id: String,
    lat: f64,
    lng: f64,
    capacity: u32,
}

#[derive(Serialize)]
struct EvacuationResponse {
    danger_zone_geojson: serde_json::Value,
    assembly_points: Vec<EvacAssemblyPoint>,
}

#[derive(Serialize)]
struct EvacAssemblyPoint {
    id: String,
    lat: f64,
    lng: f64,
    capacity: u32,
    distance_m: f64,
    estimated_travel_s: f64,
}

async fn evacuation_route(
    Json(req): Json<EvacuationRequest>,
) -> Result<Json<EvacuationResponse>, VerticalError> {
    if req.assembly_points.is_empty() {
        return Err(VerticalError::BadRequest(
            "at least one assembly point required".into(),
        ));
    }

    // Danger zone circle
    let radius_deg = req.radius_m / 111_320.0;
    let coords: Vec<[f64; 2]> = (0..=32)
        .map(|i| {
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / 32.0;
            [
                req.incident_lng + radius_deg * angle.cos(),
                req.incident_lat + radius_deg * angle.sin(),
            ]
        })
        .collect();

    let danger_zone = serde_json::json!({
        "type": "Feature",
        "geometry": {
            "type": "Polygon",
            "coordinates": [coords]
        },
        "properties": {
            "type": "danger_zone",
            "radius_m": req.radius_m
        }
    });

    let mut points: Vec<EvacAssemblyPoint> = req
        .assembly_points
        .into_iter()
        .map(|ap| {
            let dist = haversine_m(req.incident_lat, req.incident_lng, ap.lat, ap.lng);
            // Assume walking speed 5 km/h for evacuation
            let travel_s = dist / (5000.0 / 3600.0);
            EvacAssemblyPoint {
                id: ap.id,
                lat: ap.lat,
                lng: ap.lng,
                capacity: ap.capacity,
                distance_m: dist,
                estimated_travel_s: travel_s,
            }
        })
        .collect();

    points.sort_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap());

    Ok(Json(EvacuationResponse {
        danger_zone_geojson: danger_zone,
        assembly_points: points,
    }))
}

// ─── Error ──────────────────────────────────────────────────────────

enum VerticalError {
    Store(ptolemy_storage::StoreError),
    BadRequest(String),
}

impl IntoResponse for VerticalError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            VerticalError::Store(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            VerticalError::Store(ptolemy_storage::StoreError::Conflict(msg)) => {
                (StatusCode::CONFLICT, msg)
            }
            VerticalError::Store(ptolemy_storage::StoreError::Db(e)) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            VerticalError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

fn default_limit() -> i64 {
    50
}

fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let r = 6_371_000.0;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lng = (lng2 - lng1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lng / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    r * c
}

/// Create EWKB point with SRID 4326.
fn point_to_ewkb(lng: f64, lat: f64) -> Vec<u8> {
    let mut wkb = Vec::with_capacity(25);
    wkb.push(0x01); // little-endian
    wkb.extend_from_slice(&0x20000001u32.to_le_bytes()); // point with SRID flag
    wkb.extend_from_slice(&4326u32.to_le_bytes()); // SRID
    wkb.extend_from_slice(&lng.to_le_bytes());
    wkb.extend_from_slice(&lat.to_le_bytes());
    wkb
}
