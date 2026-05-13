// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Geometric network and utility network API — graph tracing and analysis.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn network_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/networks",
            get(list_networks).post(create_network),
        )
        .route("/networks/{id}", get(get_network))
        .route(
            "/networks/{id}/junctions",
            get(list_junctions).post(add_junction),
        )
        .route("/networks/{id}/edges", get(list_edges).post(add_edge))
        .route("/networks/{id}/trace", post(trace_network))
        .route("/networks/{id}/shortest-path", post(shortest_path))
        .route("/networks/{id}/astar", post(astar_path))
        .route("/networks/{id}/isochrone", post(driving_distance))
        .route("/networks/{id}/tsp", post(tsp_tour))
        .route("/networks/{id}/connectivity", get(check_connectivity))
}

#[derive(Serialize)]
struct Network {
    id: Uuid,
    dataset_id: Uuid,
    name: String,
    network_type: String,
}

#[derive(Serialize)]
struct Junction {
    id: Uuid,
    feature_id: Option<Uuid>,
    geometry: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct Edge {
    id: Uuid,
    feature_id: Uuid,
    from_junction: Option<Uuid>,
    to_junction: Option<Uuid>,
    cost: f64,
    enabled: bool,
}

async fn list_networks(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Vec<Network>>, NetworkError> {
    let rows = sqlx::query(
        "SELECT id, dataset_id, name, network_type FROM networks WHERE dataset_id = $1",
    )
    .bind(dataset_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| Network {
                id: r.get("id"),
                dataset_id: r.get("dataset_id"),
                name: r.get("name"),
                network_type: r.get("network_type"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateNetworkRequest {
    name: String,
    #[serde(default = "default_network_type")]
    network_type: String,
}

fn default_network_type() -> String {
    "geometric".into()
}

async fn create_network(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Json(req): Json<CreateNetworkRequest>,
) -> Result<(StatusCode, Json<Network>), NetworkError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO networks (id, dataset_id, name, network_type) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(dataset_id)
    .bind(&req.name)
    .bind(&req.network_type)
    .execute(store.pool())
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(Network {
            id,
            dataset_id,
            name: req.name,
            network_type: req.network_type,
        }),
    ))
}

async fn get_network(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Network>, NetworkError> {
    let r = sqlx::query("SELECT id, dataset_id, name, network_type FROM networks WHERE id = $1")
        .bind(id)
        .fetch_optional(store.pool())
        .await?
        .ok_or(NetworkError::NotFound)?;
    Ok(Json(Network {
        id: r.get("id"),
        dataset_id: r.get("dataset_id"),
        name: r.get("name"),
        network_type: r.get("network_type"),
    }))
}

async fn list_junctions(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
) -> Result<Json<Vec<Junction>>, NetworkError> {
    let rows = sqlx::query(
        "SELECT id, feature_id, ST_AsGeoJSON(geometry)::jsonb as geojson FROM network_junctions WHERE network_id = $1",
    ).bind(network_id).fetch_all(store.pool()).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Junction {
                id: r.get("id"),
                feature_id: r.get("feature_id"),
                geometry: r.get("geojson"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct AddJunctionRequest {
    feature_id: Option<Uuid>,
    lng: f64,
    lat: f64,
}

async fn add_junction(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<AddJunctionRequest>,
) -> Result<(StatusCode, Json<Junction>), NetworkError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO network_junctions (id, network_id, feature_id, geometry)
         VALUES ($1, $2, $3, ST_SetSRID(ST_MakePoint($4, $5), 4326))",
    )
    .bind(id)
    .bind(network_id)
    .bind(req.feature_id)
    .bind(req.lng)
    .bind(req.lat)
    .execute(store.pool())
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(Junction {
            id,
            feature_id: req.feature_id,
            geometry: None,
        }),
    ))
}

async fn list_edges(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
) -> Result<Json<Vec<Edge>>, NetworkError> {
    let rows = sqlx::query(
        "SELECT id, feature_id, from_junction, to_junction, cost, enabled FROM network_edges WHERE network_id = $1",
    ).bind(network_id).fetch_all(store.pool()).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Edge {
                id: r.get("id"),
                feature_id: r.get("feature_id"),
                from_junction: r.get("from_junction"),
                to_junction: r.get("to_junction"),
                cost: r.get("cost"),
                enabled: r.get("enabled"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct AddEdgeRequest {
    feature_id: Uuid,
    from_junction: Option<Uuid>,
    to_junction: Option<Uuid>,
    #[serde(default = "default_cost")]
    cost: f64,
}

fn default_cost() -> f64 {
    1.0
}

async fn add_edge(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<AddEdgeRequest>,
) -> Result<StatusCode, NetworkError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO network_edges (id, network_id, feature_id, from_junction, to_junction, cost)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(network_id)
    .bind(req.feature_id)
    .bind(req.from_junction)
    .bind(req.to_junction)
    .bind(req.cost)
    .execute(store.pool())
    .await?;
    Ok(StatusCode::CREATED)
}

// ─── Network Analysis ───────────────────────────────────────────────

#[derive(Deserialize)]
struct TraceRequest {
    start_junction: Uuid,
    /// Max hops (default unlimited)
    max_depth: Option<i32>,
    /// Trace direction: upstream, downstream, both
    #[serde(default = "default_direction")]
    direction: String,
}

fn default_direction() -> String {
    "both".into()
}

#[derive(Serialize)]
struct TraceResult {
    junctions_reached: Vec<Uuid>,
    edges_traversed: Vec<Uuid>,
    total_cost: f64,
}

async fn trace_network(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<TraceRequest>,
) -> Result<Json<TraceResult>, NetworkError> {
    let max_depth = req.max_depth.unwrap_or(1000);

    let rows = match req.direction.as_str() {
        "downstream" => {
            sqlx::query(
                "WITH RECURSIVE trace AS (
                    SELECT to_junction as junction, id as edge_id, cost, 1 as depth
                    FROM network_edges
                    WHERE network_id = $1 AND from_junction = $2 AND enabled = TRUE
                  UNION ALL
                    SELECT e.to_junction, e.id, t.cost + e.cost, t.depth + 1
                    FROM network_edges e
                    JOIN trace t ON e.from_junction = t.junction
                    WHERE e.network_id = $1 AND e.enabled = TRUE AND t.depth < $3
                )
                SELECT junction, edge_id, cost FROM trace",
            ).bind(network_id).bind(req.start_junction).bind(max_depth)
            .fetch_all(store.pool()).await?
        }
        _ => {
            sqlx::query(
                "WITH RECURSIVE trace AS (
                    SELECT CASE WHEN from_junction = $2 THEN to_junction ELSE from_junction END as junction,
                           id as edge_id, cost, 1 as depth
                    FROM network_edges
                    WHERE network_id = $1 AND (from_junction = $2 OR to_junction = $2) AND enabled = TRUE
                  UNION ALL
                    SELECT CASE WHEN e.from_junction = t.junction THEN e.to_junction ELSE e.from_junction END,
                           e.id, t.cost + e.cost, t.depth + 1
                    FROM network_edges e
                    JOIN trace t ON (e.from_junction = t.junction OR e.to_junction = t.junction)
                    WHERE e.network_id = $1 AND e.enabled = TRUE AND t.depth < $3
                      AND e.id != t.edge_id
                )
                SELECT DISTINCT junction, edge_id, cost FROM trace",
            ).bind(network_id).bind(req.start_junction).bind(max_depth)
            .fetch_all(store.pool()).await?
        }
    };

    let mut junctions = Vec::new();
    let mut edges = Vec::new();
    let mut total_cost = 0.0f64;
    for row in &rows {
        let j: Uuid = row.get("junction");
        let e: Uuid = row.get("edge_id");
        let c: f64 = row.get("cost");
        if !junctions.contains(&j) {
            junctions.push(j);
        }
        if !edges.contains(&e) {
            edges.push(e);
        }
        if c > total_cost {
            total_cost = c;
        }
    }

    Ok(Json(TraceResult {
        junctions_reached: junctions,
        edges_traversed: edges,
        total_cost,
    }))
}

#[derive(Deserialize)]
struct ShortestPathRequest {
    from_junction: Uuid,
    to_junction: Uuid,
}

#[derive(Serialize)]
struct PathResult {
    found: bool,
    path_junctions: Vec<Uuid>,
    path_edges: Vec<Uuid>,
    total_cost: f64,
}

async fn shortest_path(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<ShortestPathRequest>,
) -> Result<Json<PathResult>, NetworkError> {
    // Use pgRouting pgr_dijkstra for optimal performance
    let rows = sqlx::query(
        "SELECT seq, node, edge, cost, agg_cost
         FROM pgr_dijkstra(
             'SELECT e.id, e.from_junction::bigint AS source, e.to_junction::bigint AS target,
                     e.cost, e.cost AS reverse_cost
              FROM network_edges e WHERE e.network_id = ''' || $1::text || ''' AND e.enabled = TRUE',
             $2::bigint, $3::bigint, directed := false
         )",
    )
    .bind(network_id)
    .bind(req.from_junction)
    .bind(req.to_junction)
    .fetch_all(store.pool())
    .await?;

    if rows.is_empty() {
        return Ok(Json(PathResult {
            found: false,
            path_junctions: vec![],
            path_edges: vec![],
            total_cost: 0.0,
        }));
    }

    let mut path_junctions = Vec::new();
    let mut path_edges = Vec::new();
    let mut total_cost = 0.0f64;
    for row in &rows {
        let node: i64 = row.get("node");
        let edge: i64 = row.get("edge");
        let agg: f64 = row.get("agg_cost");
        // Convert bigint back to UUID via lookup
        path_junctions.push(Uuid::from_u128(node as u128));
        if edge >= 0 {
            path_edges.push(Uuid::from_u128(edge as u128));
        }
        if agg > total_cost {
            total_cost = agg;
        }
    }

    Ok(Json(PathResult {
        found: true,
        path_junctions,
        path_edges,
        total_cost,
    }))
}

// ─── A* (heuristic shortest path) ──────────────────────────────────

#[derive(Deserialize)]
struct AstarRequest {
    from_junction: Uuid,
    to_junction: Uuid,
}

async fn astar_path(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<AstarRequest>,
) -> Result<Json<PathResult>, NetworkError> {
    let rows = sqlx::query(
        "SELECT seq, node, edge, cost, agg_cost
         FROM pgr_astar(
             'SELECT e.id, e.from_junction::bigint AS source, e.to_junction::bigint AS target,
                     e.cost, e.cost AS reverse_cost,
                     ST_X(j1.geometry) AS x1, ST_Y(j1.geometry) AS y1,
                     ST_X(j2.geometry) AS x2, ST_Y(j2.geometry) AS y2
              FROM network_edges e
              JOIN network_junctions j1 ON j1.id = e.from_junction
              JOIN network_junctions j2 ON j2.id = e.to_junction
              WHERE e.network_id = ''' || $1::text || ''' AND e.enabled = TRUE',
             $2::bigint, $3::bigint, directed := false
         )",
    )
    .bind(network_id)
    .bind(req.from_junction)
    .bind(req.to_junction)
    .fetch_all(store.pool())
    .await?;

    if rows.is_empty() {
        return Ok(Json(PathResult {
            found: false,
            path_junctions: vec![],
            path_edges: vec![],
            total_cost: 0.0,
        }));
    }

    let mut path_junctions = Vec::new();
    let mut path_edges = Vec::new();
    let mut total_cost = 0.0f64;
    for row in &rows {
        let node: i64 = row.get("node");
        let edge: i64 = row.get("edge");
        let agg: f64 = row.get("agg_cost");
        path_junctions.push(Uuid::from_u128(node as u128));
        if edge >= 0 {
            path_edges.push(Uuid::from_u128(edge as u128));
        }
        if agg > total_cost {
            total_cost = agg;
        }
    }

    Ok(Json(PathResult {
        found: true,
        path_junctions,
        path_edges,
        total_cost,
    }))
}

// ─── Driving Distance / Isochrone ───────────────────────────────────

#[derive(Deserialize)]
struct DrivingDistanceRequest {
    start_junction: Uuid,
    max_cost: f64,
}

#[derive(Serialize)]
struct IsochroneResult {
    reachable_nodes: Vec<IsochroneNode>,
}

#[derive(Serialize)]
struct IsochroneNode {
    node: i64,
    edge: i64,
    cost: f64,
    agg_cost: f64,
}

async fn driving_distance(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<DrivingDistanceRequest>,
) -> Result<Json<IsochroneResult>, NetworkError> {
    let rows = sqlx::query(
        "SELECT seq, node, edge, cost, agg_cost
         FROM pgr_drivingDistance(
             'SELECT e.id, e.from_junction::bigint AS source, e.to_junction::bigint AS target,
                     e.cost, e.cost AS reverse_cost
              FROM network_edges e WHERE e.network_id = ''' || $1::text || ''' AND e.enabled = TRUE',
             $2::bigint, $3, directed := false
         )",
    )
    .bind(network_id)
    .bind(req.start_junction)
    .bind(req.max_cost)
    .fetch_all(store.pool())
    .await?;

    let nodes: Vec<IsochroneNode> = rows
        .iter()
        .map(|r| IsochroneNode {
            node: r.get("node"),
            edge: r.get("edge"),
            cost: r.get("cost"),
            agg_cost: r.get("agg_cost"),
        })
        .collect();

    Ok(Json(IsochroneResult {
        reachable_nodes: nodes,
    }))
}

// ─── TSP (Traveling Salesman Problem) ───────────────────────────────

#[derive(Deserialize)]
struct TspRequest {
    junction_ids: Vec<Uuid>,
    start_junction: Option<Uuid>,
}

#[derive(Serialize)]
struct TspResult {
    ordered_nodes: Vec<i64>,
    total_cost: f64,
}

async fn tsp_tour(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<TspRequest>,
) -> Result<Json<TspResult>, NetworkError> {
    // First compute cost matrix, then run TSP
    let start = req.start_junction.map(|u| u.as_u128() as i64).unwrap_or(0);
    let ids: Vec<i64> = req
        .junction_ids
        .iter()
        .map(|u| u.as_u128() as i64)
        .collect();

    let rows = sqlx::query(
        "SELECT seq, node, cost, agg_cost
         FROM pgr_TSP(
             $$SELECT * FROM pgr_dijkstraCostMatrix(
                 'SELECT e.id, e.from_junction::bigint AS source, e.to_junction::bigint AS target,
                         e.cost, e.cost AS reverse_cost
                  FROM network_edges e WHERE e.network_id = $4 AND e.enabled = TRUE',
                 $1::bigint[], directed := false
             )$$,
             start_id := $2
         )",
    )
    .bind(&ids)
    .bind(start)
    .bind(network_id)
    .fetch_all(store.pool())
    .await?;

    let mut ordered = Vec::new();
    let mut total = 0.0f64;
    for row in &rows {
        ordered.push(row.get::<i64, _>("node"));
        let agg: f64 = row.get("agg_cost");
        if agg > total {
            total = agg;
        }
    }

    Ok(Json(TspResult {
        ordered_nodes: ordered,
        total_cost: total,
    }))
}

#[derive(Serialize)]
struct ConnectivityReport {
    total_junctions: i64,
    total_edges: i64,
    connected_components: i64,
    isolated_junctions: Vec<Uuid>,
}

async fn check_connectivity(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
) -> Result<Json<ConnectivityReport>, NetworkError> {
    let stats = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM network_junctions WHERE network_id = $1) as junctions,
            (SELECT COUNT(*) FROM network_edges WHERE network_id = $1) as edges",
    )
    .bind(network_id)
    .fetch_one(store.pool())
    .await?;

    // Use pgRouting connectedComponents for accurate component count
    let components = sqlx::query(
        "SELECT COUNT(DISTINCT component) as num_components
         FROM pgr_connectedComponents(
             'SELECT e.id, e.from_junction::bigint AS source, e.to_junction::bigint AS target,
                     e.cost FROM network_edges e
              WHERE e.network_id = ''' || $1::text || ''' AND e.enabled = TRUE'
         )",
    )
    .bind(network_id)
    .fetch_optional(store.pool())
    .await?;

    let num_components = components
        .map(|r| r.get::<i64, _>("num_components"))
        .unwrap_or(0);

    let isolated = sqlx::query(
        "SELECT j.id FROM network_junctions j
         WHERE j.network_id = $1
           AND NOT EXISTS (
             SELECT 1 FROM network_edges e
             WHERE e.network_id = $1
               AND (e.from_junction = j.id OR e.to_junction = j.id)
           )",
    )
    .bind(network_id)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(ConnectivityReport {
        total_junctions: stats.get("junctions"),
        total_edges: stats.get("edges"),
        connected_components: num_components,
        isolated_junctions: isolated.into_iter().map(|r| r.get("id")).collect(),
    }))
}

// ─── Error ──────────────────────────────────────────────────────────

enum NetworkError {
    Db(sqlx::Error),
    NotFound,
}

impl From<sqlx::Error> for NetworkError {
    fn from(e: sqlx::Error) -> Self {
        NetworkError::Db(e)
    }
}

impl IntoResponse for NetworkError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            NetworkError::NotFound => (StatusCode::NOT_FOUND, "network not found".to_string()),
            NetworkError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
