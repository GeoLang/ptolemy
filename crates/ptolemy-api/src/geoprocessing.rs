// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Geoprocessing API — PostGIS-native spatial analysis operations.
//!
//! Exposes common vector geoprocessing tools (clip, intersect, difference,
//! dissolve, spatial join, voronoi, convex hull, centroid, nearest neighbor,
//! distance matrix, contour, merge, split) as REST endpoints.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn geoprocessing_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/geoprocessing/clip", post(clip))
        .route("/branches/{id}/geoprocessing/intersect", post(intersect))
        .route("/branches/{id}/geoprocessing/difference", post(difference))
        .route("/branches/{id}/geoprocessing/dissolve", post(dissolve))
        .route(
            "/branches/{id}/geoprocessing/spatial-join",
            post(spatial_join),
        )
        .route("/branches/{id}/geoprocessing/voronoi", post(voronoi))
        .route(
            "/branches/{id}/geoprocessing/convex-hull",
            post(convex_hull),
        )
        .route("/branches/{id}/geoprocessing/centroid", post(centroid))
        .route(
            "/branches/{id}/geoprocessing/nearest-neighbor",
            post(nearest_neighbor),
        )
        .route(
            "/branches/{id}/geoprocessing/distance-matrix",
            post(distance_matrix),
        )
        .route("/branches/{id}/geoprocessing/contour", post(contour))
        .route("/branches/{id}/geoprocessing/merge", post(merge_features))
        .route("/branches/{id}/geoprocessing/split", post(split_features))
        .route("/branches/{id}/geoprocessing/simplify", post(simplify))
        .route("/branches/{id}/geoprocessing/densify", post(densify))
}

// ─── Common CTE for resolving live features on a branch ──────────────

/// SQL CTE that resolves the latest live features for a given branch.
const LIVE_FEATURES_CTE: &str = "
WITH RECURSIVE chain AS (
    SELECT c.id, c.parent_id FROM changesets c
    JOIN branches b ON b.head = c.id WHERE b.id = $1
  UNION ALL
    SELECT c.id, c.parent_id FROM changesets c
    JOIN chain ch ON ch.parent_id = c.id
),
latest AS (
    SELECT DISTINCT ON (fv.feature_id)
        fv.feature_id, fv.geometry, fv.properties, fv.operation
    FROM feature_versions fv
    JOIN chain ch ON fv.changeset_id = ch.id
    ORDER BY fv.feature_id, fv.created_at DESC
),
live AS (
    SELECT feature_id, geometry, properties
    FROM latest WHERE operation != 'delete' AND geometry IS NOT NULL
)";

// ─── Clip ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ClipRequest {
    /// GeoJSON polygon to clip features by.
    clip_geometry: serde_json::Value,
}

#[derive(Serialize)]
struct GeoJsonCollection {
    r#type: String,
    features: Vec<serde_json::Value>,
}

async fn clip(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ClipRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let clip_geojson = serde_json::to_string(&req.clip_geometry)
        .map_err(|_| GeoprocessingError::BadRequest("invalid clip geometry".into()))?;

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             feature_id,
             ST_AsGeoJSON(ST_Intersection(geometry, ST_GeomFromGeoJSON($2)))::jsonb as geojson
         FROM live
         WHERE ST_Intersects(geometry, ST_GeomFromGeoJSON($2))"
    ))
    .bind(branch_id)
    .bind(&clip_geojson)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {}
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Intersect ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct IntersectRequest {
    /// GeoJSON polygon to intersect with.
    overlay_geometry: serde_json::Value,
}

async fn intersect(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<IntersectRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let overlay = serde_json::to_string(&req.overlay_geometry)
        .map_err(|_| GeoprocessingError::BadRequest("invalid overlay geometry".into()))?;

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             feature_id,
             ST_AsGeoJSON(ST_Intersection(geometry, ST_GeomFromGeoJSON($2)))::jsonb as geojson,
             properties
         FROM live
         WHERE ST_Intersects(geometry, ST_GeomFromGeoJSON($2))
           AND NOT ST_IsEmpty(ST_Intersection(geometry, ST_GeomFromGeoJSON($2)))"
    ))
    .bind(branch_id)
    .bind(&overlay)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": r.get::<serde_json::Value, _>("properties")
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Difference ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DifferenceRequest {
    /// GeoJSON polygon to subtract from features.
    subtract_geometry: serde_json::Value,
}

async fn difference(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<DifferenceRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let subtract = serde_json::to_string(&req.subtract_geometry)
        .map_err(|_| GeoprocessingError::BadRequest("invalid subtract geometry".into()))?;

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             feature_id,
             ST_AsGeoJSON(ST_Difference(geometry, ST_GeomFromGeoJSON($2)))::jsonb as geojson
         FROM live
         WHERE ST_Intersects(geometry, ST_GeomFromGeoJSON($2))
           AND NOT ST_IsEmpty(ST_Difference(geometry, ST_GeomFromGeoJSON($2)))"
    ))
    .bind(branch_id)
    .bind(&subtract)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {}
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Dissolve ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DissolveRequest {
    /// Property key to dissolve/group by.
    group_by: String,
}

#[derive(Serialize)]
struct DissolveResult {
    groups: Vec<DissolveGroup>,
}

#[derive(Serialize)]
struct DissolveGroup {
    key: serde_json::Value,
    feature_count: i64,
    geometry: serde_json::Value,
    area_sq_meters: f64,
}

async fn dissolve(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<DissolveRequest>,
) -> Result<Json<DissolveResult>, GeoprocessingError> {
    // Validate group_by key (prevent SQL injection — only allow alphanumeric + underscore)
    if !req
        .group_by
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_')
    {
        return Err(GeoprocessingError::BadRequest(
            "group_by must be alphanumeric".into(),
        ));
    }

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             properties->>'{key}' as group_key,
             COUNT(*) as cnt,
             ST_AsGeoJSON(ST_Union(geometry))::jsonb as geojson,
             COALESCE(ST_Area(ST_Union(geometry::geography)), 0) as area
         FROM live
         WHERE properties->>'{key}' IS NOT NULL
         GROUP BY properties->>'{key}'",
        key = req.group_by
    ))
    .bind(branch_id)
    .fetch_all(store.pool())
    .await?;

    let groups = rows
        .iter()
        .map(|r| DissolveGroup {
            key: serde_json::Value::String(r.get::<String, _>("group_key")),
            feature_count: r.get("cnt"),
            geometry: r.get("geojson"),
            area_sq_meters: r.get("area"),
        })
        .collect();

    Ok(Json(DissolveResult { groups }))
}

// ─── Spatial Join ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct SpatialJoinRequest {
    /// Feature IDs to join attributes to.
    target_ids: Vec<Uuid>,
    /// Spatial predicate: "intersects", "contains", "within".
    predicate: String,
    /// Property keys to copy from matching features.
    copy_properties: Vec<String>,
}

#[derive(Serialize)]
struct SpatialJoinResult {
    joined: Vec<JoinedFeature>,
}

#[derive(Serialize)]
struct JoinedFeature {
    feature_id: Uuid,
    joined_from: Uuid,
    copied_properties: serde_json::Value,
}

async fn spatial_join(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SpatialJoinRequest>,
) -> Result<Json<SpatialJoinResult>, GeoprocessingError> {
    let predicate_fn = match req.predicate.as_str() {
        "intersects" => "ST_Intersects",
        "contains" => "ST_Contains",
        "within" => "ST_Within",
        "touches" => "ST_Touches",
        "crosses" => "ST_Crosses",
        _ => {
            return Err(GeoprocessingError::BadRequest(
                "predicate must be: intersects, contains, within, touches, crosses".into(),
            ));
        }
    };

    let target_ids: Vec<String> = req.target_ids.iter().map(|id| id.to_string()).collect();

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             t.feature_id as target_id,
             s.feature_id as source_id,
             s.properties as source_props
         FROM live t
         JOIN live s ON {predicate_fn}(t.geometry, s.geometry)
            AND t.feature_id != s.feature_id
         WHERE t.feature_id = ANY($2::uuid[])"
    ))
    .bind(branch_id)
    .bind(&target_ids)
    .fetch_all(store.pool())
    .await?;

    let joined = rows
        .iter()
        .map(|r| {
            let source_props: serde_json::Value = r.get("source_props");
            let copied: serde_json::Value = if req.copy_properties.is_empty() {
                source_props
            } else {
                let obj = source_props.as_object();
                let filtered: serde_json::Map<String, serde_json::Value> = req
                    .copy_properties
                    .iter()
                    .filter_map(|k| obj.and_then(|o| o.get(k).map(|v| (k.clone(), v.clone()))))
                    .collect();
                serde_json::Value::Object(filtered)
            };

            JoinedFeature {
                feature_id: r.get("target_id"),
                joined_from: r.get("source_id"),
                copied_properties: copied,
            }
        })
        .collect();

    Ok(Json(SpatialJoinResult { joined }))
}

// ─── Voronoi Polygons ───────────────────────────────────────────────

#[derive(Deserialize)]
struct VoronoiRequest {
    /// Optional bounding envelope as GeoJSON polygon. If omitted, uses extent of features.
    envelope: Option<serde_json::Value>,
    /// Tolerance for snapping (default 0.0).
    #[serde(default)]
    tolerance: f64,
}

async fn voronoi(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<VoronoiRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let envelope_clause = if let Some(env) = &req.envelope {
        let env_json = serde_json::to_string(env)
            .map_err(|_| GeoprocessingError::BadRequest("invalid envelope".into()))?;
        format!("ST_GeomFromGeoJSON('{env_json}')")
    } else {
        "ST_Envelope(ST_Collect(geometry))".to_string()
    };

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE},
         points AS (
             SELECT ST_Collect(ST_Centroid(geometry)) as geom FROM live
         )
         SELECT
             ST_AsGeoJSON(
                 (ST_Dump(ST_VoronoiPolygons(geom, $2, {envelope_clause}))).geom
             )::jsonb as geojson
         FROM points"
    ))
    .bind(branch_id)
    .bind(req.tolerance)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            serde_json::json!({
                "type": "Feature",
                "id": i,
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {}
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Convex Hull ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ConvexHullRequest {
    /// If provided, compute hull for these feature IDs only.
    feature_ids: Option<Vec<Uuid>>,
}

#[derive(Serialize)]
struct SingleGeometryResult {
    geometry: serde_json::Value,
    area_sq_meters: f64,
}

async fn convex_hull(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ConvexHullRequest>,
) -> Result<Json<SingleGeometryResult>, GeoprocessingError> {
    let filter = if let Some(ids) = &req.feature_ids {
        let id_list: Vec<String> = ids.iter().map(|id| format!("'{id}'")).collect();
        format!("AND feature_id IN ({})", id_list.join(","))
    } else {
        String::new()
    };

    let row = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             ST_AsGeoJSON(ST_ConvexHull(ST_Collect(geometry)))::jsonb as geojson,
             COALESCE(ST_Area(ST_ConvexHull(ST_Collect(geometry::geography))), 0) as area
         FROM live
         WHERE TRUE {filter}"
    ))
    .bind(branch_id)
    .fetch_one(store.pool())
    .await?;

    Ok(Json(SingleGeometryResult {
        geometry: row.get("geojson"),
        area_sq_meters: row.get("area"),
    }))
}

// ─── Centroid ───────────────────────────────────────────────────────

async fn centroid(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             feature_id,
             ST_AsGeoJSON(ST_Centroid(geometry))::jsonb as geojson,
             properties
         FROM live"
    ))
    .bind(branch_id)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": r.get::<serde_json::Value, _>("properties")
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Nearest Neighbor ───────────────────────────────────────────────

#[derive(Deserialize)]
struct NearestNeighborRequest {
    /// Feature ID to find neighbors for.
    feature_id: Uuid,
    /// Number of neighbors to return.
    #[serde(default = "default_k")]
    k: i64,
}

fn default_k() -> i64 {
    5
}

#[derive(Serialize)]
struct NearestResult {
    feature_id: Uuid,
    neighbors: Vec<Neighbor>,
}

#[derive(Serialize)]
struct Neighbor {
    feature_id: Uuid,
    distance_meters: f64,
    geometry: serde_json::Value,
}

async fn nearest_neighbor(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<NearestNeighborRequest>,
) -> Result<Json<NearestResult>, GeoprocessingError> {
    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE},
         target AS (
             SELECT geometry FROM live WHERE feature_id = $2
         )
         SELECT
             live.feature_id,
             ST_Distance(live.geometry::geography, target.geometry::geography) as dist,
             ST_AsGeoJSON(live.geometry)::jsonb as geojson
         FROM live, target
         WHERE live.feature_id != $2
         ORDER BY live.geometry <-> target.geometry
         LIMIT $3"
    ))
    .bind(branch_id)
    .bind(req.feature_id)
    .bind(req.k)
    .fetch_all(store.pool())
    .await?;

    let neighbors = rows
        .iter()
        .map(|r| Neighbor {
            feature_id: r.get("feature_id"),
            distance_meters: r.get("dist"),
            geometry: r.get("geojson"),
        })
        .collect();

    Ok(Json(NearestResult {
        feature_id: req.feature_id,
        neighbors,
    }))
}

// ─── Distance Matrix ────────────────────────────────────────────────

#[derive(Deserialize)]
struct DistanceMatrixRequest {
    /// Feature IDs to compute distances between.
    feature_ids: Vec<Uuid>,
}

#[derive(Serialize)]
struct DistanceMatrixResult {
    matrix: Vec<DistancePair>,
}

#[derive(Serialize)]
struct DistancePair {
    from: Uuid,
    to: Uuid,
    distance_meters: f64,
}

async fn distance_matrix(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<DistanceMatrixRequest>,
) -> Result<Json<DistanceMatrixResult>, GeoprocessingError> {
    let ids: Vec<String> = req.feature_ids.iter().map(|id| id.to_string()).collect();

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             a.feature_id as from_id,
             b.feature_id as to_id,
             ST_Distance(a.geometry::geography, b.geometry::geography) as dist
         FROM live a
         CROSS JOIN live b
         WHERE a.feature_id = ANY($2::uuid[])
           AND b.feature_id = ANY($2::uuid[])
           AND a.feature_id < b.feature_id"
    ))
    .bind(branch_id)
    .bind(&ids)
    .fetch_all(store.pool())
    .await?;

    let matrix = rows
        .iter()
        .map(|r| DistancePair {
            from: r.get("from_id"),
            to: r.get("to_id"),
            distance_meters: r.get("dist"),
        })
        .collect();

    Ok(Json(DistanceMatrixResult { matrix }))
}

// ─── Contour ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ContourRequest {
    /// Property name containing the elevation/value.
    value_property: String,
    /// Contour interval.
    interval: f64,
}

async fn contour(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ContourRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    if !req
        .value_property
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_')
    {
        return Err(GeoprocessingError::BadRequest(
            "value_property must be alphanumeric".into(),
        ));
    }

    // Use ST_ContourLines (PostGIS 3.4+) on an interpolated TIN surface
    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE},
         pts AS (
             SELECT
                 ST_Centroid(geometry) as geom,
                 (properties->>'{prop}')::double precision as val
             FROM live
             WHERE properties->>'{prop}' IS NOT NULL
         ),
         tin AS (
             SELECT ST_DelaunayTriangles(ST_Collect(geom)) as geom FROM pts
         )
         SELECT
             ST_AsGeoJSON(
                 (ST_Dump(ST_ContourLines(geom, $2))).geom
             )::jsonb as geojson,
             $2 * generate_series(1, 100) as level
         FROM tin
         LIMIT 1000",
        prop = req.value_property
    ))
    .bind(branch_id)
    .bind(req.interval)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            serde_json::json!({
                "type": "Feature",
                "id": i,
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {"level": r.get::<f64, _>("level")}
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Merge Features ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct MergeRequest {
    /// Feature IDs to merge into a single geometry.
    feature_ids: Vec<Uuid>,
}

async fn merge_features(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<SingleGeometryResult>, GeoprocessingError> {
    let ids: Vec<String> = req.feature_ids.iter().map(|id| id.to_string()).collect();

    let row = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             ST_AsGeoJSON(ST_Union(geometry))::jsonb as geojson,
             COALESCE(ST_Area(ST_Union(geometry::geography)), 0) as area
         FROM live
         WHERE feature_id = ANY($2::uuid[])"
    ))
    .bind(branch_id)
    .bind(&ids)
    .fetch_one(store.pool())
    .await?;

    Ok(Json(SingleGeometryResult {
        geometry: row.get("geojson"),
        area_sq_meters: row.get("area"),
    }))
}

// ─── Split Features ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct SplitRequest {
    /// Feature ID to split.
    feature_id: Uuid,
    /// GeoJSON line to split the feature by.
    split_line: serde_json::Value,
}

async fn split_features(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SplitRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let line = serde_json::to_string(&req.split_line)
        .map_err(|_| GeoprocessingError::BadRequest("invalid split line".into()))?;

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             ST_AsGeoJSON(
                 (ST_Dump(ST_Split(geometry, ST_GeomFromGeoJSON($3)))).geom
             )::jsonb as geojson
         FROM live
         WHERE feature_id = $2"
    ))
    .bind(branch_id)
    .bind(req.feature_id)
    .bind(&line)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            serde_json::json!({
                "type": "Feature",
                "id": i,
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {}
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Simplify ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SimplifyRequest {
    /// Tolerance in map units (degrees for 4326, meters for projected).
    tolerance: f64,
    /// If true, preserve topology (ST_SimplifyPreserveTopology).
    #[serde(default)]
    preserve_topology: bool,
}

async fn simplify(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SimplifyRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let fn_name = if req.preserve_topology {
        "ST_SimplifyPreserveTopology"
    } else {
        "ST_Simplify"
    };

    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             feature_id,
             ST_AsGeoJSON({fn_name}(geometry, $2))::jsonb as geojson,
             ST_NPoints(geometry) as pts_before,
             ST_NPoints({fn_name}(geometry, $2)) as pts_after
         FROM live"
    ))
    .bind(branch_id)
    .bind(req.tolerance)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {
                    "points_before": r.get::<i32, _>("pts_before"),
                    "points_after": r.get::<i32, _>("pts_after")
                }
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Densify ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DensifyRequest {
    /// Maximum segment length.
    max_segment_length: f64,
}

async fn densify(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<DensifyRequest>,
) -> Result<Json<GeoJsonCollection>, GeoprocessingError> {
    let rows = sqlx::query(&format!(
        "{LIVE_FEATURES_CTE}
         SELECT
             feature_id,
             ST_AsGeoJSON(ST_Segmentize(geometry, $2))::jsonb as geojson
         FROM live"
    ))
    .bind(branch_id)
    .bind(req.max_segment_length)
    .fetch_all(store.pool())
    .await?;

    let features = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "id": r.get::<Uuid, _>("feature_id").to_string(),
                "geometry": r.get::<serde_json::Value, _>("geojson"),
                "properties": {}
            })
        })
        .collect();

    Ok(Json(GeoJsonCollection {
        r#type: "FeatureCollection".to_string(),
        features,
    }))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum GeoprocessingError {
    Store(sqlx::Error),
    BadRequest(String),
}

impl From<sqlx::Error> for GeoprocessingError {
    fn from(e: sqlx::Error) -> Self {
        GeoprocessingError::Store(e)
    }
}

impl IntoResponse for GeoprocessingError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            GeoprocessingError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            GeoprocessingError::Store(e) => {
                tracing::error!("Geoprocessing DB error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
