// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! STAC (SpatioTemporal Asset Catalog) API for raster/imagery discovery.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn stac_routes() -> Router<AppState> {
    Router::new()
        .route("/stac", get(stac_root))
        .route("/stac/collections", get(stac_collections))
        .route("/stac/collections/{id}", get(stac_collection))
        .route("/stac/collections/{id}/items", get(stac_items))
        .route("/stac/collections/{id}/items/{item_id}", get(stac_item))
        .route("/stac/search", get(stac_search))
}

async fn stac_root() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "type": "Catalog",
        "id": "ptolemy-stac",
        "title": "Ptolemy STAC Catalog",
        "description": "SpatioTemporal Asset Catalog for Ptolemy versioned GIS",
        "stac_version": "1.0.0",
        "conformsTo": [
            "https://api.stacspec.org/v1.0.0/core",
            "https://api.stacspec.org/v1.0.0/item-search",
            "https://api.stacspec.org/v1.0.0/ogcapi-features"
        ],
        "links": [
            {"rel": "self", "href": "/api/v1/stac", "type": "application/json"},
            {"rel": "root", "href": "/api/v1/stac", "type": "application/json"},
            {"rel": "collections", "href": "/api/v1/stac/collections", "type": "application/json"},
            {"rel": "search", "href": "/api/v1/stac/search", "type": "application/geo+json", "method": "GET"},
        ]
    }))
}

async fn stac_collections(
    State(store): State<AppState>,
) -> Result<Json<serde_json::Value>, StacError> {
    let rows = sqlx::query(
        "SELECT rc.id, rc.name, rc.srid, rc.pixel_type, rc.num_bands,
                d.name as dataset_name,
                ST_AsGeoJSON(ST_Extent(rt.bounds))::jsonb as extent
         FROM raster_catalogs rc
         JOIN datasets d ON d.id = rc.dataset_id
         LEFT JOIN raster_tiles rt ON rt.catalog_id = rc.id
         GROUP BY rc.id, rc.name, rc.srid, rc.pixel_type, rc.num_bands, d.name",
    )
    .fetch_all(store.pool())
    .await?;

    let collections: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            serde_json::json!({
                "type": "Collection",
                "id": id,
                "title": r.get::<String, _>("name"),
                "description": format!("{} raster catalog", r.get::<String, _>("dataset_name")),
                "stac_version": "1.0.0",
                "license": "proprietary",
                "extent": {
                    "spatial": {"bbox": [[-180, -90, 180, 90]]},
                    "temporal": {"interval": [[null, null]]}
                },
                "summaries": {
                    "srid": r.get::<i32, _>("srid"),
                    "pixel_type": r.get::<String, _>("pixel_type"),
                    "num_bands": r.get::<i32, _>("num_bands"),
                },
                "links": [
                    {"rel": "self", "href": format!("/api/v1/stac/collections/{id}")},
                    {"rel": "items", "href": format!("/api/v1/stac/collections/{id}/items")},
                ]
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "collections": collections,
        "links": [{"rel": "self", "href": "/api/v1/stac/collections"}]
    })))
}

async fn stac_collection(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StacError> {
    let r = sqlx::query(
        "SELECT rc.id, rc.name, rc.srid, rc.pixel_type, rc.num_bands, d.name as dataset_name
         FROM raster_catalogs rc
         JOIN datasets d ON d.id = rc.dataset_id
         WHERE rc.id = $1",
    )
    .bind(id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(StacError::NotFound)?;

    Ok(Json(serde_json::json!({
        "type": "Collection",
        "id": r.get::<Uuid, _>("id"),
        "title": r.get::<String, _>("name"),
        "description": format!("{} raster catalog", r.get::<String, _>("dataset_name")),
        "stac_version": "1.0.0",
        "license": "proprietary",
        "extent": {
            "spatial": {"bbox": [[-180, -90, 180, 90]]},
            "temporal": {"interval": [[null, null]]}
        },
        "links": [
            {"rel": "self", "href": format!("/api/v1/stac/collections/{id}")},
            {"rel": "items", "href": format!("/api/v1/stac/collections/{id}/items")},
            {"rel": "root", "href": "/api/v1/stac"},
        ]
    })))
}

async fn stac_items(
    State(store): State<AppState>,
    Path(catalog_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StacError> {
    let rows = sqlx::query(
        "SELECT id, zoom_level, ST_AsGeoJSON(bounds)::jsonb as geometry,
                ST_XMin(bounds) as xmin, ST_YMin(bounds) as ymin,
                ST_XMax(bounds) as xmax, ST_YMax(bounds) as ymax
         FROM raster_tiles WHERE catalog_id = $1
         ORDER BY zoom_level LIMIT 100",
    )
    .bind(catalog_id)
    .fetch_all(store.pool())
    .await?;

    let features: Vec<serde_json::Value> = rows.iter().map(|r| {
        let tile_id: Uuid = r.get("id");
        serde_json::json!({
            "type": "Feature",
            "stac_version": "1.0.0",
            "id": tile_id,
            "geometry": r.get::<Option<serde_json::Value>, _>("geometry"),
            "bbox": [
                r.get::<Option<f64>, _>("xmin"),
                r.get::<Option<f64>, _>("ymin"),
                r.get::<Option<f64>, _>("xmax"),
                r.get::<Option<f64>, _>("ymax"),
            ],
            "properties": {
                "zoom_level": r.get::<i32, _>("zoom_level"),
                "datetime": null,
            },
            "links": [
                {"rel": "self", "href": format!("/api/v1/stac/collections/{catalog_id}/items/{tile_id}")},
                {"rel": "collection", "href": format!("/api/v1/stac/collections/{catalog_id}")},
            ],
            "assets": {}
        })
    }).collect();

    Ok(Json(serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
        "links": [{"rel": "self", "href": format!("/api/v1/stac/collections/{catalog_id}/items")}]
    })))
}

async fn stac_item(
    State(store): State<AppState>,
    Path((catalog_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, StacError> {
    let r = sqlx::query(
        "SELECT id, zoom_level, ST_AsGeoJSON(bounds)::jsonb as geometry,
                ST_XMin(bounds) as xmin, ST_YMin(bounds) as ymin,
                ST_XMax(bounds) as xmax, ST_YMax(bounds) as ymax
         FROM raster_tiles WHERE id = $1 AND catalog_id = $2",
    )
    .bind(item_id)
    .bind(catalog_id)
    .fetch_optional(store.pool())
    .await?
    .ok_or(StacError::NotFound)?;

    Ok(Json(serde_json::json!({
        "type": "Feature",
        "stac_version": "1.0.0",
        "id": r.get::<Uuid, _>("id"),
        "geometry": r.get::<Option<serde_json::Value>, _>("geometry"),
        "bbox": [
            r.get::<Option<f64>, _>("xmin"),
            r.get::<Option<f64>, _>("ymin"),
            r.get::<Option<f64>, _>("xmax"),
            r.get::<Option<f64>, _>("ymax"),
        ],
        "properties": {
            "zoom_level": r.get::<i32, _>("zoom_level"),
            "datetime": null,
        },
        "links": [
            {"rel": "self", "href": format!("/api/v1/stac/collections/{catalog_id}/items/{item_id}")},
            {"rel": "collection", "href": format!("/api/v1/stac/collections/{catalog_id}")},
            {"rel": "root", "href": "/api/v1/stac"},
        ],
        "assets": {}
    })))
}

/// STAC search across all collections.
#[derive(Deserialize)]
struct StacSearchQuery {
    bbox: Option<String>,
    #[allow(dead_code)]
    datetime: Option<String>,
    limit: Option<i64>,
    collections: Option<String>,
}

async fn stac_search(
    State(store): State<AppState>,
    Query(q): Query<StacSearchQuery>,
) -> Result<Json<serde_json::Value>, StacError> {
    let limit = q.limit.unwrap_or(10);
    let mut conditions = vec!["1=1".to_string()];

    if let Some(bbox) = &q.bbox {
        let coords: Vec<f64> = bbox
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if coords.len() == 4 {
            conditions.push(format!(
                "bounds && ST_MakeEnvelope({}, {}, {}, {}, 4326)",
                coords[0], coords[1], coords[2], coords[3]
            ));
        }
    }

    if let Some(colls) = &q.collections {
        let ids: Vec<&str> = colls.split(',').collect();
        let in_clause = ids
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        conditions.push(format!("catalog_id::text IN ({})", in_clause));
    }

    let where_clause = conditions.join(" AND ");
    let query = format!(
        "SELECT id, catalog_id, zoom_level, ST_AsGeoJSON(bounds)::jsonb as geometry
         FROM raster_tiles WHERE {where_clause} LIMIT $1"
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .fetch_all(store.pool())
        .await?;

    let features: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "type": "Feature",
                "stac_version": "1.0.0",
                "id": r.get::<Uuid, _>("id"),
                "geometry": r.get::<Option<serde_json::Value>, _>("geometry"),
                "properties": {"zoom_level": r.get::<i32, _>("zoom_level"), "datetime": null},
                "links": [],
                "assets": {},
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
        "context": {"returned": features.len(), "limit": limit},
    })))
}

enum StacError {
    Db(sqlx::Error),
    NotFound,
}
impl From<sqlx::Error> for StacError {
    fn from(e: sqlx::Error) -> Self {
        StacError::Db(e)
    }
}
impl IntoResponse for StacError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            StacError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            StacError::Db(e) => {
                tracing::error!("DB: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (s, Json(serde_json::json!({"error": m}))).into_response()
    }
}
