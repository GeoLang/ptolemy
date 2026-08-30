// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! MobilityDB trajectories — moving objects and temporal geometry.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_storage::{WriteGrant, writes::TrajectoryTrip};
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

/// Migration 015 only gives `trajectories` the MobilityDB column types where the
/// extension is installed; without it `trip` is JSONB and `period` a tstzrange,
/// and none of the MobilityDB functions parse.
async fn has_mobilitydb(store: &AppState) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'mobilitydb')")
        .fetch_one(store.read_pool())
        .await
}

/// The analytics routes below have no JSONB form: they call MobilityDB
/// functions on `trip` and nothing else answers the question. Without the
/// extension the call is not a failure to hide behind a 500, it is a route this
/// deployment does not have.
async fn require_mobilitydb(store: &AppState) -> Result<(), TrajError> {
    if has_mobilitydb(store).await? {
        Ok(())
    } else {
        Err(TrajError::NoMobilityDb)
    }
}

#[derive(Serialize)]
struct Trajectory {
    id: Uuid,
    name: String,
    start_time: Option<String>,
    end_time: Option<String>,
}

async fn list_trajectories(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Trajectory>>, TrajError> {
    let rows = sqlx::query(
        "SELECT id, name,
                lower(period)::text as start_time,
                upper(period)::text as end_time
         FROM trajectories WHERE dataset_id = $1 ORDER BY period",
    )
    .bind(dataset_id)
    .fetch_all(store.read_pool())
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Trajectory {
                id: r.get("id"),
                name: r.get("name"),
                start_time: r.get("start_time"),
                end_time: r.get("end_time"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTrajectoryRequest {
    name: String,
    /// Array of [lng, lat, timestamp_iso] points
    points: Vec<TrajectoryPoint>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrajectoryPoint {
    lng: f64,
    lat: f64,
    timestamp: String,
}

async fn create_trajectory(
    State(store): State<AppState>,
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<CreateTrajectoryRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), TrajError> {
    let id = if has_mobilitydb(&store).await? {
        // Build MobilityDB tgeompoint from points
        let instants: Vec<String> = req
            .points
            .iter()
            .map(|p| format!("POINT({} {})@{}", p.lng, p.lat, p.timestamp))
            .collect();
        let tgeompoint_str = format!("[{}]", instants.join(", "));
        store
            .create_trajectory(
                &grant,
                &req.name,
                &TrajectoryTrip::MobilityDb(&tgeompoint_str),
            )
            .await?
    } else {
        let trip = serde_json::to_value(&req.points).unwrap_or_default();
        store
            .create_trajectory(&grant, &req.name, &TrajectoryTrip::Jsonb(&trip))
            .await?
    };

    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn get_trajectory(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, TrajError> {
    let sql = if has_mobilitydb(&store).await? {
        "SELECT id, name,
                lower(period)::text as start_time,
                upper(period)::text as end_time,
                ST_AsGeoJSON(trajectory(trip))::jsonb as path_geojson,
                numInstants(trip) as num_points
         FROM trajectories WHERE id = $1"
    } else {
        "SELECT id, name,
                lower(period)::text as start_time,
                upper(period)::text as end_time,
                ST_AsGeoJSON(ST_MakeLine(ARRAY(
                    SELECT ST_SetSRID(ST_MakePoint((e->>'lng')::float8, (e->>'lat')::float8), 4326)
                    FROM jsonb_array_elements(trip) e
                )))::jsonb as path_geojson,
                jsonb_array_length(trip) as num_points
         FROM trajectories WHERE id = $1"
    };
    let r = sqlx::query(sql)
        .bind(id)
        .fetch_optional(store.read_pool())
        .await?
        .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "id": r.get::<Uuid, _>("id"),
        "name": r.get::<String, _>("name"),
        "start_time": r.get::<Option<String>, _>("start_time"),
        "end_time": r.get::<Option<String>, _>("end_time"),
        "path": r.get::<Option<serde_json::Value>, _>("path_geojson"),
        "num_points": r.get::<Option<i32>, _>("num_points"),
    })))
}

/// Get position at a specific time.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionAtQuery {
    timestamp: String,
}

async fn position_at_time(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<PositionAtQuery>,
) -> Result<Json<serde_json::Value>, TrajError> {
    require_mobilitydb(&store).await?;
    let r = sqlx::query(
        "SELECT ST_AsGeoJSON(valueAtTimestamp(trip, $2::timestamptz))::jsonb as position
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .bind(&q.timestamp)
    .fetch_optional(store.read_pool())
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
    require_mobilitydb(&store).await?;
    let r = sqlx::query(
        "SELECT twAvg(speed(trip)) as avg_speed,
                maxValue(speed(trip)) as max_speed,
                length(trip)::float as total_distance
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.read_pool())
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
    require_mobilitydb(&store).await?;
    let r = sqlx::query(
        "SELECT length(trip)::float as distance,
                duration(period)::text as duration
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "distance_meters": r.get::<Option<f64>, _>("distance"),
        "duration": r.get::<Option<String>, _>("duration"),
    })))
}

/// Simplify trajectory using Douglas-Peucker.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    require_mobilitydb(&store).await?;
    let r = sqlx::query(
        "SELECT numInstants(trip) as before_count,
                numInstants(simplify(trip, $2)) as after_count
         FROM trajectories WHERE id = $1",
    )
    .bind(id)
    .bind(req.tolerance)
    .fetch_optional(store.read_pool())
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
#[serde(deny_unknown_fields)]
struct NearestApproachRequest {
    trajectory_a: Uuid,
    trajectory_b: Uuid,
}

async fn nearest_approach(
    State(store): State<AppState>,
    Path(_dataset_id): Path<Uuid>,
    Json(req): Json<NearestApproachRequest>,
) -> Result<Json<serde_json::Value>, TrajError> {
    require_mobilitydb(&store).await?;
    let r = sqlx::query(
        "SELECT nearestApproachDistance(a.trip, b.trip) as distance,
                nearestApproachInstant(a.trip, b.trip)::text as instant
         FROM trajectories a, trajectories b
         WHERE a.id = $1 AND b.id = $2",
    )
    .bind(req.trajectory_a)
    .bind(req.trajectory_b)
    .fetch_optional(store.read_pool())
    .await?
    .ok_or(TrajError::NotFound)?;

    Ok(Json(serde_json::json!({
        "distance": r.get::<Option<f64>, _>("distance"),
        "instant": r.get::<Option<String>, _>("instant"),
    })))
}

enum TrajError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
    NotFound,
    NoMobilityDb,
}
impl From<sqlx::Error> for TrajError {
    fn from(e: sqlx::Error) -> Self {
        TrajError::Db(e)
    }
}
impl From<ptolemy_storage::StoreError> for TrajError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        TrajError::Store(e)
    }
}
impl IntoResponse for TrajError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            TrajError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            TrajError::NoMobilityDb => (
                StatusCode::NOT_IMPLEMENTED,
                "trajectory analytics need the MobilityDB extension, which this database does not have".to_string(),
            ),
            TrajError::Store(e) => crate::errors::store_error_status(&e),
            TrajError::Db(e) => {
                crate::errors::log_db_error("trajectory", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
