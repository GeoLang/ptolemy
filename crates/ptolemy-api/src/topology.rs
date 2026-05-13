// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! PostGIS native topology — faces, edges, nodes, topology validation.

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

pub fn topology_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/datasets/{id}/topologies",
            get(list_topologies).post(create_topology),
        )
        .route("/topologies/{name}/validate", post(validate_topology))
        .route("/topologies/{name}/faces", get(list_faces))
        .route("/topologies/{name}/edges", get(list_topo_edges))
        .route("/topologies/{name}/nodes", get(list_topo_nodes))
        .route("/topologies/{name}/add-face", post(add_face))
        .route("/topologies/{name}/simplify", post(simplify_topology))
}

#[derive(Serialize)]
struct TopologyInfo {
    id: i32,
    name: String,
    srid: i32,
    precision: f64,
}

async fn list_topologies(
    State(store): State<AppState>,
    Path(_dataset_id): Path<Uuid>,
) -> Result<Json<Vec<TopologyInfo>>, TopoError> {
    let rows = sqlx::query("SELECT id, name, srid, precision FROM topology.topology ORDER BY name")
        .fetch_all(store.pool())
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TopologyInfo {
                id: r.get("id"),
                name: r.get("name"),
                srid: r.get("srid"),
                precision: r.get("precision"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateTopologyRequest {
    name: String,
    #[serde(default = "default_srid")]
    srid: i32,
    #[serde(default = "default_precision")]
    precision: f64,
}
fn default_srid() -> i32 {
    4326
}
fn default_precision() -> f64 {
    0.000001
}

async fn create_topology(
    State(store): State<AppState>,
    Path(_dataset_id): Path<Uuid>,
    Json(req): Json<CreateTopologyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), TopoError> {
    let row = sqlx::query("SELECT topology.CreateTopology($1, $2, $3) as topo_id")
        .bind(&req.name)
        .bind(req.srid)
        .bind(req.precision)
        .fetch_one(store.pool())
        .await?;
    let topo_id: i32 = row.get("topo_id");
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"topology_id": topo_id, "name": req.name})),
    ))
}

async fn validate_topology(
    State(store): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, TopoError> {
    let rows = sqlx::query("SELECT error, id1, id2 FROM topology.ValidateTopology($1)")
        .bind(&name)
        .fetch_all(store.pool())
        .await?;

    let errors: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "error": r.get::<String, _>("error"),
                "id1": r.get::<i32, _>("id1"),
                "id2": r.get::<i32, _>("id2"),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "topology": name,
        "valid": errors.is_empty(),
        "error_count": errors.len(),
        "errors": errors,
    })))
}

async fn list_faces(
    State(store): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, TopoError> {
    let query = format!(
        "SELECT face_id, ST_AsGeoJSON(mbr)::jsonb as bounds FROM {}.face WHERE face_id > 0",
        sanitize_topo_name(&name)
    );
    let rows = sqlx::query(&query).fetch_all(store.pool()).await?;
    let faces: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "face_id": r.get::<i32, _>("face_id"),
                "bounds": r.get::<Option<serde_json::Value>, _>("bounds"),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({"faces": faces})))
}

async fn list_topo_edges(
    State(store): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, TopoError> {
    let query = format!(
        "SELECT edge_id, start_node, end_node, left_face, right_face,
                ST_AsGeoJSON(geom)::jsonb as geometry
         FROM {}.edge_data ORDER BY edge_id",
        sanitize_topo_name(&name)
    );
    let rows = sqlx::query(&query).fetch_all(store.pool()).await?;
    let edges: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "edge_id": r.get::<i32, _>("edge_id"),
                "start_node": r.get::<i32, _>("start_node"),
                "end_node": r.get::<i32, _>("end_node"),
                "left_face": r.get::<i32, _>("left_face"),
                "right_face": r.get::<i32, _>("right_face"),
                "geometry": r.get::<Option<serde_json::Value>, _>("geometry"),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({"edges": edges})))
}

async fn list_topo_nodes(
    State(store): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, TopoError> {
    let query = format!(
        "SELECT node_id, containing_face, ST_AsGeoJSON(geom)::jsonb as geometry
         FROM {}.node ORDER BY node_id",
        sanitize_topo_name(&name)
    );
    let rows = sqlx::query(&query).fetch_all(store.pool()).await?;
    let nodes: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "node_id": r.get::<i32, _>("node_id"),
                "containing_face": r.get::<Option<i32>, _>("containing_face"),
                "geometry": r.get::<Option<serde_json::Value>, _>("geometry"),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({"nodes": nodes})))
}

#[derive(Deserialize)]
struct AddFaceRequest {
    geometry_wkb_hex: String,
}

async fn add_face(
    State(store): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<AddFaceRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), TopoError> {
    let wkb =
        hex::decode(&req.geometry_wkb_hex).map_err(|_| TopoError::Bad("invalid hex".into()))?;
    let row = sqlx::query("SELECT topology.AddFace($1, ST_GeomFromWKB($2, 4326)) as face_id")
        .bind(&name)
        .bind(&wkb)
        .fetch_one(store.pool())
        .await?;
    let face_id: i32 = row.get("face_id");
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"face_id": face_id})),
    ))
}

#[derive(Deserialize)]
struct SimplifyRequest {
    #[serde(default = "default_precision")]
    tolerance: f64,
}

async fn simplify_topology(
    State(store): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<SimplifyRequest>,
) -> Result<Json<serde_json::Value>, TopoError> {
    let _rows = sqlx::query(
        "SELECT topology.ST_Simplify(topology.TopoGeom_addElement(
            topology.CreateTopoGeom($1, 3, 1), (edge_id, 2)::topology.TopoElement
        ), $2) FROM (SELECT edge_id FROM topology.ST_GetFaceEdges($1, 1) LIMIT 1) sub",
    )
    .bind(&name)
    .bind(req.tolerance)
    .fetch_optional(store.pool())
    .await?;
    Ok(Json(
        serde_json::json!({"status": "simplified", "tolerance": req.tolerance}),
    ))
}

/// Sanitize topology name to prevent SQL injection.
fn sanitize_topo_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

#[allow(dead_code)]
enum TopoError {
    Db(sqlx::Error),
    NotFound,
    Bad(String),
}
impl From<sqlx::Error> for TopoError {
    fn from(e: sqlx::Error) -> Self {
        TopoError::Db(e)
    }
}
impl IntoResponse for TopoError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            TopoError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            TopoError::Bad(msg) => (StatusCode::BAD_REQUEST, msg),
            TopoError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
