// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Spatial analytics and anomaly detection API endpoints.

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

pub fn analytics_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/analytics/buffer", get(buffer_analysis))
        .route("/branches/{id}/analytics/union", get(union_analysis))
        .route("/branches/{id}/analytics/clusters", get(cluster_analysis))
        .route("/branches/{id}/analytics/anomalies", get(anomaly_detection))
        .route("/branches/{id}/analytics/stats", get(spatial_stats))
}

// ─── Buffer Analysis ────────────────────────────────────────────────

#[derive(Deserialize)]
struct BufferQuery {
    feature_id: Uuid,
    distance: f64,
}

#[derive(Serialize)]
struct BufferResult {
    feature_id: Uuid,
    buffer_geojson: serde_json::Value,
    area_sq_meters: f64,
}

async fn buffer_analysis(
    State(store): State<AppState>,
    Path(_branch_id): Path<Uuid>,
    Query(q): Query<BufferQuery>,
) -> Result<Json<BufferResult>, AnalyticsError> {
    let row = sqlx::query(
        "SELECT
            ST_AsGeoJSON(ST_Buffer(geometry::geography, $2)::geometry)::jsonb as geojson,
            ST_Area(ST_Buffer(geometry::geography, $2)) as area
         FROM feature_versions
         WHERE feature_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(q.feature_id)
    .bind(q.distance)
    .fetch_optional(store.pool())
    .await?
    .ok_or_else(|| AnalyticsError::NotFound("feature not found".into()))?;

    Ok(Json(BufferResult {
        feature_id: q.feature_id,
        buffer_geojson: row.get("geojson"),
        area_sq_meters: row.get("area"),
    }))
}

// ─── Union Analysis ─────────────────────────────────────────────────

#[derive(Serialize)]
struct UnionResult {
    feature_count: i64,
    union_geojson: serde_json::Value,
    total_area_sq_meters: f64,
}

async fn union_analysis(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<UnionResult>, AnalyticsError> {
    let row = sqlx::query(
        "WITH RECURSIVE chain AS (
            SELECT c.id, c.parent_id FROM changesets c
            JOIN branches b ON b.head = c.id WHERE b.id = $1
          UNION ALL
            SELECT c.id, c.parent_id FROM changesets c
            JOIN chain ch ON ch.parent_id = c.id
        ),
        latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.geometry, fv.operation
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            ORDER BY fv.feature_id, fv.created_at DESC
        ),
        live AS (
            SELECT geometry FROM latest WHERE operation != 'delete' AND geometry IS NOT NULL
        )
        SELECT
            COUNT(*) as cnt,
            ST_AsGeoJSON(ST_Union(geometry))::jsonb as geojson,
            COALESCE(ST_Area(ST_Union(geometry::geography)), 0) as area
        FROM live",
    )
    .bind(branch_id)
    .fetch_one(store.pool())
    .await?;

    Ok(Json(UnionResult {
        feature_count: row.get("cnt"),
        union_geojson: row.get("geojson"),
        total_area_sq_meters: row.get("area"),
    }))
}

// ─── Cluster Analysis ───────────────────────────────────────────────

#[derive(Deserialize)]
struct ClusterQuery {
    #[serde(default = "default_eps")]
    eps: f64,
    #[serde(default = "default_min_points")]
    min_points: i32,
}

fn default_eps() -> f64 {
    0.001
}
fn default_min_points() -> i32 {
    3
}

#[derive(Serialize)]
struct Cluster {
    cluster_id: i32,
    feature_count: i64,
    centroid_geojson: serde_json::Value,
}

async fn cluster_analysis(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<ClusterQuery>,
) -> Result<Json<Vec<Cluster>>, AnalyticsError> {
    let rows = sqlx::query(
        "WITH RECURSIVE chain AS (
            SELECT c.id, c.parent_id FROM changesets c
            JOIN branches b ON b.head = c.id WHERE b.id = $1
          UNION ALL
            SELECT c.id, c.parent_id FROM changesets c
            JOIN chain ch ON ch.parent_id = c.id
        ),
        latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.geometry, fv.operation
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            ORDER BY fv.feature_id, fv.created_at DESC
        ),
        live AS (
            SELECT geometry FROM latest WHERE operation != 'delete' AND geometry IS NOT NULL
        ),
        clustered AS (
            SELECT ST_ClusterDBSCAN(geometry, $2, $3) OVER () as cid, geometry
            FROM live
        )
        SELECT
            cid as cluster_id,
            COUNT(*) as cnt,
            ST_AsGeoJSON(ST_Centroid(ST_Collect(geometry)))::jsonb as centroid
        FROM clustered
        WHERE cid IS NOT NULL
        GROUP BY cid
        ORDER BY cid",
    )
    .bind(branch_id)
    .bind(q.eps)
    .bind(q.min_points)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| Cluster {
                cluster_id: row.get("cluster_id"),
                feature_count: row.get("cnt"),
                centroid_geojson: row.get("centroid"),
            })
            .collect(),
    ))
}

// ─── Anomaly Detection ──────────────────────────────────────────────

#[derive(Serialize)]
struct Anomaly {
    feature_id: Uuid,
    anomaly_type: String,
    description: String,
}

async fn anomaly_detection(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<Vec<Anomaly>>, AnalyticsError> {
    // Detect: features far from centroid (spatial outliers) and self-intersecting
    let rows = sqlx::query(
        "WITH RECURSIVE chain AS (
            SELECT c.id, c.parent_id FROM changesets c
            JOIN branches b ON b.head = c.id WHERE b.id = $1
          UNION ALL
            SELECT c.id, c.parent_id FROM changesets c
            JOIN chain ch ON ch.parent_id = c.id
        ),
        latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id, fv.geometry, fv.operation
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            ORDER BY fv.feature_id, fv.created_at DESC
        ),
        live AS (
            SELECT feature_id, geometry FROM latest
            WHERE operation != 'delete' AND geometry IS NOT NULL
        ),
        stats AS (
            SELECT
                ST_Centroid(ST_Collect(geometry)) as center,
                COALESCE(STDDEV(ST_Distance(geometry::geography, ST_Centroid(ST_Collect(geometry))::geography)), 1) as stddev_dist
            FROM live
        )
        SELECT feature_id, 'spatial_outlier' as atype,
            'Feature is >3 std deviations from dataset centroid' as descr
        FROM live, stats
        WHERE ST_Distance(live.geometry::geography, stats.center::geography) > stats.stddev_dist * 3
        UNION ALL
        SELECT feature_id, 'self_intersection' as atype,
            'Geometry contains self-intersections' as descr
        FROM live
        WHERE NOT ST_IsSimple(geometry)",
    )
    .bind(branch_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| Anomaly {
                feature_id: row.get("feature_id"),
                anomaly_type: row.get("atype"),
                description: row.get("descr"),
            })
            .collect(),
    ))
}

// ─── Spatial Statistics ─────────────────────────────────────────────

#[derive(Serialize)]
struct SpatialStats {
    feature_count: i64,
    total_area_sq_meters: f64,
    total_length_meters: f64,
    bbox: Option<serde_json::Value>,
    centroid: Option<serde_json::Value>,
}

async fn spatial_stats(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
) -> Result<Json<SpatialStats>, AnalyticsError> {
    let row = sqlx::query(
        "WITH RECURSIVE chain AS (
            SELECT c.id, c.parent_id FROM changesets c
            JOIN branches b ON b.head = c.id WHERE b.id = $1
          UNION ALL
            SELECT c.id, c.parent_id FROM changesets c
            JOIN chain ch ON ch.parent_id = c.id
        ),
        latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.geometry, fv.operation
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            ORDER BY fv.feature_id, fv.created_at DESC
        ),
        live AS (
            SELECT geometry FROM latest WHERE operation != 'delete' AND geometry IS NOT NULL
        )
        SELECT
            COUNT(*) as cnt,
            COALESCE(SUM(ST_Area(geometry::geography)), 0) as total_area,
            COALESCE(SUM(ST_Length(geometry::geography)), 0) as total_length,
            ST_AsGeoJSON(ST_Extent(geometry))::jsonb as bbox,
            ST_AsGeoJSON(ST_Centroid(ST_Collect(geometry)))::jsonb as centroid
        FROM live",
    )
    .bind(branch_id)
    .fetch_one(store.pool())
    .await?;

    Ok(Json(SpatialStats {
        feature_count: row.get("cnt"),
        total_area_sq_meters: row.get("total_area"),
        total_length_meters: row.get("total_length"),
        bbox: row.get("bbox"),
        centroid: row.get("centroid"),
    }))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum AnalyticsError {
    Store(sqlx::Error),
    NotFound(String),
}

impl From<sqlx::Error> for AnalyticsError {
    fn from(e: sqlx::Error) -> Self {
        AnalyticsError::Store(e)
    }
}

impl IntoResponse for AnalyticsError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            AnalyticsError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AnalyticsError::Store(e) => {
                tracing::error!("Database error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
