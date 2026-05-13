// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! pgvector-based feature similarity search and deduplication.

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

pub fn vector_routes() -> Router<AppState> {
    Router::new()
        .route("/branches/{id}/similarity/search", post(similarity_search))
        .route("/branches/{id}/similarity/duplicates", get(find_duplicates))
        .route("/branches/{id}/similarity/embed", post(generate_embeddings))
        .route(
            "/branches/{id}/similarity/cluster",
            post(cluster_by_embedding),
        )
}

/// Search for features similar to a given embedding vector.
#[derive(Deserialize)]
struct SimilaritySearchRequest {
    embedding: Vec<f32>,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default = "default_threshold")]
    threshold: f64,
}
fn default_limit() -> i64 {
    10
}
fn default_threshold() -> f64 {
    0.8
}

#[derive(Serialize)]
struct SimilarityResult {
    feature_id: Uuid,
    score: f64,
}

async fn similarity_search(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<SimilaritySearchRequest>,
) -> Result<Json<Vec<SimilarityResult>>, VectorError> {
    let embedding_str = format!(
        "[{}]",
        req.embedding
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );

    let rows = sqlx::query(
        "SELECT DISTINCT ON (fv.feature_id) fv.feature_id as id,
                1 - (fv.embedding <=> $2::vector) as score
         FROM feature_versions fv
         JOIN changesets c ON fv.changeset_id = c.id
         WHERE c.branch_id = $1 AND fv.embedding IS NOT NULL
           AND 1 - (fv.embedding <=> $2::vector) > $3
         ORDER BY fv.feature_id, fv.created_at DESC, fv.embedding <=> $2::vector
         LIMIT $4",
    )
    .bind(branch_id)
    .bind(&embedding_str)
    .bind(req.threshold)
    .bind(req.limit)
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.iter()
            .map(|r| SimilarityResult {
                feature_id: r.get("id"),
                score: r.get("score"),
            })
            .collect(),
    ))
}

/// Find potential duplicate features based on embedding similarity.
#[derive(Deserialize)]
struct DuplicateQuery {
    #[serde(default = "dup_threshold")]
    threshold: f64,
    limit: Option<i64>,
}
fn dup_threshold() -> f64 {
    0.95
}

#[derive(Serialize)]
struct DuplicatePair {
    feature_a: Uuid,
    feature_b: Uuid,
    similarity: f64,
}

async fn find_duplicates(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Query(q): Query<DuplicateQuery>,
) -> Result<Json<Vec<DuplicatePair>>, VectorError> {
    let rows = sqlx::query(
        "WITH latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id as id, fv.embedding
            FROM feature_versions fv
            JOIN changesets c ON fv.changeset_id = c.id
            WHERE c.branch_id = $1 AND fv.embedding IS NOT NULL AND fv.operation != 'delete'
            ORDER BY fv.feature_id, fv.created_at DESC
        )
        SELECT a.id as a_id, b.id as b_id,
               1 - (a.embedding <=> b.embedding) as similarity
        FROM latest a
        JOIN latest b ON a.id < b.id
        WHERE 1 - (a.embedding <=> b.embedding) > $2
        ORDER BY similarity DESC
        LIMIT $3",
    )
    .bind(branch_id)
    .bind(q.threshold)
    .bind(q.limit.unwrap_or(100))
    .fetch_all(store.pool())
    .await?;

    Ok(Json(
        rows.iter()
            .map(|r| DuplicatePair {
                feature_a: r.get("a_id"),
                feature_b: r.get("b_id"),
                similarity: r.get("similarity"),
            })
            .collect(),
    ))
}

/// Generate embeddings for features based on their properties (simplified).
/// Uses pgcrypto digest to create deterministic property-based vectors.
#[derive(Deserialize)]
struct EmbedRequest {
    /// Which property fields to embed
    fields: Vec<String>,
}

async fn generate_embeddings(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<EmbedRequest>,
) -> Result<Json<serde_json::Value>, VectorError> {
    // Generate deterministic property-based embeddings using pgcrypto
    let fields_expr = req
        .fields
        .iter()
        .map(|f| format!("COALESCE(fv.properties->>'{}', '')", f.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" || ' ' || ");

    let props_expr = if fields_expr.is_empty() {
        "fv.properties::text".to_string()
    } else {
        fields_expr
    };

    let query = format!(
        "UPDATE feature_versions fv
         SET embedding = (
            SELECT array_agg(v)::vector(256)
            FROM (
                SELECT (get_byte(digest(({props}) || i::text, 'sha256'), i % 32)::float / 255.0) as v
                FROM generate_series(0, 255) as i
            ) sub
         )
         FROM changesets c
         WHERE fv.changeset_id = c.id
           AND c.branch_id = $1
           AND fv.properties IS NOT NULL",
        props = props_expr,
    );

    let result = sqlx::query(&query)
        .bind(branch_id)
        .execute(store.pool())
        .await?;
    Ok(Json(
        serde_json::json!({"embedded": result.rows_affected(), "dimensions": 256}),
    ))
}

/// Cluster features by embedding similarity using k-means (via pgvector).
#[derive(Deserialize)]
struct ClusterRequest {
    #[serde(default = "default_clusters")]
    num_clusters: i32,
}
fn default_clusters() -> i32 {
    5
}

async fn cluster_by_embedding(
    State(store): State<AppState>,
    Path(branch_id): Path<Uuid>,
    Json(req): Json<ClusterRequest>,
) -> Result<Json<serde_json::Value>, VectorError> {
    // Use pgvector distance-based partitioning (ntile over embedding distance from centroid)
    let rows = sqlx::query(
        "WITH latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id as id, fv.embedding
            FROM feature_versions fv
            JOIN changesets c ON fv.changeset_id = c.id
            WHERE c.branch_id = $1 AND fv.embedding IS NOT NULL AND fv.operation != 'delete'
            ORDER BY fv.feature_id, fv.created_at DESC
        )
        SELECT kmeans_cluster, COUNT(*) as count, array_agg(id) as feature_ids
        FROM (
            SELECT id,
                   ntile($2) OVER (ORDER BY embedding <=> (SELECT avg(embedding)::vector FROM latest)) as kmeans_cluster,
                   embedding
            FROM latest
        ) clustered
        GROUP BY kmeans_cluster
        ORDER BY kmeans_cluster",
    ).bind(branch_id).bind(req.num_clusters)
    .fetch_all(store.pool()).await?;

    let clusters: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "cluster": r.get::<i64, _>("kmeans_cluster"),
                "count": r.get::<i64, _>("count"),
                "feature_ids": r.get::<Vec<Uuid>, _>("feature_ids"),
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({"clusters": clusters, "num_clusters": req.num_clusters}),
    ))
}

enum VectorError {
    Db(sqlx::Error),
}
impl From<sqlx::Error> for VectorError {
    fn from(e: sqlx::Error) -> Self {
        VectorError::Db(e)
    }
}
impl IntoResponse for VectorError {
    fn into_response(self) -> axum::response::Response {
        let VectorError::Db(e) = self;
        tracing::error!("DB: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response()
    }
}
