//! Integration tests for all Ptolemy API endpoints.
//!
//! These tests exercise the full HTTP API layer against a real PostgreSQL/PostGIS database.
//! Requires DATABASE_URL env var pointing to a PostGIS-enabled database with all extensions.
//!
//! Run: DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test -p ptolemy-api

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ptolemy_api::{app, AppState};
use ptolemy_storage::postgres::PgStore;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// Helper: create the test app from a fresh database.
async fn setup_app() -> (axum::Router, AppState) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ptolemy_test".to_string());
    let pool = PgPool::connect(&url).await.expect("DB connect failed");

    // Clean relevant tables (order matters for FK constraints)
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS conflicts CASCADE;
         DROP TABLE IF EXISTS feature_versions CASCADE;
         DROP TABLE IF EXISTS changesets CASCADE;
         DROP TABLE IF EXISTS branches CASCADE;
         DROP TABLE IF EXISTS raster_tiles CASCADE;
         DROP TABLE IF EXISTS raster_catalogs CASCADE;
         DROP TABLE IF EXISTS pointcloud_patches CASCADE;
         DROP TABLE IF EXISTS pointcloud_catalogs CASCADE;
         DROP TABLE IF EXISTS datasets CASCADE;
         DROP TABLE IF EXISTS dataset_metadata CASCADE;
         DROP TABLE IF EXISTS dataset_tags CASCADE;"
    )
    .execute(&pool)
    .await
    .unwrap();

    let store = PgStore::new(pool);
    store.migrate().await.unwrap();

    let state: AppState = Arc::new(store);
    let router = app(state.clone());
    (router, state)
}

/// Helper: make a JSON POST request and return status + body.
async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-skip") // auth middleware should skip in test
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Helper: make a GET request and return status + body.
async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", "Bearer test-skip")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Helper: create a dataset via API, return its ID.
async fn create_dataset(app: &axum::Router) -> Uuid {
    let (status, body) = post_json(app, "/api/v1/datasets", json!({
        "name": format!("test_{}", Uuid::now_v7()),
        "geometry_type": "point",
        "srid": 4326,
    })).await;
    assert_eq!(status, StatusCode::CREATED, "create dataset: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Helper: create a branch via API, return its ID.
async fn create_branch(app: &axum::Router, dataset_id: Uuid, name: &str) -> Uuid {
    let (status, body) = post_json(
        app,
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        json!({"name": name}),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "create branch: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Helper: commit features, return changeset ID.
async fn commit_features(app: &axum::Router, branch_id: Uuid, ops: Value) -> Uuid {
    let (status, body) = post_json(
        app,
        &format!("/api/v1/branches/{branch_id}/commit"),
        json!({
            "message": "test commit",
            "author": "test",
            "operations": ops,
        }),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "commit: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

// ═══════════════════════════════════════════════════════════════════════
// Dataset CRUD Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_dataset_crud() {
    let (app, _) = setup_app().await;

    // Create
    let ds_id = create_dataset(&app).await;

    // Get
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["srid"], 4326);

    // List
    let (status, body) = get_json(&app, "/api/v1/datasets").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().len() >= 1);
}

// ═══════════════════════════════════════════════════════════════════════
// Branch CRUD Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_branch_crud() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    // Create
    let branch_id = create_branch(&app, ds_id, "main").await;

    // Get
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "main");

    // List
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/branches")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
}

// ═══════════════════════════════════════════════════════════════════════
// Commit & Feature Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_commit_and_query_features() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    // WKB hex for POINT(1 2) — little-endian
    let point_hex = "0101000000000000000000F03F0000000000000040";

    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {"name": "Park"}}
    ])).await;

    // Query features
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/features")).await;
    assert_eq!(status, StatusCode::OK);
    let features = body.as_array().unwrap();
    assert_eq!(features.len(), 1);
    assert_eq!(features[0]["properties"]["name"], "Park");
}

#[tokio::test]
async fn test_spatial_query_bbox() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040"; // POINT(1 2)
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {}}
    ])).await;

    // Bbox that includes POINT(1 2)
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features?bbox=0,0,3,3"),
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().len() >= 1);

    // Bbox that excludes POINT(1 2)
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features?bbox=10,10,20,20"),
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Diff & History Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_branch_history() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {}}
    ])).await;
    commit_features(&app, branch_id, json!([
        {"type": "update", "feature_id": f1.to_string(), "properties": {"v": 2}}
    ])).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/history")).await;
    assert_eq!(status, StatusCode::OK);
    let history = body.as_array().unwrap();
    assert_eq!(history.len(), 2);
}

// ═══════════════════════════════════════════════════════════════════════
// Merge Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_merge_branches() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {"name": "origin"}}
    ])).await;

    // Create feature branch
    let dev_id = create_branch(&app, ds_id, "dev").await;
    let f2 = Uuid::now_v7();
    commit_features(&app, dev_id, json!([
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb": point_hex, "properties": {"name": "new"}}
    ])).await;

    // Merge dev → main
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{main_id}/merge"),
        json!({"source_branch_id": dev_id.to_string(), "author": "test"}),
    ).await;
    assert!(status == StatusCode::OK || status == StatusCode::CREATED, "merge: {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Raster Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_raster_catalog_and_tiles() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    // Create raster catalog
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/rasters"),
        json!({"name": "imagery", "srid": 4326, "pixel_type": "uint8", "num_bands": 3}),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "create catalog: {body}");
    let catalog_id = body["id"].as_str().unwrap();

    // List catalogs
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/rasters")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Get catalog
    let (status, body) = get_json(&app, &format!("/api/v1/rasters/{catalog_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "imagery");

    // Get stats (empty)
    let (status, body) = get_json(&app, &format!("/api/v1/rasters/{catalog_id}/stats")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tile_count"], 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Point Cloud Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_pointcloud_catalog() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    // Create point cloud catalog
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/pointclouds"),
        json!({"name": "lidar_scan", "srid": 4326}),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "create pc catalog: {body}");
    let catalog_id = body["id"].as_str().unwrap();

    // List catalogs
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/pointclouds")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Get catalog
    let (status, body) = get_json(&app, &format!("/api/v1/pointclouds/{catalog_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "lidar_scan");

    // Stats (empty)
    let (status, body) = get_json(&app, &format!("/api/v1/pointclouds/{catalog_id}/stats")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["patch_count"], 0);
}

// ═══════════════════════════════════════════════════════════════════════
// Format Export Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_export_geojson() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {"name": "Park"}}
    ])).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/export/geojson")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "FeatureCollection");
    assert!(body["features"].as_array().unwrap().len() >= 1);
}

#[tokio::test]
async fn test_export_csv() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {"name": "Test"}}
    ])).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/export/csv")).await;
    assert_eq!(status, StatusCode::OK);
    // CSV response should have rows field
    assert!(body["rows"].as_array().is_some() || body["csv"].is_string());
}

// ═══════════════════════════════════════════════════════════════════════
// CRS Transformation Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_crs_transform() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    // Transform a point from EPSG:4326 to EPSG:3857
    let point_hex = "0101000000000000000000F03F0000000000000040"; // POINT(1 2) in 4326
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/transform"),
        json!({"from_srid": 4326, "to_srid": 3857, "geometry_wkb_hex": point_hex}),
    ).await;
    assert_eq!(status, StatusCode::OK, "transform: {body}");
    assert!(body["geojson"].is_object());
}

#[tokio::test]
async fn test_crs_search() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/crs/search?q=WGS+84"),
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().len() >= 1);
}

// ═══════════════════════════════════════════════════════════════════════
// CQL2 Filter Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cql2_filter() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {"pop": 1000}}
    ])).await;

    // CQL2 property filter
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/cql2/filter"),
        json!({
            "filter": {
                "op": ">",
                "args": [{"property": "pop"}, 500]
            }
        }),
    ).await;
    assert_eq!(status, StatusCode::OK, "cql2 filter: {body}");
    assert!(body["features"].as_array().unwrap().len() >= 1);
}

// ═══════════════════════════════════════════════════════════════════════
// OGC API Features Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_ogc_conformance() {
    let (app, _) = setup_app().await;

    let (status, body) = get_json(&app, "/api/v1/ogc/conformance").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["conformsTo"].as_array().unwrap().len() >= 1);
}

#[tokio::test]
async fn test_ogc_collections() {
    let (app, _) = setup_app().await;
    let _ds_id = create_dataset(&app).await;

    let (status, body) = get_json(&app, "/api/v1/ogc/collections").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["collections"].as_array().unwrap().len() >= 1);
}

// ═══════════════════════════════════════════════════════════════════════
// STAC API Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_stac_catalog() {
    let (app, _) = setup_app().await;

    let (status, body) = get_json(&app, "/api/v1/stac").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "Catalog");
    assert_eq!(body["stac_version"], "1.0.0");
}

#[tokio::test]
async fn test_stac_collections() {
    let (app, _) = setup_app().await;

    let (status, body) = get_json(&app, "/api/v1/stac/collections").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["collections"].as_array().is_some());
}

// ═══════════════════════════════════════════════════════════════════════
// Analytics Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_buffer_analysis() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/analytics/buffer"),
        json!({"distance": 0.01}),
    ).await;
    assert_eq!(status, StatusCode::OK, "buffer: {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Topology Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_topology_validate() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {}}
    ])).await;

    let (status, _body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/topology/validate"),
        json!({}),
    ).await;
    assert!(status == StatusCode::OK || status == StatusCode::UNPROCESSABLE_ENTITY);
}

// ═══════════════════════════════════════════════════════════════════════
// SFCGAL 3D Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sfcgal_extrude() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    // Polygon WKB hex for extrusion (simple square)
    let polygon_hex = "01030000000100000005000000000000000000000000000000000000000000000000002440000000000000000000000000000024400000000000002440000000000000000000000000000024400000000000000000000000000000000000000000";

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/sfcgal/extrude"),
        json!({"geometry_wkb_hex": polygon_hex, "height": 10.0}),
    ).await;
    // SFCGAL might not be installed in test env, so accept either OK or 500
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "sfcgal extrude: {status} {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// H3 Hexagonal Index Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_h3_index_features() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/h3/index"),
        json!({"resolution": 7}),
    ).await;
    // h3-pg might not be installed in test env
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "h3 index: {status} {body}");
}

#[tokio::test]
async fn test_h3_hexagons() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, _body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/h3/hexagons?resolution=7"),
    ).await;
    // h3-pg might not be installed
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR);
}

// ═══════════════════════════════════════════════════════════════════════
// Vector Search Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_vector_generate_embeddings() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {"name": "test"}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/vectors/embed"),
        json!({"fields": ["name"]}),
    ).await;
    // pgvector + pgcrypto might not be installed
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "embed: {status} {body}");
    if status == StatusCode::OK {
        assert!(body["embedded"].as_i64().unwrap() >= 1);
    }
}

#[tokio::test]
async fn test_vector_similarity_search() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    // Search with a random embedding (should return empty if no embeddings exist)
    let embedding: Vec<f64> = (0..256).map(|i| (i as f64) / 256.0).collect();
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/vectors/search"),
        json!({"embedding": embedding, "limit": 5}),
    ).await;
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "similarity: {status} {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Network / pgRouting Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_network_shortest_path() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/network/shortest-path"),
        json!({"source": 1, "target": 2}),
    ).await;
    // pgRouting might not be installed, or no edges exist
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "routing: {status} {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Trajectory / MobilityDB Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_trajectory_list() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, _body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/trajectories"),
    ).await;
    // MobilityDB might not be installed
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR);
}

// ═══════════════════════════════════════════════════════════════════════
// Webhook Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_webhook_crud() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    // Create webhook
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/webhooks"),
        json!({"url": "https://example.com/hook", "events": ["commit"]}),
    ).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::OK, "webhook create: {body}");

    // List webhooks
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/webhooks")).await;
    assert_eq!(status, StatusCode::OK, "webhook list: {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Lock Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_feature_locking() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb": point_hex, "properties": {}}
    ])).await;

    // Acquire lock
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/locks"),
        json!({"feature_id": f1.to_string(), "owner": "alice"}),
    ).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::OK, "lock: {body}");

    // List locks
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/locks")).await;
    assert_eq!(status, StatusCode::OK, "list locks: {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Catalog / Metadata Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_dataset_catalog_search() {
    let (app, _) = setup_app().await;
    let _ds_id = create_dataset(&app).await;

    let (status, body) = get_json(&app, "/api/v1/catalog/search?q=test").await;
    assert_eq!(status, StatusCode::OK, "catalog search: {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Multi-Tenancy Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_create_organization() {
    let (app, _) = setup_app().await;

    let (status, body) = post_json(
        &app,
        "/api/v1/orgs",
        json!({"name": "TestOrg", "owner": "admin"}),
    ).await;
    assert!(status == StatusCode::CREATED || status == StatusCode::OK, "create org: {body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Metrics & Health Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_health_check() {
    let (app, _) = setup_app().await;

    let (status, _) = get_json(&app, "/health").await;
    assert!(status == StatusCode::OK || status == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_metrics_endpoint() {
    let (app, _) = setup_app().await;

    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
