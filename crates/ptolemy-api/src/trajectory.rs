// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! MobilityDB trajectories — moving objects and temporal geometry.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn trajectory_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/trajectories",
            get(list_trajectories).post(create_trajectory),
        )
        .route("/trajectories/{id}", get(get_trajectory))
        .route("/trajectories/{id}/at", get(position_at_time))
        .route("/trajectories/{id}/speed", get(trajectory_speed))
        .route("/trajectories/{id}/distance", get(trajectory_distance))
        .route("/trajectories/{id}/simplify", post(simplify_trajectory))
        .route(
            "/datasets/{id}/trajectories/nearest",
            post(nearest_approach),
        )
}

#[derive(Serialize)]
struct Trajectory {
    id: Uuid,
    name: String,
    feature_id: Option<Uuid>,
    start_time: Option<String>,
    end_time: Option<String>,
}

async fn list_trajectories(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Trajectory>>, TrajError> {
    let rows = sqlx::query(
        "SELECT id, name, feature_id,
                lower(period)::text as start_time,
                upper(period)::text as end_time
         FROM trajectories WHERE dataset_id = $1 ORDER BY period",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Trajectory {
                id: r.get("id"),
                name: r.get("name"),
                feature_id: r.get("feature_id"),
                start_time: r.get("start_time"),
                end_time: r.get("end_time"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateTrajectoryRequest {
    name: String,
    feature_id: Option<Uuid>,
    /// Array of [lng, lat, timestamp_iso] points
    points: Vec<TrajectoryPoint>,
}

#[derive(Deserialize)]
struct TrajectoryPoint {
    lng: f64,
    lat: f64,
    timestamp: String,
}

async fn create_trajectory(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateTrajectoryRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), TrajError> {
    let id = Uuid::now_v7();

    // Build MobilityDB tgeompoint from points
    let instants: Vec<String> = req
        .points
        .iter()
        .map(|p| format!("POINT({} {})@{}", p.lng, p.lat, p.timestamp))
        .collect();
    let tgeompoint_str = format!("[{}]", instants.join(", "));

    sqlx::query(
        "INSERT INTO trajectories (id, dataset_id, feature_id, name, trip, period)
         VALUES ($1, $2, $3, $4, $5::tgeompoint, period($5::tgeompoint))",
    )
    .bind(id)
    .bind(dataset_id)
    .bind(req.feature_id)
    .bind(&req.name)
    .bind(&tgeompoint_str)
    .execute(store.pool())
    .await?;

    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_trajectory(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let r = sqlx::query(
        "SELECT id, name, feature_id,
                lower(period)::text as start_time,
                upper(period)::text as end_time,
                ST_AsGeoJSON(trajectory(trip))::jsonb as path_geojson,
                numInstants(trip) as num_points
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "id": r.get::<Uuid, _>("id"),
        "name": r.get::<String, _>("name"),
        "feature_id": r.get::<Option<Uuid>, _>("feature_id"),
        "start_time": r.get::<Option<String>, _>("start_time"),
        "end_time": r.get::<Option<String>, _>("end_time"),
        "path": r.get::<Option<serde_json::Value>, _>("path_geojson"),
        "num_points": r.get::<Option<i32>, _>("num_points"),
    })))
}

/// Get position at a specific time.
#[derive(Deserialize)]
struct PositionAtQuery {
    timestamp: String,
}

async fn position_at_time(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<PositionAtQuery>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let r = sqlx::query(
        "SELECT ST_AsGeoJSON(valueAtTimestamp(trip, $2::timestamptz))::jsonb as position
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .bind(&q.timestamp)
    .fetch_optional(store.pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "timestamp": q.timestamp,
        "position": r.get::<Option<serde_json::Value>, _>("position"),
    })))
}

/// Get speed along trajectory.
async fn trajectory_speed(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let r = sqlx::query(
        "SELECT twAvg(speed(trip)) as avg_speed,
                maxValue(speed(trip)) as max_speed,
                length(trip)::float as total_distance
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "avg_speed": r.get::<Option<f64>, _>("avg_speed"),
        "max_speed": r.get::<Option<f64>, _>("max_speed"),
        "total_distance": r.get::<Option<f64>, _>("total_distance"),
    })))
}

/// Get cumulative distance.
async fn trajectory_distance(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let r = sqlx::query(
        "SELECT length(trip)::float as distance,
                duration(period)::text as duration
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "distance_meters": r.get::<Option<f64>, _>("distance"),
        "duration": r.get::<Option<String>, _>("duration"),
    })))
}

/// Simplify trajectory using Douglas-Peucker.
#[derive(Deserialize)]
struct SimplifyRequest {
    #[serde(default = "default_tolerance")]
    tolerance: f64,
}
fn default_tolerance() -> f64 {
    0.0001
}

async fn simplify_trajectory(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SimplifyRequest>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let r = sqlx::query(
        "SELECT numInstants(trip) as before_count,
                numInstants(simplify(trip, $2)) as after_count
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .bind(req.tolerance)
    .fetch_optional(store.pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "points_before": r.get::<Option<i32>, _>("before_count"),
        "points_after": r.get::<Option<i32>, _>("after_count"),
        "tolerance": req.tolerance,
    })))
}

/// Find nearest approach between two trajectories.
#[derive(Deserialize)]
struct NearestApproachRequest {
    trajectory_a: Uuid,
    trajectory_b: Uuid,
}

async fn nearest_approach(
    State(store): State<AppState>,
    Path(_dataset_id): Path<Uuid>,
    Json(req): Json<NearestApproachRequest>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let r = sqlx::query(
        "SELECT nearestApproachDistance(a.trip, b.trip) as distance,
                nearestApproachInstant(a.trip, b.trip)::text as instant
         FROM trajectories a, trajectories b
         WHERE a.id = $1 AND b.id = $2",
    )
    .bind(req.trajectory_a)
    .bind(req.trajectory_b)
    .fetch_optional(store.pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "distance": r.get::<Option<f64>, _>("distance"),
        "instant": r.get::<Option<String>, _>("instant"),
    })))
}

enum TrajError {
    Db(sqlx::Error),
    NotFound,
}
impl From<sqlx::Error> for TrajError {
    fn from(e: sqlx::Error) -> Self {
        TrajError::Db(e)
    }
}
impl IntoResponse for TrajError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            TrajError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            TrajError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
