// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! OGC API - Features compliant endpoints and audit log.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use ptolemy_storage::AuditEntry;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn ogc_routes() -> Router<AppState> {
    Router::new()
        // OGC API - Features landing
        .route("/ogc", get(landing))
        .route("/ogc/conformance", get(conformance))
        .route("/ogc/collections", get(collections))
        .route("/ogc/collections/{id}", get(collection_info))
        .route("/ogc/collections/{id}/items", get(items))
        .route("/ogc/collections/{id}/items/{fid}", get(item))
        // Audit log
        .route("/audit", get(list_audit))
}

// ─── OGC API - Features ─────────────────────────────────────────────

#[derive(Serialize)]
struct LandingPage {
    title: String,
    description: String,
    links: Vec<Link>,
}

#[derive(Serialize)]
struct Link {
    href: String,
    rel: String,
    #[serde(rename = "type")]
    media_type: String,
    title: String,
}

async fn landing() -> Json<LandingPage> {
    Json(LandingPage {
        title: "Ptolemy OGC API".into(),
        description: "OGC API - Features compliant interface to Ptolemy versioned GIS database"
            .into(),
        links: vec![
            Link {
                href: "/api/v1/ogc".into(),
                rel: "self".into(),
                media_type: "application/json".into(),
                title: "This document".into(),
            },
            Link {
                href: "/api/v1/ogc/conformance".into(),
                rel: "conformance".into(),
                media_type: "application/json".into(),
                title: "Conformance classes".into(),
            },
            Link {
                href: "/api/v1/ogc/collections".into(),
                rel: "data".into(),
                media_type: "application/json".into(),
                title: "Collections".into(),
            },
        ],
    })
}

#[derive(Serialize)]
struct Conformance {
    #[serde(rename = "conformsTo")]
    conforms_to: Vec<String>,
}

async fn conformance() -> Json<Conformance> {
    Json(Conformance {
        conforms_to: vec![
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core".into(),
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson".into(),
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30".into(),
        ],
    })
}

#[derive(Serialize)]
struct Collection {
    id: String,
    title: String,
    description: String,
    extent: Option<serde_json::Value>,
    links: Vec<Link>,
}

#[derive(Serialize)]
struct Collections {
    collections: Vec<Collection>,
}

async fn collections(State(store): State<AppState>) -> Result<Json<Collections>, OgcError> {
    let rows = sqlx::query("SELECT id, name FROM datasets ORDER BY name")
        .fetch_all(store.pool())
        .await?;

    let cols: Vec<Collection> = rows
        .into_iter()
        .map(|row| {
            let id: Uuid = row.get("id");
            let name: String = row.get("name");
            Collection {
                id: id.to_string(),
                title: name.clone(),
                description: format!("Dataset: {name}"),
                extent: None,
                links: vec![Link {
                    href: format!("/api/v1/ogc/collections/{id}/items"),
                    rel: "items".into(),
                    media_type: "application/geo+json".into(),
                    title: "Items".into(),
                }],
            }
        })
        .collect();

    Ok(Json(Collections { collections: cols }))
}

async fn collection_info(
    State(store): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Collection>, OgcError> {
    let row = sqlx::query("SELECT id, name FROM datasets WHERE id = $1")
        .bind(id)
        .fetch_optional(store.pool())
        .await?
        .ok_or_else(|| OgcError::NotFound("collection not found".into()))?;

    let name: String = row.get("name");
    Ok(Json(Collection {
        id: id.to_string(),
        title: name.clone(),
        description: format!("Dataset: {name}"),
        extent: None,
        links: vec![Link {
            href: format!("/api/v1/ogc/collections/{id}/items"),
            rel: "items".into(),
            media_type: "application/geo+json".into(),
            title: "Items".into(),
        }],
    }))
}

#[derive(Deserialize)]
struct ItemsQuery {
    #[serde(default = "default_items_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    /// branch to query (defaults to main)
    branch: Option<Uuid>,
    /// bbox filter: minx,miny,maxx,maxy
    bbox: Option<String>,
}

fn default_items_limit() -> i64 {
    100
}

#[derive(Serialize)]
struct FeatureCollection {
    #[serde(rename = "type")]
    fc_type: String,
    features: Vec<serde_json::Value>,
    #[serde(rename = "numberMatched")]
    number_matched: i64,
    #[serde(rename = "numberReturned")]
    number_returned: usize,
}

async fn items(
    State(store): State<AppState>,
    Path(dataset_id): Path<Uuid>,
    Query(q): Query<ItemsQuery>,
) -> Result<Json<FeatureCollection>, OgcError> {
    // Find branch (main or specified)
    let branch_id = if let Some(b) = q.branch {
        b
    } else {
        let row =
            sqlx::query("SELECT id FROM branches WHERE dataset_id = $1 AND name = 'main' LIMIT 1")
                .bind(dataset_id)
                .fetch_optional(store.pool())
                .await?
                .ok_or_else(|| OgcError::NotFound("no main branch".into()))?;
        row.get("id")
    };

    let features = if let Some(bbox_str) = &q.bbox {
        // Parse bbox
        let parts: Vec<f64> = bbox_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if parts.len() != 4 {
            return Err(OgcError::NotFound("invalid bbox format".into()));
        }
        sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id FROM changesets c
                JOIN branches b ON b.head = c.id WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation, fv.geometry, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, ST_AsGeoJSON(geometry)::jsonb as geojson, properties
            FROM latest
            WHERE operation != 'delete'
              AND geometry IS NOT NULL
              AND geometry && ST_MakeEnvelope($2, $3, $4, $5, 4326)
            LIMIT $6 OFFSET $7",
        )
        .bind(branch_id)
        .bind(parts[0])
        .bind(parts[1])
        .bind(parts[2])
        .bind(parts[3])
        .bind(q.limit)
        .bind(q.offset)
        .fetch_all(store.pool())
        .await?
    } else {
        sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id FROM changesets c
                JOIN branches b ON b.head = c.id WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation, fv.geometry, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, ST_AsGeoJSON(geometry)::jsonb as geojson, properties
            FROM latest
            WHERE operation != 'delete'
            LIMIT $2 OFFSET $3",
        )
        .bind(branch_id)
        .bind(q.limit)
        .bind(q.offset)
        .fetch_all(store.pool())
        .await?
    };

    let geojson_features: Vec<serde_json::Value> = features
        .iter()
        .map(|row| {
            let fid: Uuid = row.get("feature_id");
            let geom: Option<serde_json::Value> = row.get("geojson");
            let props: serde_json::Value = row.get("properties");
            serde_json::json!({
                "type": "Feature",
                "id": fid.to_string(),
                "geometry": geom,
                "properties": props
            })
        })
        .collect();

    let count = geojson_features.len();
    Ok(Json(FeatureCollection {
        fc_type: "FeatureCollection".into(),
        features: geojson_features,
        number_matched: count as i64,
        number_returned: count,
    }))
}

async fn item(
    State(store): State<AppState>,
    Path((_dataset_id, feature_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, OgcError> {
    let row = sqlx::query(
        "SELECT feature_id, ST_AsGeoJSON(geometry)::jsonb as geojson, properties
         FROM feature_versions
         WHERE feature_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(feature_id)
    .fetch_optional(store.pool())
    .await?
    .ok_or_else(|| OgcError::NotFound("feature not found".into()))?;

    let geom: Option<serde_json::Value> = row.get("geojson");
    let props: serde_json::Value = row.get("properties");

    Ok(Json(serde_json::json!({
        "type": "Feature",
        "id": feature_id.to_string(),
        "geometry": geom,
        "properties": props
    })))
}

// ─── Audit Log ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: i64,
    actor: Option<String>,
}

fn default_audit_limit() -> i64 {
    100
}

async fn list_audit(
    State(store): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEntry>>, OgcError> {
    let entries = store.list_audit_log(q.limit, q.actor.as_deref()).await?;
    Ok(Json(entries))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum OgcError {
    Store(sqlx::Error),
    StoreErr(ptolemy_storage::StoreError),
    NotFound(String),
}

impl From<sqlx::Error> for OgcError {
    fn from(e: sqlx::Error) -> Self {
        OgcError::Store(e)
    }
}

impl From<ptolemy_storage::StoreError> for OgcError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        OgcError::StoreErr(e)
    }
}

impl IntoResponse for OgcError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            OgcError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            OgcError::Store(e) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            OgcError::StoreErr(ptolemy_storage::StoreError::NotFound(msg)) => {
                (StatusCode::NOT_FOUND, msg)
            }
            OgcError::StoreErr(e) => {
                tracing::error!("Store error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
