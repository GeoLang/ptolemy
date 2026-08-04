// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Geometric network and utility network API — graph tracing and analysis.

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_storage::WriteGrant;
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

/// The routing routes below call `pgr_*` functions and have no hand-rolled
/// form: `trace_network` walks the edge table itself, but dijkstra, A*, driving
/// distance, TSP and connected components are pgRouting or nothing. Without the
/// extension the call is not a failure to hide behind a 500, it is a route this
/// deployment does not have.
async fn require_pgrouting(store: &AppState) -> Result<(), NetworkError> {
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pgrouting')",
    )
    .fetch_one(store.read_pool())
    .await?;
    if present {
        Ok(())
    } else {
        Err(NetworkError::NoPgRouting)
    }
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
    .fetch_all(store.read_pool())
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<CreateNetworkRequest>,
) -> Result<(StatusCode, Json<Network>), NetworkError> {
    let id = store
        .create_network(&grant, &req.name, &req.network_type)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(Network {
            id,
            dataset_id: grant.id(),
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
        .fetch_optional(store.read_pool())
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
    ).bind(network_id).fetch_all(store.read_pool()).await?;
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<AddJunctionRequest>,
) -> Result<(StatusCode, Json<Junction>), NetworkError> {
    let id = store
        .add_network_junction(&grant, req.feature_id, req.lng, req.lat)
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
    ).bind(network_id).fetch_all(store.read_pool()).await?;
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
    Extension(grant): Extension<WriteGrant>,
    Json(req): Json<AddEdgeRequest>,
) -> Result<StatusCode, NetworkError> {
    store
        .add_network_edge(
            &grant,
            req.feature_id,
            req.from_junction,
            req.to_junction,
            req.cost,
        )
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
            .fetch_all(store.read_pool()).await?
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
            .fetch_all(store.read_pool()).await?
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

// pgRouting wants bigint vertex and edge ids, the schema has uuids. Both rank
// helpers assign row_number() over the network's rows, the outer statement
// ranks the same rows the same way to translate the request uuids in and the
// result ids back out, and because every rank is computed inside the one
// statement the mapping cannot drift. `net` is the sql expression for the
// network id: a bind parameter outside, a spliced literal inside the quoted
// sql handed to pgr_*.

fn node_rank(net: &str) -> String {
    format!(
        "SELECT id, geometry, row_number() OVER (ORDER BY id) AS nid \
         FROM network_junctions WHERE network_id = {net}"
    )
}

fn edge_rank(net: &str) -> String {
    format!(
        "SELECT id, from_junction, to_junction, cost, row_number() OVER (ORDER BY id) AS eid \
         FROM network_edges WHERE network_id = {net} AND enabled = TRUE"
    )
}

/// A sql expression evaluating to the edges sql string handed to pgr_*
/// functions. `xy` adds the endpoint coordinates pgr_astar's heuristic needs.
fn pgr_edges_expr(xy: bool) -> String {
    let net = "''' || $1::text || '''";
    let xy_cols = if xy {
        ", ST_X(ns.geometry) AS x1, ST_Y(ns.geometry) AS y1, \
         ST_X(nt.geometry) AS x2, ST_Y(nt.geometry) AS y2"
    } else {
        ""
    };
    format!(
        "'WITH n AS ({n}), e AS ({e}) \
         SELECT e.eid AS id, ns.nid AS source, nt.nid AS target, \
                e.cost, e.cost AS reverse_cost{xy_cols} \
         FROM e JOIN n ns ON ns.id = e.from_junction \
                JOIN n nt ON nt.id = e.to_junction'",
        n = node_rank(net),
        e = edge_rank(net)
    )
}

/// Shared tail of the path-shaped statements: an unknown junction coalesces to
/// rank 0, which no vertex holds, so pgr_* returns no rows and the route
/// answers empty instead of erroring.
fn path_sql(pgr_call: &str) -> String {
    format!(
        "WITH n AS ({n}), e AS ({e}),
         path AS (SELECT * FROM {pgr_call})
         SELECT p.seq, nj.id AS node_id, ne.id AS edge_id, p.cost, p.agg_cost
         FROM path p
         JOIN n nj ON nj.nid = p.node
         LEFT JOIN e ne ON ne.eid = p.edge
         ORDER BY p.seq",
        n = node_rank("$1"),
        e = edge_rank("$1")
    )
}

fn rows_to_path(rows: &[sqlx::postgres::PgRow]) -> PathResult {
    if rows.is_empty() {
        return PathResult {
            found: false,
            path_junctions: vec![],
            path_edges: vec![],
            total_cost: 0.0,
        };
    }
    let mut path_junctions = Vec::new();
    let mut path_edges = Vec::new();
    let mut total_cost = 0.0f64;
    for row in rows {
        path_junctions.push(row.get("node_id"));
        // the arrival row carries edge -1, which ranks to no edge
        if let Some(edge) = row.get::<Option<Uuid>, _>("edge_id") {
            path_edges.push(edge);
        }
        let agg: f64 = row.get("agg_cost");
        if agg > total_cost {
            total_cost = agg;
        }
    }
    PathResult {
        found: true,
        path_junctions,
        path_edges,
        total_cost,
    }
}

async fn shortest_path(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<ShortestPathRequest>,
) -> Result<Json<PathResult>, NetworkError> {
    require_pgrouting(&store).await?;
    let call = format!(
        "pgr_dijkstra({expr},
             COALESCE((SELECT nid FROM n WHERE id = $2), 0),
             COALESCE((SELECT nid FROM n WHERE id = $3), 0),
             directed := false)",
        expr = pgr_edges_expr(false)
    );
    let rows = sqlx::query(&path_sql(&call))
        .bind(network_id)
        .bind(req.from_junction)
        .bind(req.to_junction)
        .fetch_all(store.read_pool())
        .await?;
    Ok(Json(rows_to_path(&rows)))
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
    require_pgrouting(&store).await?;
    let call = format!(
        "pgr_astar({expr},
             COALESCE((SELECT nid FROM n WHERE id = $2), 0),
             COALESCE((SELECT nid FROM n WHERE id = $3), 0),
             directed := false)",
        expr = pgr_edges_expr(true)
    );
    let rows = sqlx::query(&path_sql(&call))
        .bind(network_id)
        .bind(req.from_junction)
        .bind(req.to_junction)
        .fetch_all(store.read_pool())
        .await?;
    Ok(Json(rows_to_path(&rows)))
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
    node: Uuid,
    edge: Option<Uuid>,
    cost: f64,
    agg_cost: f64,
}

async fn driving_distance(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<DrivingDistanceRequest>,
) -> Result<Json<IsochroneResult>, NetworkError> {
    require_pgrouting(&store).await?;
    let call = format!(
        "pgr_drivingDistance({expr},
             COALESCE((SELECT nid FROM n WHERE id = $2), 0),
             $3, directed := false)",
        expr = pgr_edges_expr(false)
    );
    let rows = sqlx::query(&path_sql(&call))
        .bind(network_id)
        .bind(req.start_junction)
        .bind(req.max_cost)
        .fetch_all(store.read_pool())
        .await?;

    let nodes: Vec<IsochroneNode> = rows
        .iter()
        .map(|r| IsochroneNode {
            node: r.get("node_id"),
            edge: r.get("edge_id"),
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
    ordered_junctions: Vec<Uuid>,
    total_cost: f64,
}

async fn tsp_tour(
    State(store): State<AppState>,
    Path(network_id): Path<Uuid>,
    Json(req): Json<TspRequest>,
) -> Result<Json<TspResult>, NetworkError> {
    require_pgrouting(&store).await?;
    // a matrix under two stops has no tour, and pgr_TSP errors rather than
    // returning empty on one, so resolve the request against real junctions
    // first
    let stops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM network_junctions WHERE network_id = $1 AND id = ANY($2)",
    )
    .bind(network_id)
    .bind(&req.junction_ids)
    .fetch_one(store.read_pool())
    .await?;
    if stops < 2 {
        return Ok(Json(TspResult {
            ordered_junctions: vec![],
            total_cost: 0.0,
        }));
    }

    // the matrix sql is assembled in-database: quote_literal re-quotes the
    // edges sql for its second level of nesting, and the wanted ranks become
    // an array literal
    let sql = format!(
        "WITH n AS ({n}),
         tour AS (SELECT seq, node, agg_cost FROM pgr_TSP(
             'SELECT * FROM pgr_dijkstraCostMatrix(' || quote_literal({expr}) || ', '
                 || (SELECT 'ARRAY[' || string_agg(nid::text, ',') || ']::bigint[]'
                     FROM n WHERE id = ANY($2))
                 || ', directed := false)',
             start_id := COALESCE((SELECT nid FROM n WHERE id = $3 AND id = ANY($2)), 0)))
         SELECT t.seq, nj.id AS node_id, t.agg_cost
         FROM tour t JOIN n nj ON nj.nid = t.node
         ORDER BY t.seq",
        n = node_rank("$1"),
        expr = pgr_edges_expr(false)
    );
    let rows = sqlx::query(&sql)
        .bind(network_id)
        .bind(&req.junction_ids)
        .bind(req.start_junction)
        .fetch_all(store.read_pool())
        .await?;

    let mut ordered = Vec::new();
    let mut total = 0.0f64;
    for row in &rows {
        ordered.push(row.get::<Uuid, _>("node_id"));
        let agg: f64 = row.get("agg_cost");
        if agg > total {
            total = agg;
        }
    }

    Ok(Json(TspResult {
        ordered_junctions: ordered,
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
    require_pgrouting(&store).await?;
    let stats = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM network_junctions WHERE network_id = $1) as junctions,
            (SELECT COUNT(*) FROM network_edges WHERE network_id = $1) as edges",
    )
    .bind(network_id)
    .fetch_one(store.read_pool())
    .await?;

    let components: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT COUNT(DISTINCT component)
         FROM pgr_connectedComponents({expr})",
        expr = pgr_edges_expr(false)
    ))
    .bind(network_id)
    .fetch_optional(store.read_pool())
    .await?;

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
    .fetch_all(store.read_pool())
    .await?;

    Ok(Json(ConnectivityReport {
        total_junctions: stats.get("junctions"),
        total_edges: stats.get("edges"),
        connected_components: components.unwrap_or(0),
        isolated_junctions: isolated.into_iter().map(|r| r.get("id")).collect(),
    }))
}

// ─── Error ──────────────────────────────────────────────────────────

enum NetworkError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
    NotFound,
    NoPgRouting,
}

impl From<sqlx::Error> for NetworkError {
    fn from(e: sqlx::Error) -> Self {
        NetworkError::Db(e)
    }
}

impl From<ptolemy_storage::StoreError> for NetworkError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        NetworkError::Store(e)
    }
}

impl IntoResponse for NetworkError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            NetworkError::NotFound => (StatusCode::NOT_FOUND, "network not found".to_string()),
            NetworkError::NoPgRouting => (
                StatusCode::NOT_IMPLEMENTED,
                "network routing needs the pgRouting extension, which this database does not have"
                    .to_string(),
            ),
            NetworkError::Store(e) => crate::errors::store_error_status(&e),
            NetworkError::Db(e) => {
                crate::errors::log_db_error("network", &e);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
