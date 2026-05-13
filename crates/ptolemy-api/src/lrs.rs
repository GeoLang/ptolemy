// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Linear referencing: routes and events along measured geometries.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn lrs_routes() -> Router<AppState> {
    Router::new()
        .route("/datasets/{id}/routes", get(list_routes).post(create_route))
        .route("/routes/{id}", get(get_route))
        .route("/routes/{id}/events", get(list_events).post(create_event))
        .route("/routes/{id}/locate", get(locate_point))
        .route("/routes/{id}/subline", get(get_subline))
}

#[derive(Serialize)]
struct Route {
    id: Uuid,
    name: String,
    total_length: Option<f64>,
    geometry: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct RouteEvent {
    id: Uuid,
    event_type: String,
    from_measure: f64,
    to_measure: Option<f64>,
    properties: serde_json::Value,
    geometry: Option<serde_json::Value>,
}

async fn list_routes(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Route>>, LrsError> {
    let rows = sqlx::query(
        "SELECT id, name, total_length, ST_AsGeoJSON(geometry)::jsonb as geojson
         FROM routes WHERE dataset_id = $1 ORDER BY name",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| Route {
                id: r.get("id"),
                name: r.get("name"),
                total_length: r.get("total_length"),
                geometry: r.get("geojson"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateRouteRequest {
    name: String,
    geometry_wkb_hex: String,
}

async fn create_route(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateRouteRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), LrsError> {
    let id = Uuid::now_v7();
    let wkb =
        hex::decode(&req.geometry_wkb_hex).map_err(|_| LrsError::Bad("invalid hex".into()))?;
    sqlx::query(
        "INSERT INTO routes (id, dataset_id, name, geometry, total_length)
         VALUES ($1, $2, $3, ST_GeomFromWKB($4, 4326), ST_Length(ST_GeomFromWKB($4, 4326)::geography))",
    ).bind(id).bind(dataset_id).bind(&req.name).bind(&wkb)
    .execute(store.pool()).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_route(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Route>, LrsError> {
    let r = sqlx::query(
        "SELECT id, name, total_length, ST_AsGeoJSON(geometry)::jsonb as geojson FROM routes WHERE id = $1",
    ).bind(id).fetch_optional(store.pool()).await?.ok_or(LrsError::NotFound)?;
    Ok(Json(Route {
        id: r.get("id"),
        name: r.get("name"),
        total_length: r.get("total_length"),
        geometry: r.get("geojson"),
    }))
}

async fn list_events(
    State(store): State<AppState>,
    Path(route_id): Path<Uuid>,
) -> Result<Json<Vec<RouteEvent>>, LrsError> {
    let rows = sqlx::query(
        "SELECT id, event_type, from_measure, to_measure, properties,
                ST_AsGeoJSON(geometry)::jsonb as geojson
         FROM route_events WHERE route_id = $1 ORDER BY from_measure",
    )
    .bind(route_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| RouteEvent {
                id: r.get("id"),
                event_type: r.get("event_type"),
                from_measure: r.get("from_measure"),
                to_measure: r.get("to_measure"),
                properties: r.get("properties"),
                geometry: r.get("geojson"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateEventRequest {
    event_type: String,
    from_measure: f64,
    to_measure: Option<f64>,
    #[serde(default)]
    properties: serde_json::Value,
}

async fn create_event(
    State(store): State<AppState>,
    Path(route_id): Path<Uuid>,
    Json(req): Json<CreateEventRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), LrsError> {
    let id = Uuid::now_v7();
    // Generate geometry from route + measures using ST_LocateAlong / ST_LocateBetween
    if req.to_measure.is_some() {
        // Linear event
        sqlx::query(
            "INSERT INTO route_events (id, route_id, event_type, from_measure, to_measure, properties, geometry)
             SELECT $1, $2, $3, $4, $5, $6,
                    ST_LocateBetween(r.geometry, $4, $5)
             FROM routes r WHERE r.id = $2",
        ).bind(id).bind(route_id).bind(&req.event_type)
        .bind(req.from_measure).bind(req.to_measure).bind(&req.properties)
        .execute(store.pool()).await?;
    } else {
        // Point event
        sqlx::query(
            "INSERT INTO route_events (id, route_id, event_type, from_measure, properties, geometry)
             SELECT $1, $2, $3, $4, $5,
                    ST_LocateAlong(r.geometry, $4)
             FROM routes r WHERE r.id = $2",
        ).bind(id).bind(route_id).bind(&req.event_type)
        .bind(req.from_measure).bind(&req.properties)
        .execute(store.pool()).await?;
    }
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

/// Locate a point along a route (return measure value for a coordinate).
#[derive(Deserialize)]
struct LocateQuery {
    lng: f64,
    lat: f64,
}

async fn locate_point(
    State(store): State<AppState>,
    Path(route_id): Path<Uuid>,
    Query(q): Query<LocateQuery>,
) -> Result<Json<serde_json::Value>, LrsError> {
    let row = sqlx::query(
        "SELECT ST_LineLocatePoint(geometry, ST_SetSRID(ST_MakePoint($2, $3), 4326)) as fraction,
                total_length
         FROM routes WHERE id = $1",
    )
    .bind(route_id)
    .bind(q.lng)
    .bind(q.lat)
    .fetch_optional(store.pool())
    .await?
    .ok_or(LrsError::NotFound)?;

    let fraction: f64 = row.get("fraction");
    let length: Option<f64> = row.get("total_length");
    let measure = fraction * length.unwrap_or(1.0);

    Ok(Json(serde_json::json!({
        "fraction": fraction,
        "measure": measure,
    })))
}

/// Get a sub-line between two measures.
#[derive(Deserialize)]
struct SublineQuery {
    from_measure: f64,
    to_measure: f64,
}

async fn get_subline(
    State(store): State<AppState>,
    Path(route_id): Path<Uuid>,
    Query(q): Query<SublineQuery>,
) -> Result<Json<serde_json::Value>, LrsError> {
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(
            ST_LineSubstring(geometry,
                LEAST($2 / NULLIF(total_length, 0), 1.0),
                LEAST($3 / NULLIF(total_length, 0), 1.0)
            )
         )::jsonb as geojson
         FROM routes WHERE id = $1",
    )
    .bind(route_id)
    .bind(q.from_measure)
    .bind(q.to_measure)
    .fetch_optional(store.pool())
    .await?
    .ok_or(LrsError::NotFound)?;

    Ok(Json(row.get("geojson")))
}

// ─── Error ──────────────────────────────────────────────────────────

enum LrsError {
    Db(sqlx::Error),
    NotFound,
    Bad(String),
}
impl From<sqlx::Error> for LrsError {
    fn from(e: sqlx::Error) -> Self {
        LrsError::Db(e)
    }
}

impl IntoResponse for LrsError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            LrsError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            LrsError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            LrsError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
