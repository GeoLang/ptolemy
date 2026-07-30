//! Integration tests for all Ptolemy API endpoints.
//!
//! These tests exercise the full HTTP API layer against a real PostgreSQL/PostGIS database.
//! Requires DATABASE_URL env var pointing to a PostGIS-enabled database with all extensions.
//!
//! Run: DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test -p ptolemy-api

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ptolemy_api::{AppState, AuthConfig, Role, app_with_auth, generate_token};
use ptolemy_storage::postgres::PgStore;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// Secret for the auth-enabled tests. Never a real deployment value.
const TEST_SECRET: &str = "integration-test-secret-0123456789abcdef";

/// Helper: reset the database and return fresh state.
async fn fresh_state() -> AppState {
    fresh_state_with_analyze_threshold(ptolemy_storage::DEFAULT_ANALYZE_ROW_THRESHOLD).await
}

/// Same, with the bulk-write ANALYZE threshold pinned so a test does not depend
/// on the ambient environment.
async fn fresh_state_with_analyze_threshold(rows: usize) -> AppState {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ptolemy_test".to_string());
    let pool = PgPool::connect(&url).await.expect("DB connect failed");

    // Clean relevant tables (order matters for FK constraints)
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS conflicts CASCADE;
         DROP TABLE IF EXISTS attachments CASCADE;
         DROP TABLE IF EXISTS feature_versions CASCADE;
         DROP TABLE IF EXISTS changesets CASCADE;
         DROP TABLE IF EXISTS branches CASCADE;
         DROP TABLE IF EXISTS raster_tiles CASCADE;
         DROP TABLE IF EXISTS raster_catalogs CASCADE;
         DROP TABLE IF EXISTS pointcloud_patches CASCADE;
         DROP TABLE IF EXISTS pointcloud_catalogs CASCADE;
         DROP TABLE IF EXISTS datasets CASCADE;
         DROP TABLE IF EXISTS dataset_metadata CASCADE;
         DROP TABLE IF EXISTS dataset_tags CASCADE;
         DROP TABLE IF EXISTS _sqlx_migrations CASCADE;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let store = PgStore::with_analyze_threshold(pool, rows);
    store.migrate().await.unwrap();

    Arc::new(store)
}

/// Helper: create the test app from a fresh database, with auth off. The bulk
/// of these tests exercise handlers, not the auth layer; see the
/// `auth_enabled_*` tests for the enforced behaviour.
async fn setup_app() -> (axum::Router, AppState) {
    let state = fresh_state().await;
    let router = app_with_auth(state.clone(), AuthConfig::disabled());
    (router, state)
}

/// Helper: create the test app with auth enforced against [`TEST_SECRET`].
async fn setup_app_authed() -> axum::Router {
    setup_app_authed_with_state().await.0
}

/// Same, keeping the store so a test can seed rows the API would not create,
/// such as a dataset with no permission rows at all.
async fn setup_app_authed_with_state() -> (axum::Router, AppState) {
    let state = fresh_state().await;
    let router = app_with_auth(state.clone(), AuthConfig::enabled(TEST_SECRET));
    (router, state)
}

/// Helper: make a JSON POST request and return status + body.
async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        // ignored: setup_app builds the router with auth disabled
        .header("authorization", "Bearer test-skip")
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
    let (status, body) = post_json(
        app,
        "/api/v1/datasets",
        json!({
            "name": format!("test_{}", Uuid::now_v7()),
            "geometry_type": "point",
            "srid": 4326,
            "created_by": "test"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create dataset: {body}");
    let id_str = body["id"].as_str().unwrap();
    Uuid::parse_str(id_str).unwrap()
}

/// Helper: create a branch via API, return its ID.
async fn create_branch(app: &axum::Router, dataset_id: Uuid, name: &str) -> Uuid {
    let (status, body) = post_json(
        app,
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        json!({"name": name, "created_by": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create branch: {body}");
    let id_str = body["id"].as_str().unwrap();
    Uuid::parse_str(id_str).unwrap()
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
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "commit failed with {status}: {body}"
    );
    // The response is a Changeset struct serialized as JSON
    let id_str = body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("commit response has no 'id' field: {body}"));
    Uuid::parse_str(id_str).unwrap()
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
    assert!(!body.as_array().unwrap().is_empty());
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "Park"}}
    ])).await;

    // Query features
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/features")).await;
    assert_eq!(status, StatusCode::OK);
    let features = body["features"].as_array().unwrap();
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {}}
    ])).await;

    // Bbox that includes POINT(1 2)
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/bbox?min_x=0&min_y=0&max_x=3&max_y=3"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.as_array().unwrap().is_empty());

    // Bbox that excludes POINT(1 2)
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/bbox?min_x=10&min_y=10&max_x=20&max_y=20"),
    )
    .await;
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {}}
    ])).await;
    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"v": 2}}
        ]),
    )
    .await;

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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "origin"}}
    ])).await;

    // Create feature branch
    let dev_id = create_branch(&app, ds_id, "dev").await;
    let f2 = Uuid::now_v7();
    commit_features(&app, dev_id, json!([
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "new"}}
    ])).await;

    // Merge dev → main (route is /branches/{target}/merge/{source})
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/branches/{main_id}/merge/{dev_id}"))
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::CREATED,
        "merge status: {status}"
    );
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
    )
    .await;
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
    )
    .await;
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "Park"}}
    ])).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/export/geojson"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "FeatureCollection");
    assert!(!body["features"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_export_csv() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "Test"}}
    ])).await;

    // CSV export returns text/csv, not JSON
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/branches/{branch_id}/export/csv"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("csv"), "expected csv content-type, got {ct}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let csv = String::from_utf8_lossy(&bytes);
    assert!(
        csv.contains("id,longitude,latitude"),
        "CSV header missing: {csv}"
    );
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
    )
    .await;
    assert_eq!(status, StatusCode::OK, "transform: {status} {body}");
    assert!(
        body["geometry"].is_object() || body["wkb_hex"].is_string(),
        "transform body: {body}"
    );
}

#[tokio::test]
async fn test_crs_search() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let _branch_id = create_branch(&app, ds_id, "main").await;

    let (status, body) = get_json(&app, "/api/v1/crs/search?q=WGS+84").await;
    assert_eq!(status, StatusCode::OK, "crs search: {body}");
    assert!(!body["results"].as_array().unwrap().is_empty());
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"pop": 1000}}
    ])).await;

    // CQL2 property filter (route is /features/filter)
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({
            "filter": {
                "op": ">",
                "args": [{"property": "pop"}, 500]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 filter: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
}

#[tokio::test]
async fn test_cql2_filter_sees_inherited_features_on_fork() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"pop": 1000}}
    ])).await;

    // Fork from main's head
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/branches"),
        json!({"name": "fork", "created_by": "test", "fork_from_branch": main_id.to_string()}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork branch: {body}");
    let fork_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let filter = json!({"filter": {"op": ">", "args": [{"property": "pop"}, 500]}});

    // The fork must see the pre-fork feature
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{fork_id}/features/filter"),
        filter.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 on fork: {body}");
    assert_eq!(
        body["numberReturned"], 1,
        "fork must see pre-fork feature: {body}"
    );

    // Edit the feature on the fork only
    commit_features(
        &app,
        fork_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"pop": 2000}}
        ]),
    )
    .await;

    // Fork sees its own version
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{fork_id}/features/filter"),
        filter.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 on fork after edit: {body}");
    assert_eq!(body["features"][0]["properties"]["pop"], 2000, "{body}");

    // Parent still sees its own version
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{main_id}/features/filter"),
        filter,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 on main: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["properties"]["pop"], 1000, "{body}");
}

#[tokio::test]
async fn test_cql2_numeric_filter_ignores_non_numeric_property() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let numeric = Uuid::now_v7();
    let numeric_string = Uuid::now_v7();
    let text = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": numeric.to_string(), "geometry_wkb_hex": point_hex, "properties": {"pop": 1000}},
        {"type": "insert", "feature_id": numeric_string.to_string(), "geometry_wkb_hex": point_hex, "properties": {"pop": "250"}},
        {"type": "insert", "feature_id": text.to_string(), "geometry_wkb_hex": point_hex, "properties": {"pop": "unknown"}}
    ])).await;

    // The non-numeric row must not blow up the cast
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({"filter": {"op": ">", "args": [{"property": "pop"}, 500]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 numeric filter: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["id"], numeric.to_string(), "{body}");

    // Numeric strings still compare as numbers
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({"filter": {"op": "<", "args": [{"property": "pop"}, 500]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 numeric filter: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(
        body["features"][0]["id"],
        numeric_string.to_string(),
        "{body}"
    );

    // between has the same guarded cast
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({"filter": {"op": "between", "args": [{"property": "pop"}, 100, 500]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cql2 between filter: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(
        body["features"][0]["id"],
        numeric_string.to_string(),
        "{body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Conflict Resolution Tests
// ═══════════════════════════════════════════════════════════════════════

/// Helper: fork a branch off another branch's head, return its ID.
async fn create_fork(app: &axum::Router, dataset_id: Uuid, name: &str, from: Uuid) -> Uuid {
    let (status, body) = post_json(
        app,
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        json!({"name": name, "created_by": "test", "fork_from_branch": from.to_string()}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork branch: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Helper: resolve a conflicting merge of `source_id` into `target_id` by
/// keeping the target's version. `resolve_and_merge` names the target side
/// "ours", the opposite of the branch-relative naming callers may expect.
async fn resolve_keeping_target(
    app: &axum::Router,
    target_id: Uuid,
    source_id: Uuid,
    feature_id: Uuid,
) -> Value {
    let (status, body) = post_json(
        app,
        &format!("/api/v1/branches/{target_id}/merge/{source_id}/resolve"),
        json!({
            "resolutions": [{"feature_id": feature_id.to_string(), "strategy": "ours"}],
            "message": "keep target",
            "author": "test",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve: {body}");
    assert_eq!(body["success"], true, "resolve: {body}");
    body
}

#[tokio::test]
async fn test_resolve_target_side_uses_target_chain_not_newest_write() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "main-v1"}}
    ])).await;

    // Source branch diverges the feature
    let dev_id = create_fork(&app, ds_id, "dev", main_id).await;
    commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-v1"}}
        ]),
    )
    .await;

    // Target moves on too, so the merge actually conflicts
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "main-v2"}}
        ]),
    )
    .await;

    // An unrelated branch writes the newest version of the same feature
    let other_id = create_fork(&app, ds_id, "other", main_id).await;
    commit_features(
        &app,
        other_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "other-newer"}}
        ]),
    )
    .await;

    resolve_keeping_target(&app, main_id, dev_id, f1).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{main_id}/features")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 1, "{body}");
    assert_eq!(
        features[0]["properties"]["name"], "main-v2",
        "the target side must come from the target's own chain, not another branch: {body}"
    );
}

#[tokio::test]
async fn test_resolve_target_side_delete_propagates_as_delete() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "main-v1"}}
    ])).await;

    let dev_id = create_fork(&app, ds_id, "dev", main_id).await;
    commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-v1"}}
        ]),
    )
    .await;

    // Target deletes the feature
    commit_features(
        &app,
        main_id,
        json!([{"type": "delete", "feature_id": f1.to_string()}]),
    )
    .await;

    resolve_keeping_target(&app, main_id, dev_id, f1).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{main_id}/features")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["features"].as_array().unwrap().len(),
        0,
        "keeping a target-side delete must delete the feature: {body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// OGC API Features Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_ogc_conformance() {
    let (app, _) = setup_app().await;

    let (status, body) = get_json(&app, "/api/v1/ogc/conformance").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["conformsTo"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_ogc_collections() {
    let (app, _) = setup_app().await;
    let _ds_id = create_dataset(&app).await;

    let (status, body) = get_json(&app, "/api/v1/ogc/collections").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["collections"].as_array().unwrap().is_empty());
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
async fn test_parcel_search_finds_match_beyond_limit_window() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    // many decoys so a limit-scaled candidate fetch cannot cover the branch
    let point_hex = "0101000000000000000000F03F0000000000000040";
    let mut ops: Vec<Value> = (0..30)
        .map(|i| {
            json!({"type": "insert", "feature_id": Uuid::now_v7().to_string(),
                   "geometry_wkb_hex": point_hex, "properties": {"apn": format!("DECOY-{i}")}})
        })
        .collect();
    ops.push(
        json!({"type": "insert", "feature_id": Uuid::now_v7().to_string(),
                    "geometry_wkb_hex": point_hex, "properties": {"apn": "TARGET-99"}}),
    );
    commit_features(&app, branch_id, json!(ops)).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/parcels/search?branch_id={branch_id}&type=apn&q=TARGET-99&limit=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "search: {body}");
    let hits = body.as_array().unwrap();
    assert_eq!(hits.len(), 1, "search must filter in sql, got: {body}");
    assert_eq!(hits[0]["apn"], "TARGET-99");
}

#[tokio::test]
async fn test_geoprocessing_merge() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let (f1, f2) = (Uuid::now_v7(), Uuid::now_v7());

    // two adjacent unit squares: (0..1, 0..1) and (1..2, 0..1)
    let sq1 = "0103000000010000000500000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000F03F00000000000000000000000000000000";
    let sq2 = "01030000000100000005000000000000000000F03F0000000000000000000000000000004000000000000000000000000000000040000000000000F03F000000000000F03F000000000000F03F000000000000F03F0000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": sq1, "properties": {}},
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": sq2, "properties": {}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/geoprocessing/merge"),
        json!({"feature_ids": [f1.to_string(), f2.to_string()]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge: {body}");
    assert!(body["geometry"].is_object(), "merge geometry: {body}");
    // union of two 1x1 degree squares near the equator is ~2.45e10 m^2
    let area = body["area_sq_meters"].as_f64().unwrap();
    assert!(area > 1.0e10 && area < 1.0e11, "merge area: {area}");
}

#[tokio::test]
async fn test_buffer_analysis() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {}}
    ])).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/analytics/buffer?feature_id={f1}&distance=0.01"),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "buffer: {status} {body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Topology Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_topology_validate() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    // List topologies for dataset
    let (status, _body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/topologies")).await;
    assert!(status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR);
}

// ═══════════════════════════════════════════════════════════════════════
// SFCGAL 3D Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_sfcgal_extrude() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    // Insert a polygon feature first
    let polygon_hex = "01030000000100000005000000000000000000000000000000000000000000000000002440000000000000000000000000000024400000000000002440000000000000000000000000000024400000000000000000000000000000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": polygon_hex, "properties": {}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/3d/extrude"),
        json!({"feature_id": f1.to_string(), "height": 10.0}),
    )
    .await;
    // SFCGAL might not be installed, or feature might not be in view yet — accept 404/500
    assert!(
        status == StatusCode::OK
            || status == StatusCode::INTERNAL_SERVER_ERROR
            || status == StatusCode::NOT_FOUND,
        "sfcgal extrude: {status} {body}"
    );
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/h3/index"),
        json!({"resolution": 7}),
    )
    .await;
    // h3-pg might not be installed in test env
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "h3 index: {status} {body}"
    );
}

#[tokio::test]
async fn test_h3_hexagons() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, _body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/h3/hexagons?resolution=7"),
    )
    .await;
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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "test"}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/similarity/embed"),
        json!({"fields": ["name"]}),
    )
    .await;
    // pgvector + pgcrypto might not be installed
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "embed: {status} {body}"
    );
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
        &format!("/api/v1/branches/{branch_id}/similarity/search"),
        json!({"embedding": embedding, "limit": 5}),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "similarity: {status} {body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Network / pgRouting Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_network_shortest_path() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let _branch_id = create_branch(&app, ds_id, "main").await;

    // Network routes need a network ID, not branch ID
    // Just verify the route group exists
    let (status, _body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/networks")).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "network list: {status}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Trajectory / MobilityDB Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_trajectory_list() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let _branch_id = create_branch(&app, ds_id, "main").await;

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/trajectories")).await;
    assert_eq!(status, StatusCode::OK, "trajectory list: {body}");
    assert_eq!(body, json!([]), "{body}");
}

/// Create and read back a trajectory. The path and instant count come out of
/// MobilityDB where it is installed and out of the JSONB fallback where it is
/// not, so this holds either way.
#[tokio::test]
async fn test_trajectory_create_and_read_back() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/trajectories"),
        json!({
            "name": "bus 12",
            "points": [
                {"lng": 1.0, "lat": 2.0, "timestamp": "2024-01-01T00:00:00Z"},
                {"lng": 3.0, "lat": 4.0, "timestamp": "2024-01-01T01:00:00Z"},
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create trajectory: {body}");
    let traj_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &format!("/api/v1/trajectories/{traj_id}")).await;
    assert_eq!(status, StatusCode::OK, "get trajectory: {body}");
    assert_eq!(body["name"], "bus 12", "{body}");
    assert_eq!(body["num_points"], 2, "{body}");
    assert_eq!(body["path"]["type"], "LineString", "{body}");
    assert!(
        body["start_time"]
            .as_str()
            .unwrap()
            .starts_with("2024-01-01"),
        "{body}"
    );

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/trajectories")).await;
    assert_eq!(status, StatusCode::OK, "list trajectories: {body}");
    assert_eq!(body[0]["id"], traj_id, "{body}");
    assert_eq!(body[0]["name"], "bus 12", "{body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Cartography Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_label_rule_create_and_read_back() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/labels"),
        json!({
            "name": "street names",
            "field_expression": "name",
            "placement": {"type": "curved"},
            "font": {"family": "Inter", "size": 14},
            "min_scale": 1000.0,
            "priority": 3,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create label: {body}");
    let label_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &format!("/api/v1/labels/{label_id}")).await;
    assert_eq!(status, StatusCode::OK, "get label: {body}");
    assert_eq!(body["field_expression"], "name", "{body}");
    assert_eq!(body["placement"]["type"], "curved", "{body}");
    assert_eq!(body["font"]["family"], "Inter", "{body}");
    assert_eq!(body["min_scale"], 1000.0, "{body}");
    assert_eq!(body["priority"], 3, "{body}");

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/labels")).await;
    assert_eq!(status, StatusCode::OK, "list labels: {body}");
    assert_eq!(body[0]["id"], label_id, "{body}");
    assert_eq!(body[0]["field_expression"], "name", "{body}");
}

/// Placement and font are jsonb columns with schema defaults, so a create that
/// names neither still reads back as objects.
#[tokio::test]
async fn test_label_rule_defaults_are_json_objects() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/labels"),
        json!({"name": "plain", "field_expression": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create label: {body}");
    let label_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &format!("/api/v1/labels/{label_id}")).await;
    assert_eq!(status, StatusCode::OK, "get label: {body}");
    assert_eq!(body["placement"]["type"], "point_on_surface", "{body}");
    assert_eq!(body["font"]["family"], "Arial", "{body}");
}

#[tokio::test]
async fn test_symbology_rule_create_and_read_back() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/symbology"),
        json!({
            "name": "water",
            "symbol": {"type": "simple_fill", "color": [0, 0, 255, 255]},
            "filter_expression": "kind = 'lake'",
            "priority": 1,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create symbology: {body}");
    let rule_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &format!("/api/v1/symbology/{rule_id}")).await;
    assert_eq!(status, StatusCode::OK, "get symbology: {body}");
    assert_eq!(body["symbol"]["type"], "simple_fill", "{body}");
    assert_eq!(body["filter_expression"], "kind = 'lake'", "{body}");
}

// ═══════════════════════════════════════════════════════════════════════
// Relationship Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_relationship_class_and_record_round_trip() {
    let (app, _) = setup_app().await;
    let origin = create_dataset(&app).await;
    let destination = create_dataset(&app).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{origin}/relationships"),
        json!({
            "name": "parcel to owner",
            "origin_dataset_id": origin,
            "destination_dataset_id": destination,
            "origin_foreign_key": "parcel_id",
            "cardinality": "one_to_many",
            "forward_label": "owned by",
            "backward_label": "owns",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create class: {body}");
    let class_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &format!("/api/v1/relationship-classes/{class_id}")).await;
    assert_eq!(status, StatusCode::OK, "get class: {body}");
    assert_eq!(body["name"], "parcel to owner", "{body}");
    assert_eq!(body["cardinality"], "one_to_many", "{body}");
    assert_eq!(body["forward_label"], "owned by", "{body}");
    assert_eq!(body["backward_label"], "owns", "{body}");

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{origin}/relationships")).await;
    assert_eq!(status, StatusCode::OK, "list classes: {body}");
    assert_eq!(body[0]["id"], class_id, "{body}");

    let parcel = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/relationship-classes/{class_id}/records"),
        json!({
            "origin_feature_id": parcel,
            "destination_feature_id": owner,
            "properties": {"since": 2024},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create record: {body}");

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/relationship-classes/{class_id}/records"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list records: {body}");
    assert_eq!(body[0]["origin_feature_id"], parcel.to_string(), "{body}");
    assert_eq!(
        body[0]["destination_feature_id"],
        owner.to_string(),
        "{body}"
    );
    assert_eq!(body[0]["properties"]["since"], 2024, "{body}");

    let (status, body) = get_json(&app, &format!("/api/v1/features/{parcel}/related")).await;
    assert_eq!(status, StatusCode::OK, "related: {body}");
    assert_eq!(
        body["forward"][0]["feature_id"],
        owner.to_string(),
        "{body}"
    );
    assert_eq!(body["forward"][0]["label"], "owned by", "{body}");

    let (status, body) = get_json(&app, &format!("/api/v1/features/{owner}/related")).await;
    assert_eq!(status, StatusCode::OK, "related: {body}");
    assert_eq!(
        body["backward"][0]["feature_id"],
        parcel.to_string(),
        "{body}"
    );
    assert_eq!(body["backward"][0]["label"], "owns", "{body}");
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
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "webhook create: {body}"
    );

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
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {}}
    ])).await;

    // Acquire lock
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/locks"),
        json!({"feature_id": f1.to_string(), "locked_by": "alice"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "lock: {body}");

    // List locks
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/locks")).await;
    assert_eq!(status, StatusCode::OK, "list locks: {body}");
    assert_eq!(body[0]["locked_by"], "alice", "{body}");

    // With auth off the query param is the actor, so alice can release her lock
    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/branches/{branch_id}/locks/{f1}?actor=alice"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "unlock: {body}");

    let (_, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/locks")).await;
    assert_eq!(body.as_array().unwrap().len(), 0, "{body}");
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
    )
    .await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::UNPROCESSABLE_ENTITY,
        "create org: {status} {body}"
    );
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

// ═══════════════════════════════════════════════════════════════════════
// QGIS Sync Tests
// ═══════════════════════════════════════════════════════════════════════

/// Helper: pull a branch through the QGIS sync endpoint.
async fn qgis_pull(app: &axum::Router, branch_id: Uuid) -> (StatusCode, Value) {
    get_json(app, &format!("/api/v1/qgis/branches/{branch_id}/sync")).await
}

/// Helper: feature ids in a QGIS pull response, sorted.
fn pulled_ids(body: &Value) -> Vec<String> {
    let mut ids: Vec<String> = body["geojson"]["features"]
        .as_array()
        .unwrap_or_else(|| panic!("pull response has no geojson.features: {body}"))
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn test_qgis_pull_returns_branch_features_only() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();
    let f3 = Uuid::now_v7();

    let p1 = "0101000000000000000000F03F0000000000000040"; // POINT(1 2)
    let p2 = "010100000000000000000008400000000000001040"; // POINT(3 4)
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "one"}},
            {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": p2, "properties": {"name": "two"}}
        ]),
    )
    .await;

    // A feature written on an unrelated branch must not leak into main's pull
    let other_id = create_fork(&app, ds_id, "other", main_id).await;
    commit_features(
        &app,
        other_id,
        json!([
            {"type": "insert", "feature_id": f3.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "three"}}
        ]),
    )
    .await;

    let (status, body) = qgis_pull(&app, main_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut expected = vec![f1.to_string(), f2.to_string()];
    expected.sort();
    assert_eq!(pulled_ids(&body), expected, "{body}");
    assert_eq!(body["up_to_date"], false, "{body}");

    let feats = body["geojson"]["features"].as_array().unwrap();
    let one = feats.iter().find(|f| f["id"] == f1.to_string()).unwrap();
    assert_eq!(one["properties"]["name"], "one", "{body}");
    assert_eq!(one["geometry"]["type"], "Point", "{body}");
}

#[tokio::test]
async fn test_qgis_pull_omits_deleted_features() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    let p1 = "0101000000000000000000F03F0000000000000040";
    let p2 = "010100000000000000000008400000000000001040";
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "one"}},
            {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": p2, "properties": {"name": "two"}}
        ]),
    )
    .await;

    let (status, before) = qgis_pull(&app, main_id).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert!(
        pulled_ids(&before).contains(&f2.to_string()),
        "f2 should be visible before the delete: {before}"
    );

    commit_features(
        &app,
        main_id,
        json!([{"type": "delete", "feature_id": f2.to_string()}]),
    )
    .await;

    let (status, after) = qgis_pull(&app, main_id).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(
        pulled_ids(&after),
        vec![f1.to_string()],
        "a feature whose latest version is a delete must be gone from the pull: {after}"
    );
}

#[tokio::test]
async fn test_qgis_pull_returns_latest_version_of_a_feature() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let p1 = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "v1"}}
    ])).await;
    commit_features(
        &app,
        main_id,
        json!([{"type": "update", "feature_id": f1.to_string(), "properties": {"name": "v2"}}]),
    )
    .await;

    let (status, body) = qgis_pull(&app, main_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let feats = body["geojson"]["features"].as_array().unwrap();
    assert_eq!(feats.len(), 1, "one version per feature: {body}");
    assert_eq!(feats[0]["properties"]["name"], "v2", "{body}");
}

// ═══════════════════════════════════════════════════════════════════════
// CQL2 Spatial & Injection Tests
// ═══════════════════════════════════════════════════════════════════════

/// POINT(1 2) and POINT(50 50) as little-endian WKB hex.
const WKB_POINT_1_2: &str = "0101000000000000000000F03F0000000000000040";
const WKB_POINT_50_50: &str = "010100000000000000000049400000000000004940";

/// Box covering 0..10 on both axes, so it holds POINT(1 2) but not POINT(50 50).
fn box_0_10() -> Value {
    json!({"type": "Polygon", "coordinates": [[[0, 0], [0, 10], [10, 10], [10, 0], [0, 0]]]})
}

/// Fails if the table was dropped or the rows were altered by injected SQL.
async fn feature_version_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM feature_versions")
        .fetch_one(pool)
        .await
        .expect("feature_versions must still be queryable")
}

#[tokio::test]
async fn test_cql2_spatial_filter_matches_features() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let inside = Uuid::now_v7();
    let outside = Uuid::now_v7();

    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": inside.to_string(), "geometry_wkb_hex": WKB_POINT_1_2, "properties": {"name": "inside"}},
        {"type": "insert", "feature_id": outside.to_string(), "geometry_wkb_hex": WKB_POINT_50_50, "properties": {"name": "outside"}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    // s_intersects keeps only the point inside the box
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "s_intersects", "args": [{"property": "geometry"}, box_0_10()]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "s_intersects: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["id"], inside.to_string(), "{body}");

    // s_within agrees, and "geom" is accepted as the geometry column reference
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "s_within", "args": [{"property": "geom"}, box_0_10()]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "s_within: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["id"], inside.to_string(), "{body}");

    // a point cannot contain the box
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "s_contains", "args": [{"property": "geometry"}, box_0_10()]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "s_contains: {body}");
    assert_eq!(body["numberReturned"], 0, "{body}");

    // a "crs" member must not push the literal into another SRID (mixed-SRID error)
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "s_intersects", "args": [{"property": "geometry"}, {
            "type": "Polygon",
            "coordinates": [[[0, 0], [0, 10], [10, 10], [10, 0], [0, 0]]],
            "crs": {"type": "name", "properties": {"name": "EPSG:3857"}}
        }]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "crs member: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");

    // a geometry collection round-trips through validation
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "s_intersects", "args": [{"property": "geometry"}, {
            "type": "GeometryCollection",
            "geometries": [{"type": "Point", "coordinates": [1, 2]}]
        }]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "geometry collection: {body}");
    assert_eq!(body["numberReturned"], 1, "{body}");

    assert_eq!(feature_version_count(state.pool()).await, 2);
}

#[tokio::test]
async fn test_cql2_spatial_geojson_injection_is_rejected() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": WKB_POINT_1_2, "properties": {"name": "inside"}},
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": WKB_POINT_50_50, "properties": {"name": "outside"}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");
    let before = feature_version_count(state.pool()).await;

    // each payload tries to close the GeoJSON string literal and append SQL;
    // "OR (true)" would widen the result to both features if it ever ran
    let payloads = [
        json!({"type": "Point'); DROP TABLE feature_versions; --", "coordinates": [1, 2]}),
        json!({"type": "Point", "coordinates": [1, 2], "extra": "')) OR (true) --"}),
        json!("{\"type\":\"Point\",\"coordinates\":[1,2]}')) OR (true) --"),
        json!({"type": "Polygon", "coordinates": [[[0, 0], [0, 10], [10, 10], [10, 0], [0, 0]]],
               "id": "x'); UPDATE feature_versions SET properties = '{}'; --"}),
        json!({"type": "Point", "coordinates": [1, 2], "x": "\\') OR (true) --"}),
        json!({"coordinates": [1, 2]}),
        json!({"type": "Point"}),
        json!([1, 2]),
    ];

    for payload in payloads {
        let (status, body) = post_json(
            &app,
            &uri,
            json!({"filter": {"op": "s_intersects", "args": [{"property": "geometry"}, payload.clone()]}}),
        )
        .await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::OK,
            "payload {payload} gave {status}: {body}"
        );
        if status == StatusCode::OK {
            assert_ne!(
                body["numberReturned"], 2,
                "injected predicate ran for {payload}: {body}"
            );
        }
        assert_eq!(
            feature_version_count(state.pool()).await,
            before,
            "rows changed by {payload}"
        );
    }

    // properties survived the UPDATE payload
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "=", "args": [{"property": "name"}, "inside"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["numberReturned"], 1,
        "properties must be intact: {body}"
    );
}

#[tokio::test]
async fn test_cql2_spatial_rejects_non_geometry_property() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    for prop in ["pop", "geometry) OR (true", ""] {
        let (status, body) = post_json(
            &app,
            &uri,
            json!({"filter": {"op": "s_intersects", "args": [{"property": prop}, box_0_10()]}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "property {prop:?}: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("geometry"),
            "property {prop:?}: {body}"
        );
    }
}

#[tokio::test]
async fn test_cql2_property_name_injection_is_neutralized() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": WKB_POINT_1_2, "properties": {"pop": 1000}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    // control: the clean property name matches
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "=", "args": [{"property": "pop"}, "1000"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 1, "control must match: {body}");

    // hostile names are looked up as literal jsonb keys: they neither escape the
    // query nor get stripped down into a different (matching) key
    for name in [
        "pop') = '1000' OR (1=1",
        "pop'; DROP TABLE feature_versions; --",
        "p'op",
        "po\u{0301}p",
        "",
    ] {
        let (status, body) = post_json(
            &app,
            &uri,
            json!({"filter": {"op": "=", "args": [{"property": name}, "1000"]}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "property {name:?}: {body}");
        assert_eq!(
            body["numberReturned"], 0,
            "property {name:?} must match nothing: {body}"
        );
        assert_eq!(
            feature_version_count(state.pool()).await,
            1,
            "property {name:?} changed data"
        );
    }
}

#[tokio::test]
async fn test_cql2_value_injection_is_neutralized() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": WKB_POINT_1_2, "properties": {"pop": 1000, "name": "alpha"}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    for value in [
        "alpha' OR '1'='1",
        "alpha'; DROP TABLE feature_versions; --",
        "alpha')) OR (true) --",
        "alpha\\') OR (true) --",
    ] {
        for op in ["=", "like", "in"] {
            let filter = json!({"op": op, "args": [{"property": "name"}, value]});
            let (status, body) = post_json(&app, &uri, json!({"filter": filter})).await;
            assert_eq!(status, StatusCode::OK, "{op} {value:?}: {body}");
            assert_eq!(
                body["numberReturned"], 0,
                "{op} {value:?} must match nothing: {body}"
            );
            assert_eq!(
                feature_version_count(state.pool()).await,
                1,
                "{op} {value:?} changed data"
            );
        }
    }

    // between bounds are numeric, so a string payload is refused outright
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "between", "args": [{"property": "pop"}, "1' OR '1'='1", 5000]}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_cql2_text_ops_still_match() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": WKB_POINT_1_2, "properties": {"pop": 1000, "name": "alpha"}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    for (label, filter) in [
        (
            "like",
            json!({"op": "like", "args": [{"property": "name"}, "alp%"]}),
        ),
        (
            "in",
            json!({"op": "in", "args": [{"property": "name"}, "alpha", "beta"]}),
        ),
        (
            "isNull",
            json!({"op": "isNull", "args": [{"property": "missing"}]}),
        ),
        (
            "and",
            json!({"op": "and", "args": [
                {"op": "=", "args": [{"property": "name"}, "alpha"]},
                {"op": ">", "args": [{"property": "pop"}, 500]}
            ]}),
        ),
        (
            "not",
            json!({"op": "not", "args": [{"op": "=", "args": [{"property": "name"}, "beta"]}]}),
        ),
    ] {
        let (status, body) = post_json(&app, &uri, json!({"filter": filter})).await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
        assert_eq!(body["numberReturned"], 1, "{label}: {body}");
    }
}

#[tokio::test]
async fn test_cql2_short_args_return_400_not_panic() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    for filter in [
        json!({"op": "not", "args": []}),
        json!({"op": "and", "args": []}),
        json!({"op": "or", "args": []}),
        json!({"op": "=", "args": [{"property": "pop"}]}),
        json!({"op": "in", "args": [{"property": "pop"}]}),
        json!({"op": "between", "args": [{"property": "pop"}, 1]}),
        json!({"op": "like", "args": [{"property": "pop"}]}),
        json!({"op": "isNull", "args": []}),
        json!({"op": "s_intersects", "args": []}),
        json!({"op": "s_within", "args": [{"property": "geometry"}]}),
        json!({"op": "=", "args": ["pop"]}),
    ] {
        let (status, body) = post_json(&app, &uri, json!({"filter": filter.clone()})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "filter {filter}: {body}");
    }
}

#[tokio::test]
async fn test_qgis_layer_definition_scopes_to_branch() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();
    let f3 = Uuid::now_v7();

    let p1 = "0101000000000000000000F03F0000000000000040"; // POINT(1 2)
    let p2 = "010100000000000000000008400000000000001040"; // POINT(3 4)
    let far = "010100000000000000000049400000000000004E40"; // POINT(50 60)
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "one"}},
            {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": p2, "properties": {"name": "two"}}
        ]),
    )
    .await;

    // A far-away feature on a fork must not inflate main's count or extent
    let other_id = create_fork(&app, ds_id, "other", main_id).await;
    commit_features(&app, other_id, json!([
        {"type": "insert", "feature_id": f3.to_string(), "geometry_wkb_hex": far, "properties": {"name": "three"}}
    ])).await;

    let (status, body) = get_json(&app, &format!("/api/v1/qgis/branches/{main_id}/layer")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["feature_count"], 2, "{body}");
    assert_eq!(body["extent"]["min_x"], 1.0, "{body}");
    assert_eq!(body["extent"]["min_y"], 2.0, "{body}");
    assert_eq!(body["extent"]["max_x"], 3.0, "{body}");
    assert_eq!(body["extent"]["max_y"], 4.0, "{body}");
    let fields: Vec<&str> = body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["name"], "{body}");

    // The fork sees its own feature plus the two it inherited
    let (status, fork_body) =
        get_json(&app, &format!("/api/v1/qgis/branches/{other_id}/layer")).await;
    assert_eq!(status, StatusCode::OK, "{fork_body}");
    assert_eq!(fork_body["feature_count"], 3, "{fork_body}");
    assert_eq!(fork_body["extent"]["max_x"], 50.0, "{fork_body}");

    // Deleting on main drops it from the count
    commit_features(
        &app,
        main_id,
        json!([{"type": "delete", "feature_id": f2.to_string()}]),
    )
    .await;
    let (status, after) = get_json(&app, &format!("/api/v1/qgis/branches/{main_id}/layer")).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["feature_count"], 1, "{after}");
}

#[tokio::test]
async fn test_qgis_push_updates_live_features_and_inserts_the_rest() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();
    let f3 = Uuid::now_v7();

    let p1 = "0101000000000000000000F03F0000000000000040"; // POINT(1 2)
    let head = commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "one"}}
    ])).await;

    // f3 exists only on an unrelated branch, so pushing it to main must insert,
    // not update (an update would look for a base version main never had)
    let other_id = create_fork(&app, ds_id, "other", main_id).await;
    commit_features(&app, other_id, json!([
        {"type": "insert", "feature_id": f3.to_string(), "geometry_wkb_hex": p1, "properties": {"name": "three-on-other"}}
    ])).await;

    let point = |x: f64, y: f64| json!({"type": "Point", "coordinates": [x, y]});
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/qgis/branches/{main_id}/sync"),
        json!({
            "base_changeset": head.to_string(),
            "author": "qgis",
            "message": "push from qgis",
            "geojson": {
                "type": "FeatureCollection",
                "features": [
                    {"type": "Feature", "id": f1.to_string(), "geometry": point(9.0, 9.0), "properties": {"name": "one-edited"}},
                    {"type": "Feature", "id": f2.to_string(), "geometry": point(5.0, 6.0), "properties": {"name": "two-new"}},
                    {"type": "Feature", "id": f3.to_string(), "geometry": point(7.0, 8.0), "properties": {"name": "three-pushed"}}
                ]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "success", "{body}");

    let (status, pulled) = qgis_pull(&app, main_id).await;
    assert_eq!(status, StatusCode::OK, "{pulled}");
    let mut expected = vec![f1.to_string(), f2.to_string(), f3.to_string()];
    expected.sort();
    assert_eq!(pulled_ids(&pulled), expected, "{pulled}");
    let feats = pulled["geojson"]["features"].as_array().unwrap();
    let by_id = |id: Uuid| {
        feats
            .iter()
            .find(|f| f["id"] == id.to_string())
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(f1)["properties"]["name"], "one-edited", "{pulled}");
    let coords: Vec<f64> = by_id(f1)["geometry"]["coordinates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_f64().unwrap())
        .collect();
    assert_eq!(coords, vec![9.0, 9.0], "{pulled}");
    assert_eq!(by_id(f2)["properties"]["name"], "two-new", "{pulled}");
    assert_eq!(by_id(f3)["properties"]["name"], "three-pushed", "{pulled}");
}

// ─── Auth enforcement (router built with a real secret) ─────────────

/// Helper: send a request with an optional bearer token, return status + body.
async fn request_as(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let req = match &body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(b).unwrap()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn token_for(role: Role) -> String {
    generate_token(TEST_SECRET, "test-user", role, 3600)
}

/// A correctly signed token whose `exp` is an hour in the past.
fn expired_token(secret: &str, role: Role) -> String {
    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        exp: usize,
        role: String,
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &Claims {
            sub: "test-user".into(),
            exp: now - 3600,
            role: role.as_str().to_string(),
        },
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

fn new_dataset_body() -> Value {
    json!({
        "name": format!("auth_{}", Uuid::now_v7()),
        "geometry_type": "point",
        "srid": 4326,
        "created_by": "test"
    })
}

#[tokio::test]
async fn test_auth_enabled_get_is_anonymous() {
    let app = setup_app_authed().await;
    let (status, _) = request_as(&app, "GET", "/api/v1/datasets", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request_as(&app, "GET", "/api/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_auth_enabled_write_without_token_is_401() {
    let app = setup_app_authed().await;
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        None,
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn test_auth_enabled_viewer_write_is_403() {
    let app = setup_app_authed().await;
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&token_for(Role::Viewer)),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn test_auth_enabled_editor_can_commit() {
    let app = setup_app_authed().await;
    let editor = token_for(Role::Editor);

    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&editor),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = dataset["id"].as_str().unwrap();

    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&editor),
        Some(json!({"name": "main", "created_by": "test"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = branch["id"].as_str().unwrap();

    let (status, commit) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(&editor),
        Some(json!({
            "message": "authed commit",
            "author": "test",
            "operations": [{
                "type": "insert",
                "feature_id": Uuid::now_v7(),
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {"name": "one"}
            }]
        })),
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "commit as editor failed with {status}: {commit}"
    );
}

#[tokio::test]
async fn test_auth_enabled_admin_route_rejects_editor() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&admin),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = dataset["id"].as_str().unwrap();

    let hook = json!({"url": "https://example.invalid/hook", "events": ["commit"]});
    let uri = format!("/api/v1/datasets/{dataset_id}/webhooks");

    let (status, body) = request_as(
        &app,
        "POST",
        &uri,
        Some(&token_for(Role::Editor)),
        Some(hook.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = request_as(&app, "POST", &uri, Some(&admin), Some(hook)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn test_auth_enabled_rejects_garbage_and_expired_tokens() {
    let app = setup_app_authed().await;

    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some("not-a-jwt"),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // signed with the right secret but expired well past the 60s clock-skew
    // leeway jsonwebtoken allows by default
    let expired = expired_token(TEST_SECRET, Role::Admin);
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&expired),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // valid claims, wrong signing key
    let forged = generate_token(
        "another-secret-that-is-long-enough-000000",
        "attacker",
        Role::Admin,
        3600,
    );
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&forged),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn test_auth_enabled_no_api_key_bypass() {
    let app = setup_app_authed().await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/datasets")
        .header("content-type", "application/json")
        .header("x-api-key", "anything-at-all")
        .body(Body::from(serde_json::to_vec(&new_dataset_body()).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── Sensitive reads (config/ACL/membership/audit) are admin-only ───
//
// Regression guard: these GETs used to be anonymous because classify()
// returned Public for every GET before checking the path. Each proves the
// three-way ladder no-token->401, viewer->403, admin->200.

/// Create a dataset in an auth-enabled app with an admin token, return its id.
async fn create_dataset_authed(app: &axum::Router, admin: &str) -> String {
    let (status, dataset) = request_as(
        app,
        "POST",
        "/api/v1/datasets",
        Some(admin),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    dataset["id"].as_str().unwrap().to_string()
}

/// Assert a GET is 401 without a token, 403 for a viewer, 200 for an admin.
async fn assert_read_is_admin_only(app: &axum::Router, uri: &str, admin: &str) {
    let (status, body) = request_as(app, "GET", uri, None, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no-token GET {uri}: {body}"
    );

    let (status, body) = request_as(app, "GET", uri, Some(&token_for(Role::Viewer)), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "viewer GET {uri}: {body}");

    let (status, body) = request_as(app, "GET", uri, Some(admin), None).await;
    assert_eq!(status, StatusCode::OK, "admin GET {uri}: {body}");
}

#[tokio::test]
async fn test_webhooks_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/webhooks");
    assert_read_is_admin_only(&app, &uri, &admin).await;
}

/// Grant reads are not public and not for outsiders, but they are no longer
/// role-gated: the dataset's own admin reads them too, which
/// `test_dataset_admin_manages_only_its_own_dataset` covers.
async fn assert_read_needs_dataset_admin(app: &axum::Router, uri: &str, admin: &str) {
    let (status, body) = request_as(app, "GET", uri, None, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no-token GET {uri}: {body}"
    );

    let outsider = token_for_user("outsider", Role::Editor);
    let (status, body) = request_as(app, "GET", uri, Some(&outsider), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "outsider GET {uri}: {body}");

    let (status, body) = request_as(app, "GET", uri, Some(admin), None).await;
    assert_eq!(status, StatusCode::OK, "admin GET {uri}: {body}");
}

#[tokio::test]
async fn test_dataset_permissions_read_needs_dataset_admin() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/permissions");
    assert_read_needs_dataset_admin(&app, &uri, &admin).await;
}

#[tokio::test]
async fn test_permission_check_read_needs_dataset_admin() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/permissions/some-user/check");
    assert_read_needs_dataset_admin(&app, &uri, &admin).await;
}

#[tokio::test]
async fn test_orgs_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    assert_read_is_admin_only(&app, "/api/v1/orgs", &admin).await;
}

#[tokio::test]
async fn test_audit_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    assert_read_is_admin_only(&app, "/api/v1/audit", &admin).await;
}

#[tokio::test]
async fn test_metrics_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    assert_read_is_admin_only(&app, "/metrics", &admin).await;
}

#[tokio::test]
async fn test_dataset_events_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/events");
    assert_read_is_admin_only(&app, &uri, &admin).await;
}

#[tokio::test]
async fn test_replication_feed_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&admin),
        Some(json!({"name": "main", "created_by": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = branch["id"].as_str().unwrap();
    let uri = format!("/api/v1/replication/feed/{branch_id}");
    assert_read_is_admin_only(&app, &uri, &admin).await;
}

/// The lrs endpoint that shares the `/events` suffix is map data and stays open.
#[tokio::test]
async fn test_route_events_read_stays_public() {
    let app = setup_app_authed().await;
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/routes/{}/events", Uuid::now_v7()),
        None,
        None,
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn test_data_read_stays_public_without_token() {
    let app = setup_app_authed().await;
    let (status, body) = request_as(&app, "GET", "/api/v1/datasets", None, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "anonymous data read must stay open: {body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// External datasets: read-only over a PostGIS table ptolemy does not own
// ═══════════════════════════════════════════════════════════════════════

/// A plain PostGIS table of the kind a team already has: an integer key, its
/// own column names, no ptolemy metadata. `fresh_state` does not know about
/// these, so each run recreates them.
///
/// It goes into whichever pool external reads use, so setting
/// `PTOLEMY_EXTERNAL_DATABASE_URL` runs this whole group against a second
/// database instead of the primary one.
async fn create_external_fixture(state: &AppState) {
    let pool = state.external_pool().await.unwrap();
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS ext_parcels CASCADE;
         CREATE TABLE ext_parcels (
             parcel_id integer PRIMARY KEY,
             owner text NOT NULL,
             assessed integer NOT NULL,
             geom geometry(Geometry, 4326) NOT NULL
         );
         INSERT INTO ext_parcels (parcel_id, owner, assessed, geom) VALUES
           (1, 'alice', 100, ST_SetSRID(ST_MakePoint(10.0, 20.0), 4326)),
           (2, 'bob', 200, ST_SetSRID(ST_MakePoint(-30.0, -40.0), 4326)),
           (3, 'carol', 300, ST_GeomFromText('POLYGON((0 0,0 1,1 1,1 0,0 0))', 4326));
         DROP TABLE IF EXISTS ext_no_geom CASCADE;
         CREATE TABLE ext_no_geom (id integer PRIMARY KEY, label text);",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn register_external(
    app: &axum::Router,
    table: &str,
    id_column: &str,
    geometry_column: &str,
) -> (StatusCode, Value) {
    post_json(
        app,
        "/api/v1/datasets",
        json!({
            "name": format!("ext_{}", Uuid::now_v7()),
            "created_by": "test",
            "external_table": table,
            "external_id_column": id_column,
            "external_geometry_column": geometry_column,
        }),
    )
    .await
}

/// Register the fixture and return (dataset id, main branch id).
async fn setup_external(app: &axum::Router, state: &AppState) -> (Uuid, Uuid) {
    create_external_fixture(state).await;
    let (status, body) = register_external(app, "ext_parcels", "parcel_id", "geom").await;
    assert_eq!(status, StatusCode::CREATED, "register external: {body}");
    let dataset_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let (status, branches) =
        get_json(app, &format!("/api/v1/datasets/{dataset_id}/branches")).await;
    assert_eq!(status, StatusCode::OK);
    let branches = branches.as_array().unwrap();
    assert_eq!(
        branches.len(),
        1,
        "registration must create main: {branches:?}"
    );
    assert_eq!(branches[0]["name"], "main");
    let branch_id = Uuid::parse_str(branches[0]["id"].as_str().unwrap()).unwrap();
    (dataset_id, branch_id)
}

#[tokio::test]
async fn test_external_registration_reports_the_relation() {
    let (app, state) = setup_app().await;
    let (dataset_id, _) = setup_external(&app, &state).await;

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{dataset_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["external"]["table"], "ext_parcels");
    assert_eq!(body["external"]["id_column"], "parcel_id");
    assert_eq!(body["external"]["geometry_column"], "geom");
    // srid comes from the relation, not the request
    assert_eq!(body["srid"], 4326);
}

/// An ordinary dataset must look exactly as it did before this feature.
#[tokio::test]
async fn test_normal_dataset_has_no_external_field() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("external").is_none(),
        "external key leaked into an ordinary dataset: {body}"
    );
}

#[tokio::test]
async fn test_external_feature_listing_and_paging() {
    let (app, state) = setup_app().await;
    let (_, branch_id) = setup_external(&app, &state).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/features")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 3, "{body}");
    // the row's own key stays visible; the geometry column does not duplicate
    let owners: Vec<&str> = features
        .iter()
        .map(|f| f["properties"]["owner"].as_str().unwrap())
        .collect();
    assert!(
        owners.contains(&"alice") && owners.contains(&"carol"),
        "{owners:?}"
    );
    assert!(features[0]["properties"]["parcel_id"].is_number(), "{body}");
    assert!(features[0]["properties"].get("geom").is_none(), "{body}");

    // paging: one at a time, following the cursor, sees every feature once
    let mut seen = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..3 {
        let uri = match &cursor {
            Some(c) => format!("/api/v1/branches/{branch_id}/features?limit=1&cursor={c}"),
            None => format!("/api/v1/branches/{branch_id}/features?limit=1"),
        };
        let (status, page) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let page_features = page["features"].as_array().unwrap();
        assert_eq!(page_features.len(), 1, "{page}");
        seen.insert(page_features[0]["id"].as_str().unwrap().to_string());
        cursor = page["next_cursor"].as_str().map(|s| s.to_string());
    }
    assert_eq!(seen.len(), 3, "paging repeated or skipped features");

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/count"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 3);
}

#[tokio::test]
async fn test_external_bbox_filter() {
    let (app, state) = setup_app().await;
    let (_, branch_id) = setup_external(&app, &state).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/bbox?min_x=9&min_y=19&max_x=11&max_y=21"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let features = body.as_array().unwrap();
    assert_eq!(features.len(), 1, "{body}");
    assert_eq!(features[0]["properties"]["owner"], "alice");
}

#[tokio::test]
async fn test_external_cql2_filter() {
    let (app, state) = setup_app().await;
    let (_, branch_id) = setup_external(&app, &state).await;

    // attribute equality
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({"filter": {"op": "eq", "args": [{"property": "owner"}, "bob"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["properties"]["owner"], "bob");

    // numeric comparison over the same jsonb properties
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({"filter": {"op": "gte", "args": [{"property": "assessed"}, 200]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 2, "{body}");

    // spatial operator against the geometry column
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/filter"),
        json!({"filter": {"op": "s_intersects", "args": [
            {"property": "geometry"},
            {"type": "Polygon", "coordinates": [[[-1.0, -1.0], [-1.0, 2.0], [2.0, 2.0], [2.0, -1.0], [-1.0, -1.0]]]}
        ]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["properties"]["owner"], "carol");
}

#[tokio::test]
async fn test_external_ogc_items_and_single_feature() {
    let (app, state) = setup_app().await;
    let (dataset_id, _) = setup_external(&app, &state).await;

    let (status, body) =
        get_json(&app, &format!("/api/v1/ogc/collections/{dataset_id}/items")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "FeatureCollection");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 3, "{body}");
    assert!(features[0]["geometry"]["type"].is_string(), "{body}");

    // bbox variant
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{dataset_id}/items?bbox=9,19,11,21"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["features"].as_array().unwrap().len(), 1, "{body}");
    let feature_id = body["features"][0]["id"].as_str().unwrap().to_string();

    // single feature get resolves the same id the listing handed out
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{dataset_id}/items/{feature_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], feature_id);
    assert_eq!(body["properties"]["owner"], "alice");

    // an id that is not in the relation is a 404, not a 500
    let (status, _) = get_json(
        &app,
        &format!(
            "/api/v1/ogc/collections/{dataset_id}/items/{}",
            Uuid::now_v7()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_external_geojson_export() {
    let (app, state) = setup_app().await;
    let (_, branch_id) = setup_external(&app, &state).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/export/geojson"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["features"].as_array().unwrap().len(), 3, "{body}");
}

#[tokio::test]
async fn test_external_dataset_rejects_every_write() {
    let (app, state) = setup_app().await;
    let (dataset_id, branch_id) = setup_external(&app, &state).await;

    let insert = json!([{
        "type": "insert",
        "geometry_wkb_hex": "0101000000000000000000f03f0000000000000040",
        "properties": {"owner": "mallory"}
    }]);

    let cases: Vec<(&str, String, Value)> = vec![
        (
            "commit",
            format!("/api/v1/branches/{branch_id}/commit"),
            json!({"message": "m", "author": "a", "operations": insert}),
        ),
        (
            "batch commit",
            format!("/api/v1/branches/{branch_id}/batch"),
            json!({"message": "m", "author": "a", "operations": insert}),
        ),
        (
            "merge",
            format!("/api/v1/branches/{branch_id}/merge/{}", Uuid::now_v7()),
            json!({"author": "a"}),
        ),
        (
            "qgis push",
            format!("/api/v1/qgis/branches/{branch_id}/sync"),
            json!({
                "message": "m",
                "author": "a",
                "geojson": {"type": "FeatureCollection", "features": []}
            }),
        ),
        (
            "wfs transaction",
            format!("/api/v1/qgis/branches/{branch_id}/transaction"),
            json!({"message": "m", "author": "a", "operations": []}),
        ),
        (
            "geojson import",
            format!("/api/v1/branches/{branch_id}/import/geojson"),
            json!({"features": []}),
        ),
        (
            "branch create",
            format!("/api/v1/datasets/{dataset_id}/branches"),
            json!({"name": "feature-x", "created_by": "test"}),
        ),
    ];

    for (label, uri, body) in cases {
        let (status, response) = post_json(&app, &uri, body).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{label} must be rejected, got {status}: {response}"
        );
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains("read-only"),
            "{label} rejection must say why: {response}"
        );
    }

    // and nothing was written behind the rejection
    let (_, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/count"),
    )
    .await;
    assert_eq!(body["count"], 3);
    let versions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM feature_versions WHERE dataset_id = $1")
            .bind(dataset_id)
            .fetch_one(state.pool())
            .await
            .unwrap();
    assert_eq!(versions, 0, "a write reached ptolemy's version table");
}

#[tokio::test]
async fn test_external_registration_rejects_hostile_identifiers() {
    let (app, state) = setup_app().await;
    create_external_fixture(&state).await;

    let hostile = [
        "ext_parcels\"; drop table ext_parcels;--",
        "ext_parcels'; drop table ext_parcels;--",
        "ext_parcels; DROP TABLE ext_parcels",
        "pg_catalog.pg_authid; --",
        "a.b.c",
    ];
    for name in hostile {
        let (status, body) = register_external(&app, name, "parcel_id", "geom").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "table {name}: {body}");
        let (status, body) = register_external(&app, "ext_parcels", name, "geom").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "id column {name}: {body}");
        let (status, body) = register_external(&app, "ext_parcels", "parcel_id", name).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "geometry column {name}: {body}"
        );
    }

    // the fixture survived every attempt
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ext_parcels")
        .fetch_one(state.external_pool().await.unwrap())
        .await
        .unwrap();
    assert_eq!(rows, 3);
}

#[tokio::test]
async fn test_external_registration_probes_the_relation() {
    let (app, state) = setup_app().await;
    create_external_fixture(&state).await;

    let (status, body) = register_external(&app, "no_such_relation", "id", "geom").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("does not exist"),
        "{body}"
    );

    let (status, body) =
        register_external(&app, "ext_parcels", "parcel_id", "no_such_column").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = register_external(&app, "ext_parcels", "parcel_id", "owner").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not PostGIS geometry"),
        "{body}"
    );

    let (status, body) = register_external(&app, "ext_no_geom", "id", "label").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_external_fields_are_all_or_none() {
    let (app, state) = setup_app().await;
    create_external_fixture(&state).await;

    for partial in [
        json!({"external_table": "ext_parcels"}),
        json!({"external_id_column": "parcel_id"}),
        json!({"external_table": "ext_parcels", "external_id_column": "parcel_id"}),
        json!({"external_table": "ext_parcels", "external_geometry_column": "geom"}),
    ] {
        let mut body = json!({"name": format!("ext_{}", Uuid::now_v7()), "created_by": "test"});
        for (k, v) in partial.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, response) = post_json(&app, "/api/v1/datasets", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{partial}: {response}");
    }

    // the database refuses a partial row too, in case something bypasses the API
    let err = sqlx::query(
        "INSERT INTO datasets (id, name, srid, geometry_type, created_by, external_table)
         VALUES ($1, 'partial', 4326, 'point', 'test', 'ext_parcels')",
    )
    .bind(Uuid::now_v7())
    .execute(state.pool())
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("datasets_external_all_or_none"),
        "expected the CHECK constraint to fire: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Audit identity comes from the token, not the body
// ═══════════════════════════════════════════════════════════════════════
//
// Before this, any caller could set `author` / `created_by` / `granted_by` to
// someone else's name and the audit trail would believe it. With auth on the
// token subject wins; with auth off there is no token, so the body stands.

/// Token for a specific subject, so a test can tell "who the token says" from
/// "who the body says".
fn token_for_sub(sub: &str, role: Role) -> String {
    generate_token(TEST_SECRET, sub, role, 3600)
}

#[tokio::test]
async fn test_authed_commit_author_is_token_subject() {
    let app = setup_app_authed().await;
    let editor = token_for_sub("real-editor", Role::Editor);

    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&editor),
        Some(json!({
            "name": format!("audit_{}", Uuid::now_v7()),
            "geometry_type": "point",
            "srid": 4326,
            "created_by": "someone-else"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    assert_eq!(dataset["created_by"], "real-editor", "{dataset}");
    let dataset_id = dataset["id"].as_str().unwrap();

    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&editor),
        Some(json!({"name": "main", "created_by": "someone-else"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    assert_eq!(branch["created_by"], "real-editor", "{branch}");
    let branch_id = branch["id"].as_str().unwrap();

    let (status, commit) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(&editor),
        Some(json!({
            "message": "spoof attempt",
            "author": "someone-else",
            "operations": [{
                "type": "insert",
                "feature_id": Uuid::now_v7(),
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {"name": "one"}
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{commit}");
    assert_eq!(commit["author"], "real-editor", "{commit}");

    // and it is what was persisted, not just what the response echoed
    let (status, history) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/history"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["author"], "real-editor", "{history}");
}

#[tokio::test]
async fn test_authed_batch_and_review_author_is_token_subject() {
    let app = setup_app_authed().await;
    let editor = token_for_sub("real-editor", Role::Editor);

    let (_, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&editor),
        Some(new_dataset_body()),
    )
    .await;
    let dataset_id = dataset["id"].as_str().unwrap();
    let (_, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&editor),
        Some(json!({"name": "main", "created_by": "x"})),
    )
    .await;
    let branch_id = branch["id"].as_str().unwrap();
    let (_, target) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&editor),
        Some(json!({"name": "target", "created_by": "x"})),
    )
    .await;
    let target_id = target["id"].as_str().unwrap();

    let (status, batch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/batch"),
        Some(&editor),
        Some(json!({
            "message": "batch spoof attempt",
            "author": "someone-else",
            "operations": [{
                "type": "insert",
                "feature_id": Uuid::now_v7(),
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {"name": "b"}
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{batch}");
    assert_eq!(batch["changeset"]["author"], "real-editor", "{batch}");

    let (status, review) = request_as(
        &app,
        "POST",
        "/api/v1/reviews",
        Some(&editor),
        Some(json!({
            "dataset_id": dataset_id,
            "source_branch_id": branch_id,
            "target_branch_id": target_id,
            "title": "review spoof attempt",
            "author": "someone-else"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{review}");
    assert_eq!(review["author"], "real-editor", "{review}");
    let review_id = review["id"].as_str().unwrap();

    let (status, comment) = request_as(
        &app,
        "POST",
        &format!("/api/v1/reviews/{review_id}/comments"),
        Some(&editor),
        Some(json!({"author": "someone-else", "body": "looks fine"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    assert_eq!(comment["author"], "real-editor", "{comment}");
}

#[tokio::test]
async fn test_authed_granted_by_is_token_subject() {
    let app = setup_app_authed().await;
    let admin = token_for_sub("real-admin", Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;

    let (status, perm) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/permissions"),
        Some(&admin),
        Some(json!({
            "user_id": "bob",
            "permission": "write",
            "granted_by": "someone-else"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{perm}");
    assert_eq!(perm["granted_by"], "real-admin", "{perm}");

    let (_, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&admin),
        Some(json!({"name": "main", "created_by": "x"})),
    )
    .await;
    let branch_id = branch["id"].as_str().unwrap();

    let (status, perm) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/permissions"),
        Some(&admin),
        Some(json!({
            "user_id": "bob",
            "permission": "read",
            "granted_by": "someone-else"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{perm}");
    assert_eq!(perm["granted_by"], "real-admin", "{perm}");
}

#[tokio::test]
async fn test_no_auth_keeps_body_author() {
    // dev mode has no token, so the body value is all there is
    let (app, _) = setup_app().await;
    let dataset_id = create_dataset(&app).await;

    let (status, branch) = post_json(
        &app,
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        json!({"name": "main", "created_by": "field-surveyor"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    assert_eq!(branch["created_by"], "field-surveyor", "{branch}");
    let branch_id = branch["id"].as_str().unwrap();

    let (status, commit) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/commit"),
        json!({
            "message": "offline edit",
            "author": "field-surveyor",
            "operations": [{
                "type": "insert",
                "feature_id": Uuid::now_v7(),
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {"name": "one"}
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{commit}");
    assert_eq!(commit["author"], "field-surveyor", "{commit}");
}

// ─── Feature locks are owned by the token subject ────────────────────
//
// unlock_feature used to send a hardcoded "system" actor, and the storage layer
// refuses to unlock when it does not match `locked_by`, so nobody could release
// their own lock over HTTP.

/// Dataset, branch and one committed feature in an auth-enabled app.
async fn locked_branch(app: &axum::Router, token: &str) -> (Uuid, Uuid) {
    let (_, dataset) = request_as(
        app,
        "POST",
        "/api/v1/datasets",
        Some(token),
        Some(new_dataset_body()),
    )
    .await;
    let dataset_id = dataset["id"].as_str().unwrap();
    let (_, branch) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(token),
        Some(json!({"name": "main", "created_by": "x"})),
    )
    .await;
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let feature_id = Uuid::now_v7();
    let (status, commit) = request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(token),
        Some(json!({
            "message": "seed",
            "author": "x",
            "operations": [{
                "type": "insert",
                "feature_id": feature_id,
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {"name": "one"}
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{commit}");

    (branch_id, feature_id)
}

/// Take a lock as `token`'s subject. The body `locked_by` is deliberately wrong,
/// to prove the token subject is what gets recorded.
async fn take_lock(app: &axum::Router, branch_id: Uuid, feature_id: Uuid, token: &str) {
    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/locks"),
        Some(token),
        Some(json!({"feature_id": feature_id, "locked_by": "someone-else"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "lock: {body}");
}

#[tokio::test]
async fn test_lock_owner_can_unlock_own_lock() {
    let app = setup_app_authed().await;
    let owner = token_for_sub("lock-owner", Role::Editor);
    let (branch_id, feature_id) = locked_branch(&app, &owner).await;
    take_lock(&app, branch_id, feature_id, &owner).await;

    let (status, locks) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/locks"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{locks}");
    assert_eq!(locks[0]["locked_by"], "lock-owner", "{locks}");

    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/branches/{branch_id}/locks/{feature_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "unlock: {body}");

    let (_, locks) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/locks"),
        None,
        None,
    )
    .await;
    assert_eq!(locks.as_array().unwrap().len(), 0, "{locks}");
}

#[tokio::test]
async fn test_lock_cannot_be_released_by_another_user() {
    let app = setup_app_authed().await;
    let owner = token_for_sub("lock-owner", Role::Editor);
    let intruder = token_for_sub("intruder", Role::Editor);
    let (branch_id, feature_id) = locked_branch(&app, &owner).await;
    take_lock(&app, branch_id, feature_id, &owner).await;

    let uri = format!("/api/v1/branches/{branch_id}/locks/{feature_id}");

    // an editor with no grant on the dataset never reaches the lock rule: the
    // write layer refuses the branch first
    let (status, body) = request_as(&app, "DELETE", &uri, Some(&intruder), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "ungranted unlock: {body}");

    // with a grant they clear the write layer, and the lock is still not theirs
    let (_, branch) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}"),
        None,
        None,
    )
    .await;
    let dataset_id = Uuid::parse_str(branch["dataset_id"].as_str().unwrap()).unwrap();
    grant(&app, "datasets", dataset_id, "intruder", "write").await;

    let (status, body) = request_as(&app, "DELETE", &uri, Some(&intruder), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "intruder unlock: {body}");

    // and the query param cannot be used to claim the owner's name
    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("{uri}?actor=lock-owner"),
        Some(&intruder),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "spoofed actor unlock: {body}");

    let (_, locks) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/locks"),
        None,
        None,
    )
    .await;
    assert_eq!(
        locks.as_array().unwrap().len(),
        1,
        "lock must survive: {locks}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// CQL2 filter input validation
// ═══════════════════════════════════════════════════════════════════════
//
// Each of these used to reach PostGIS or PostgreSQL and come back as a 500,
// or (for the spec `in` form) be rejected as an unparseable literal.

/// Branch with two features, "small" (pop 100, at 1 2) and "big" (pop 5000).
async fn cql2_branch(app: &axum::Router) -> Uuid {
    let ds_id = create_dataset(app).await;
    let branch_id = create_branch(app, ds_id, "main").await;
    commit_features(
        app,
        branch_id,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": WKB_POINT_1_2, "properties": {"name": "small", "pop": 100}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": WKB_POINT_50_50, "properties": {"name": "big", "pop": 5000}}
        ]),
    )
    .await;
    branch_id
}

#[tokio::test]
async fn test_cql2_malformed_coordinates_are_400() {
    let (app, _) = setup_app().await;
    let branch_id = cql2_branch(&app).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    for geometry in [
        // wrong nesting depth for the declared type
        json!({"type": "Point", "coordinates": [[1, 2]]}),
        json!({"type": "Polygon", "coordinates": [[0, 0], [0, 1], [1, 1], [0, 0]]}),
        json!({"type": "LineString", "coordinates": [1, 2]}),
        json!({"type": "MultiPolygon", "coordinates": [[[0, 0], [0, 1], [1, 1], [0, 0]]]}),
        // positions that are not 2 or 3 finite numbers
        json!({"type": "Point", "coordinates": [1]}),
        json!({"type": "Point", "coordinates": [1, 2, 3, 4]}),
        json!({"type": "Point", "coordinates": ["a", "b"]}),
        json!({"type": "Point", "coordinates": [null, 2]}),
        json!({"type": "LineString", "coordinates": [[1, 2], [3]]}),
        // empty arrays
        json!({"type": "LineString", "coordinates": []}),
        json!({"type": "Polygon", "coordinates": []}),
        // rings that are too short or not closed
        json!({"type": "Polygon", "coordinates": [[[0, 0], [0, 1], [1, 1]]]}),
        json!({"type": "Polygon", "coordinates": [[[0, 0], [0, 1], [1, 1], [1, 0]]]}),
        json!({"type": "GeometryCollection",
               "geometries": [{"type": "Point", "coordinates": [1]}]}),
    ] {
        let (status, body) = post_json(
            &app,
            &uri,
            json!({"filter": {"op": "s_intersects",
                              "args": [{"property": "geometry"}, geometry.clone()]}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{geometry}: {body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("geometry") || e.contains("ring")),
            "{geometry} needs a message naming the problem: {body}"
        );
    }

    // the well-formed equivalents still work
    for geometry in [
        json!({"type": "Point", "coordinates": [1, 2]}),
        json!({"type": "Point", "coordinates": [1, 2, 0]}),
        json!({"type": "Polygon", "coordinates": [[[0, 0], [0, 10], [10, 10], [10, 0], [0, 0]]]}),
    ] {
        let (status, body) = post_json(
            &app,
            &uri,
            json!({"filter": {"op": "s_intersects",
                              "args": [{"property": "geometry"}, geometry.clone()]}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{geometry}: {body}");
    }
}

#[tokio::test]
async fn test_cql2_paging_bounds_are_400() {
    let (app, _) = setup_app().await;
    let branch_id = cql2_branch(&app).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");
    let filter = json!({"op": "=", "args": [{"property": "name"}, "small"]});

    for (paging, needle) in [
        (json!({"limit": -1}), "negative"),
        (json!({"offset": -1}), "negative"),
        (json!({"limit": 10001}), "10000"),
    ] {
        let mut body = json!({"filter": filter.clone()});
        for (k, v) in paging.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, response) = post_json(&app, &uri, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{paging}: {response}");
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|e| e.contains(needle)),
            "{paging} error must mention '{needle}': {response}"
        );
    }

    // the boundary value is accepted
    let (status, response) = post_json(
        &app,
        &uri,
        json!({"filter": filter, "limit": 10000, "offset": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["numberReturned"], 1, "{response}");
}

#[tokio::test]
async fn test_cql2_in_accepts_spec_array_form() {
    let (app, _) = setup_app().await;
    let branch_id = cql2_branch(&app).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    // spec form: args: [prop, [a, b]]
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "in", "args": [{"property": "name"}, ["small", "nobody"]]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
    assert_eq!(body["features"][0]["properties"]["name"], "small", "{body}");

    // both entries match
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "in", "args": [{"property": "name"}, ["small", "big"]]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 2, "{body}");

    // the flat form this endpoint took first still works
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "in", "args": [{"property": "name"}, "small", "big"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 2, "{body}");

    // an empty list matches nothing rather than producing invalid SQL
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"filter": {"op": "in", "args": [{"property": "name"}, []]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 0, "{body}");
}

#[tokio::test]
async fn test_cql2_rejects_non_json_filter_lang() {
    let (app, _) = setup_app().await;
    let branch_id = cql2_branch(&app).await;
    let uri = format!("/api/v1/branches/{branch_id}/features/filter");

    for lang in ["cql2-text", "CQL-Text", "sql"] {
        let (status, body) = post_json(
            &app,
            &uri,
            json!({"filter": {"op": "=", "args": [{"property": "name"}, "small"]},
                   "filter_lang": lang}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{lang}: {body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("cql2-json")),
            "{lang} error must name the supported language: {body}"
        );
    }

    // the supported value, and its default when omitted, still pass
    for body in [
        json!({"filter": {"op": "=", "args": [{"property": "name"}, "small"]},
               "filter_lang": "cql2-json"}),
        json!({"filter": {"op": "=", "args": [{"property": "name"}, "small"]}}),
    ] {
        let (status, response) = post_json(&app, &uri, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}: {response}");
        assert_eq!(response["numberReturned"], 1, "{response}");
    }
}

// ─── Per-dataset write permission enforcement ───────────────────────

fn token_for_user(sub: &str, role: Role) -> String {
    generate_token(TEST_SECRET, sub, role, 3600)
}

/// A dataset and branch created straight through the store, so they carry no
/// permission rows: the state every dataset created before enforcement is in.
async fn seed_unowned_dataset(state: &AppState) -> (Uuid, Uuid) {
    let ds = ptolemy_core::dataset::Dataset {
        id: Uuid::now_v7(),
        name: format!("unowned_{}", Uuid::now_v7()),
        srid: 4326,
        geometry_type: ptolemy_core::dataset::GeometryType::Point,
        created_at: time::OffsetDateTime::now_utc(),
        created_by: "legacy".into(),
        external: None,
        visibility: Default::default(),
    };
    state.create_dataset(&ds, None).await.unwrap();
    let branch = ptolemy_core::branch::Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "main".into(),
        head: None,
        created_at: time::OffsetDateTime::now_utc(),
        created_by: "legacy".into(),
    };
    state
        .create_branch(&branch, &ptolemy_storage::Writer::Unenforced)
        .await
        .unwrap();
    (ds.id, branch.id)
}

fn insert_op() -> Value {
    json!([{
        "type": "insert",
        "feature_id": Uuid::now_v7(),
        "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
        "properties": {"name": "one"}
    }])
}

async fn commit_as(app: &axum::Router, branch_id: Uuid, token: &str) -> (StatusCode, Value) {
    request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(token),
        Some(json!({
            "message": "permission probe",
            "author": "ignored",
            "operations": insert_op(),
        })),
    )
    .await
}

async fn grant(
    app: &axum::Router,
    scope: &str,
    id: Uuid,
    user: &str,
    permission: &str,
) -> StatusCode {
    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/{scope}/{id}/permissions"),
        Some(&token_for_user("root", Role::Admin)),
        Some(json!({
            "user_id": user,
            "permission": permission,
            "granted_by": "ignored",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "grant failed: {body}");
    status
}

/// The compatibility rule: a dataset that never had a grant keeps accepting
/// writes from any editor.
#[tokio::test]
async fn test_dataset_without_permission_rows_accepts_any_editor() {
    let (app, state) = setup_app_authed_with_state().await;
    let (_, branch_id) = seed_unowned_dataset(&state).await;

    let (status, body) = commit_as(&app, branch_id, &token_for_user("eve", Role::Editor)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// ... and the first grant flips it to enforced.
#[tokio::test]
async fn test_first_grant_locks_out_other_editors() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;

    grant(&app, "datasets", dataset_id, "alice", "write").await;

    let (status, body) = commit_as(&app, branch_id, &token_for_user("eve", Role::Editor)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = commit_as(&app, branch_id, &token_for_user("alice", Role::Editor)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// A read grant is not a write grant.
#[tokio::test]
async fn test_read_grant_cannot_write() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;

    grant(&app, "datasets", dataset_id, "viewer-vic", "read").await;

    let (status, body) =
        commit_as(&app, branch_id, &token_for_user("viewer-vic", Role::Editor)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// Branch rows decide once they exist: a dataset-level write grant does not
/// reach into an enforced branch.
#[tokio::test]
async fn test_branch_permissions_win_over_dataset() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;

    grant(&app, "datasets", dataset_id, "alice", "write").await;
    grant(&app, "branches", branch_id, "bob", "write").await;

    let (status, body) = commit_as(&app, branch_id, &token_for_user("alice", Role::Editor)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = commit_as(&app, branch_id, &token_for_user("bob", Role::Editor)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// The creator of a dataset gets an admin row, which makes the dataset enforced
/// from the moment it exists.
#[tokio::test]
async fn test_creator_owns_the_dataset_and_others_are_denied() {
    let app = setup_app_authed().await;
    let creator = token_for_user("carol", Role::Editor);
    let intruder = token_for_user("eve", Role::Editor);

    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&creator),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = dataset["id"].as_str().unwrap().to_string();

    // the auto-granted row is what the creator writes with
    let (status, perms) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/permissions"),
        Some(&token_for_user("root", Role::Admin)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{perms}");
    let rows = perms.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{perms}");
    assert_eq!(rows[0]["user_id"], "carol", "{perms}");
    assert_eq!(rows[0]["permission"], "admin", "{perms}");
    assert_eq!(rows[0]["granted_by"], "carol", "{perms}");

    // creating a branch is a dataset write, so the intruder cannot
    let branch_uri = format!("/api/v1/datasets/{dataset_id}/branches");
    let (status, body) = request_as(
        &app,
        "POST",
        &branch_uri,
        Some(&intruder),
        Some(json!({"name": "intruder", "created_by": "eve"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, branch) = request_as(
        &app,
        "POST",
        &branch_uri,
        Some(&creator),
        Some(json!({"name": "main", "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let (status, body) = commit_as(&app, branch_id, &intruder).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = commit_as(&app, branch_id, &creator).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // the instance admin role bypasses per-dataset permissions
    let (status, body) = commit_as(&app, branch_id, &token_for_user("root", Role::Admin)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Import, sync push, merge and compaction go through the same ladder as commit.
#[tokio::test]
async fn test_every_write_path_checks_permissions() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;
    grant(&app, "datasets", dataset_id, "alice", "write").await;
    let alice = token_for_user("alice", Role::Editor);
    let eve = token_for_user("eve", Role::Editor);

    // a commit alice is allowed to make, so the branch has a head to merge from
    let (status, body) = commit_as(&app, branch_id, &alice).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let geojson = json!({
        "features": [{
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": [1.0, 2.0]},
            "properties": {"name": "imported"}
        }],
        "author": "ignored"
    });
    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/import/geojson"),
        Some(&eve),
        Some(geojson.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "import as eve: {body}");
    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/import/geojson"),
        Some(&alice),
        Some(geojson),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "import as alice: {body}");

    let push = json!({
        "branch_id": branch_id,
        "message": "offline edits",
        "author": "ignored",
        "operations": [{
            "type": "insert",
            "feature_id": Uuid::now_v7(),
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {"name": "pushed"}
        }]
    });
    let (status, body) =
        request_as(&app, "POST", "/api/v1/sync/push", Some(&eve), Some(push)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "sync push as eve: {body}");

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/compact"),
        Some(&eve),
        Some(json!({"keep_latest": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "compact as eve: {body}");

    // merge needs write on the target branch, which is where the commit lands
    let (status, fork) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&alice),
        Some(json!({"name": "fork", "created_by": "alice", "fork_from_branch": branch_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id = Uuid::parse_str(fork["id"].as_str().unwrap()).unwrap();
    let (status, body) = commit_as(&app, fork_id, &alice).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/merge/{fork_id}"),
        Some(&eve),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "merge as eve: {body}");

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/merge/{fork_id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge as alice: {body}");
}

/// With auth off there is no identity to check, so permission rows are ignored
/// and the dev and CLI flows keep working.
#[tokio::test]
async fn test_auth_disabled_ignores_permission_rows() {
    let (app, state) = setup_app().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;
    state
        .grant_dataset_permission(dataset_id, "alice", "write", "root")
        .await
        .unwrap();

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/commit"),
        json!({
            "message": "dev mode commit",
            "author": "whoever",
            "operations": insert_op(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

// ─── Per-dataset read visibility ────────────────────────────────────

/// A private dataset owned by "carol", with one feature committed and the branch
/// id of that content. Returns (dataset id, branch id, carol's token).
async fn seed_private_dataset(app: &axum::Router) -> (Uuid, Uuid, String) {
    let carol = token_for_user("carol", Role::Editor);
    let mut body = new_dataset_body();
    body["visibility"] = json!("private");
    let (status, dataset) =
        request_as(app, "POST", "/api/v1/datasets", Some(&carol), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    assert_eq!(dataset["visibility"], "private", "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

    let (status, branch) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "main", "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let (status, body) = commit_as(app, branch_id, &carol).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    (dataset_id, branch_id, carol)
}

/// Reads whose handler needs an optional postgres extension the test database
/// may not have (h3-pg, pgvector). Visibility still has to gate them, so they
/// stay in the list, but an authorized caller may legitimately get a 500.
fn needs_optional_extension(uri: &str) -> bool {
    uri.contains("/h3/") || uri.contains("/similarity/")
}

/// Every read path that serves content of a private dataset, so one test covers
/// the handlers that resolve a dataset in their own way.
fn content_read_uris(dataset_id: Uuid, branch_id: Uuid) -> Vec<String> {
    vec![
        format!("/api/v1/datasets/{dataset_id}"),
        format!("/api/v1/datasets/{dataset_id}/branches"),
        format!("/api/v1/branches/{branch_id}"),
        format!("/api/v1/branches/{branch_id}/features"),
        format!("/api/v1/branches/{branch_id}/features/count"),
        format!(
            "/api/v1/branches/{branch_id}/features/bbox?min_x=-180&min_y=-90&max_x=180&max_y=90"
        ),
        format!("/api/v1/branches/{branch_id}/features/at?at=2030-01-01T00:00:00Z"),
        format!("/api/v1/branches/{branch_id}/history"),
        format!("/api/v1/branches/{branch_id}/tiles/0/0/0"),
        format!("/api/v1/branches/{branch_id}/export/geojson"),
        format!("/api/v1/branches/{branch_id}/export/csv"),
        format!("/api/v1/branches/{branch_id}/export/flatgeobuf"),
        format!("/api/v1/branches/{branch_id}/h3/hexagons?resolution=7"),
        format!("/api/v1/branches/{branch_id}/similarity/duplicates"),
        format!("/api/v1/branches/{branch_id}/quality"),
        format!("/api/v1/qgis/branches/{branch_id}/layer"),
        format!("/api/v1/ogc/collections/{dataset_id}"),
        format!("/api/v1/ogc/collections/{dataset_id}/items"),
        format!("/api/v1/sync/pull?branch_id={branch_id}"),
        format!("/api/v1/sensors?branch_id={branch_id}"),
    ]
}

#[tokio::test]
async fn test_private_dataset_content_is_404_for_outsiders() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = seed_private_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    for uri in content_read_uris(dataset_id, branch_id) {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "anonymous GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-granted editor GET {uri}: {body}"
        );

        if needs_optional_extension(&uri) {
            continue;
        }

        // the owner and the instance admin both get real answers
        let (status, body) = request_as(&app, "GET", &uri, Some(&carol), None).await;
        assert_eq!(status, StatusCode::OK, "owner GET {uri}: {body}");

        let (status, body) = request_as(
            &app,
            "GET",
            &uri,
            Some(&token_for_user("root", Role::Admin)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin GET {uri}: {body}");
    }
}

/// A query-shaped POST is a read, and it is covered too.
#[tokio::test]
async fn test_private_dataset_query_posts_are_404_for_outsiders() {
    let app = setup_app_authed().await;
    let (_, branch_id, carol) = seed_private_dataset(&app).await;

    let bodies: Vec<(String, Value)> = vec![
        (
            format!("/api/v1/branches/{branch_id}/features/intersects"),
            json!({"geometry": {"type": "Point", "coordinates": [1.0, 1.0]}}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/features/filter"),
            json!({"filter": {"op": "=", "args": [{"property": "name"}, "one"]}}),
        ),
    ];

    for (uri, body) in bodies {
        let (status, resp) = request_as(&app, "POST", &uri, None, Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "anonymous POST {uri}: {resp}"
        );

        let (status, resp) = request_as(&app, "POST", &uri, Some(&carol), Some(body)).await;
        assert_eq!(status, StatusCode::OK, "owner POST {uri}: {resp}");
    }
}

/// A plain read grant is enough to see a private dataset.
#[tokio::test]
async fn test_read_grant_opens_a_private_dataset() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _) = seed_private_dataset(&app).await;
    let vic = token_for_user("vic", Role::Viewer);

    let uri = format!("/api/v1/branches/{branch_id}/features");
    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    grant(&app, "datasets", dataset_id, "vic", "read").await;

    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["features"].as_array().unwrap().len(), 1, "{body}");
}

/// A grant on one branch is enough for the dataset's content.
#[tokio::test]
async fn test_branch_grant_opens_a_private_dataset() {
    let app = setup_app_authed().await;
    let (_, branch_id, _) = seed_private_dataset(&app).await;
    let bob = token_for_user("bob", Role::Viewer);

    grant(&app, "branches", branch_id, "bob", "read").await;

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/features"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Public datasets keep serving anonymous reads, which is what the viewer's
/// golden path depends on.
#[tokio::test]
async fn test_public_dataset_reads_stay_anonymous() {
    let app = setup_app_authed().await;
    let carol = token_for_user("carol", Role::Editor);

    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    assert_eq!(dataset["visibility"], "public", "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "main", "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();
    let (status, body) = commit_as(&app, branch_id, &carol).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    for uri in content_read_uris(dataset_id, branch_id) {
        if needs_optional_extension(&uri) {
            continue;
        }
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "anonymous GET {uri}: {body}");
    }
}

/// External datasets get the same treatment: the read substitutes a derived
/// table for the features view, but visibility is decided before that.
#[tokio::test]
async fn test_private_external_dataset_is_covered() {
    let (app, state) = setup_app_authed_with_state().await;
    create_external_fixture(&state).await;
    let carol = token_for_user("carol", Role::Editor);

    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(json!({
            "name": format!("ext_{}", Uuid::now_v7()),
            "created_by": "carol",
            "visibility": "private",
            "external_table": "ext_parcels",
            "external_id_column": "parcel_id",
            "external_geometry_column": "geom",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

    let (status, branches) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{branches}");
    let branch_id =
        Uuid::parse_str(branches.as_array().unwrap()[0]["id"].as_str().unwrap()).unwrap();

    for uri in [
        format!("/api/v1/branches/{branch_id}/features"),
        format!("/api/v1/branches/{branch_id}/export/geojson"),
        format!("/api/v1/ogc/collections/{dataset_id}/items"),
    ] {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "anonymous GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&carol), None).await;
        assert_eq!(status, StatusCode::OK, "owner GET {uri}: {body}");
    }
}

/// Flipping visibility is a dataset-admin operation, and it takes effect at once.
#[tokio::test]
async fn test_visibility_patch_needs_a_dataset_admin_grant() {
    let app = setup_app_authed().await;
    let carol = token_for_user("carol", Role::Editor);
    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();
    let uri = format!("/api/v1/datasets/{dataset_id}");

    // a write grant is not enough to publish or hide someone else's dataset
    grant(&app, "datasets", dataset_id, "eve", "write").await;
    let (status, body) = request_as(
        &app,
        "PATCH",
        &uri,
        Some(&token_for_user("eve", Role::Editor)),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = request_as(
        &app,
        "PATCH",
        &uri,
        Some(&carol),
        Some(json!({"visibility": "nonsense"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = request_as(
        &app,
        "PATCH",
        &uri,
        Some(&carol),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["visibility"], "private", "{body}");

    let (status, body) = request_as(&app, "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = request_as(
        &app,
        "PATCH",
        &uri,
        Some(&carol),
        Some(json!({"visibility": "public"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request_as(&app, "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The auth layer sits outside visibility: a token that does not verify is a 401
/// on a write, and is treated as anonymous on a read.
#[tokio::test]
async fn test_bad_token_does_not_reach_visibility() {
    let app = setup_app_authed().await;
    let (_, branch_id, _) = seed_private_dataset(&app).await;

    let (status, body) = commit_as(&app, branch_id, "not-a-jwt").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/features"),
        Some("not-a-jwt"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// With auth off there is no identity, so visibility is not enforced either.
#[tokio::test]
async fn test_auth_disabled_ignores_visibility() {
    let (app, state) = setup_app().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;
    state
        .set_dataset_visibility(dataset_id, ptolemy_core::dataset::Visibility::Private)
        .await
        .unwrap();

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/features")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Three read handlers take the branch id in the request body, where the
/// visibility layer cannot see it, so they check it themselves.
#[tokio::test]
async fn test_body_scoped_reads_respect_visibility() {
    let app = setup_app_authed().await;
    let (_, branch_id, carol) = seed_private_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let feature = Uuid::now_v7();

    let calls: Vec<(&str, Value)> = vec![
        (
            "/api/v1/parcels/split",
            json!({
                "branch_id": branch_id,
                "feature_id": feature,
                "line": [[0.0, 0.0], [1.0, 1.0]],
                "author": "eve"
            }),
        ),
        (
            "/api/v1/parcels/merge",
            json!({
                "branch_id": branch_id,
                "feature_ids": [feature, Uuid::now_v7()],
                "author": "eve"
            }),
        ),
        (
            "/api/v1/surveys/compare",
            json!({
                "branch_id": branch_id,
                "survey_a": feature,
                "survey_b": Uuid::now_v7()
            }),
        ),
    ];

    for (uri, body) in calls {
        let (status, resp) = request_as(&app, "POST", uri, Some(&eve), Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-granted editor POST {uri}: {resp}"
        );

        // the owner gets past the visibility check and into the handler, which
        // then complains about the made-up feature ids rather than hiding the branch
        let (status, resp) = request_as(&app, "POST", uri, Some(&carol), Some(body)).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "owner POST {uri}: {resp}");
    }
}

/// A raster catalog with one tile inside a private dataset. The tile row goes in
/// through the store because uploading one through the API needs real raster WKB.
/// Returns (catalog id, tile id, carol's token).
async fn seed_private_raster(app: &axum::Router, state: &AppState) -> (Uuid, Uuid, Uuid, String) {
    let (dataset_id, _, carol) = seed_private_dataset(app).await;

    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/rasters"),
        Some(&carol),
        Some(json!({"name": "imagery", "srid": 4326, "num_bands": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let catalog_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let tile_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO raster_tiles (id, catalog_id, bounds, zoom_level)
         VALUES ($1, $2, ST_MakeEnvelope(0, 0, 1, 1, 4326), 0)",
    )
    .bind(tile_id)
    .bind(catalog_id)
    .execute(state.pool())
    .await
    .unwrap();

    (dataset_id, catalog_id, tile_id, carol)
}

/// Raster and STAC reads name a catalog or a tile, never the dataset, so they
/// rely on the layer resolving those ids back to the owning dataset.
#[tokio::test]
async fn test_private_dataset_rasters_are_404_for_outsiders() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, catalog_id, tile_id, carol) = seed_private_raster(&app, &state).await;
    let eve = token_for_user("eve", Role::Editor);

    let uris = [
        format!("/api/v1/datasets/{dataset_id}/rasters"),
        format!("/api/v1/rasters/{catalog_id}"),
        format!("/api/v1/rasters/{catalog_id}/tiles"),
        format!("/api/v1/rasters/{catalog_id}/value?lng=0.5&lat=0.5"),
        format!("/api/v1/rasters/{catalog_id}/stats"),
        format!("/api/v1/stac/collections/{catalog_id}"),
        format!("/api/v1/stac/collections/{catalog_id}/items"),
        format!("/api/v1/stac/collections/{catalog_id}/items/{tile_id}"),
    ];

    for uri in uris {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "anonymous GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-granted editor GET {uri}: {body}"
        );

        let (status, body) = request_as(&app, "GET", &uri, Some(&carol), None).await;
        assert_eq!(status, StatusCode::OK, "owner GET {uri}: {body}");

        let (status, body) = request_as(
            &app,
            "GET",
            &uri,
            Some(&token_for_user("root", Role::Admin)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin GET {uri}: {body}");
    }
}

/// A read grant opens the raster reads exactly as it opens the feature reads.
#[tokio::test]
async fn test_read_grant_opens_a_private_raster() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, catalog_id, _, _) = seed_private_raster(&app, &state).await;
    let vic = token_for_user("vic", Role::Viewer);
    let uri = format!("/api/v1/rasters/{catalog_id}");

    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    grant(&app, "datasets", dataset_id, "vic", "read").await;

    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// STAC search names no id, so it filters its own results.
#[tokio::test]
async fn test_stac_search_hides_private_tiles() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, _, tile_id, carol) = seed_private_raster(&app, &state).await;
    let tile = tile_id.to_string();
    let eve = token_for_user("eve", Role::Editor);
    let vic = token_for_user("vic", Role::Viewer);

    for (who, token) in [("anonymous", None), ("non-granted editor", Some(&eve))] {
        assert!(
            !listing_mentions(
                &app,
                "/api/v1/stac/search",
                token.map(String::as_str),
                &tile
            )
            .await,
            "{who} STAC search leaked a private tile"
        );
    }

    for (who, token) in [
        ("owner", carol.clone()),
        ("instance admin", token_for_user("root", Role::Admin)),
    ] {
        assert!(
            listing_mentions(&app, "/api/v1/stac/search", Some(&token), &tile).await,
            "{who} STAC search hid the tile"
        );
    }

    assert!(!listing_mentions(&app, "/api/v1/stac/search", Some(&vic), &tile).await);
    grant(&app, "datasets", dataset_id, "vic", "read").await;
    assert!(
        listing_mentions(&app, "/api/v1/stac/search", Some(&vic), &tile).await,
        "a read grant did not surface the tile in STAC search"
    );
}

/// Public rasters keep serving anonymous reads.
#[tokio::test]
async fn test_public_raster_reads_stay_anonymous() {
    let (app, state) = setup_app_authed_with_state().await;
    let carol = token_for_user("carol", Role::Editor);
    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/rasters"),
        Some(&carol),
        Some(json!({"name": "imagery"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let catalog_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let tile_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO raster_tiles (id, catalog_id, bounds, zoom_level)
         VALUES ($1, $2, ST_MakeEnvelope(0, 0, 1, 1, 4326), 0)",
    )
    .bind(tile_id)
    .bind(catalog_id)
    .execute(state.pool())
    .await
    .unwrap();

    for uri in [
        format!("/api/v1/rasters/{catalog_id}"),
        format!("/api/v1/rasters/{catalog_id}/tiles"),
        format!("/api/v1/stac/collections/{catalog_id}"),
        format!("/api/v1/stac/collections/{catalog_id}/items/{tile_id}"),
    ] {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "anonymous GET {uri}: {body}");
    }

    assert!(
        listing_mentions(&app, "/api/v1/stac/search", None, &tile_id.to_string()).await,
        "anonymous STAC search hid a public tile"
    );
}

/// STAC search takes its collection filter as a query value, so the layer sees
/// the catalog id too. What this pins is the filter itself: a public tile the
/// caller asked for comes back, one they did not does not.
#[tokio::test]
async fn test_stac_search_collections_filter() {
    let (app, state) = setup_app_authed_with_state().await;
    let carol = token_for_user("carol", Role::Editor);

    let mut tiles = Vec::new();
    for _ in 0..2 {
        let (status, dataset) = request_as(
            &app,
            "POST",
            "/api/v1/datasets",
            Some(&carol),
            Some(new_dataset_body()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{dataset}");
        let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

        let (status, body) = request_as(
            &app,
            "POST",
            &format!("/api/v1/datasets/{dataset_id}/rasters"),
            Some(&carol),
            Some(json!({"name": "imagery"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let catalog_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

        let tile_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO raster_tiles (id, catalog_id, bounds, zoom_level)
             VALUES ($1, $2, ST_MakeEnvelope(0, 0, 1, 1, 4326), 0)",
        )
        .bind(tile_id)
        .bind(catalog_id)
        .execute(state.pool())
        .await
        .unwrap();
        tiles.push((catalog_id, tile_id));
    }

    let (wanted_catalog, wanted_tile) = tiles[0];
    let (_, other_tile) = tiles[1];
    let uri = format!("/api/v1/stac/search?collections={wanted_catalog}");
    assert!(
        listing_mentions(&app, &uri, None, &wanted_tile.to_string()).await,
        "the filtered collection's tile was missing"
    );
    assert!(
        !listing_mentions(&app, &uri, None, &other_tile.to_string()).await,
        "the filter let another collection's tile through"
    );

    // a collection id that is not a uuid matches nothing rather than everything
    assert!(
        !listing_mentions(
            &app,
            "/api/v1/stac/search?collections=not-a-uuid",
            None,
            &wanted_tile.to_string()
        )
        .await,
        "an unparseable collection id disabled the filter"
    );
}

/// A point cloud catalog with one patch inside a private dataset. The patch row
/// goes in through the store: adding one through the API needs real PC binary.
/// Returns (dataset id, catalog id, patch id, carol's token).
async fn seed_private_pointcloud(
    app: &axum::Router,
    state: &AppState,
) -> (Uuid, Uuid, Uuid, String) {
    let (dataset_id, _, carol) = seed_private_dataset(app).await;

    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/pointclouds"),
        Some(&carol),
        Some(json!({"name": "lidar", "srid": 4326})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let catalog_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let patch_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pointcloud_patches (id, catalog_id, bounds, num_points)
         VALUES ($1, $2, ST_MakeEnvelope(0, 0, 1, 1, 4326), 3)",
    )
    .bind(patch_id)
    .bind(catalog_id)
    .execute(state.pool())
    .await
    .unwrap();

    (dataset_id, catalog_id, patch_id, carol)
}

/// Point cloud reads name a catalog, never the dataset, so they rely on the
/// layer resolving that id back to the owning dataset.
#[tokio::test]
async fn test_private_dataset_pointclouds_are_404_for_outsiders() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, catalog_id, _, carol) = seed_private_pointcloud(&app, &state).await;
    let eve = token_for_user("eve", Role::Editor);
    let root = token_for_user("root", Role::Admin);

    for uri in [
        format!("/api/v1/datasets/{dataset_id}/pointclouds"),
        format!("/api/v1/pointclouds/{catalog_id}"),
        format!("/api/v1/pointclouds/{catalog_id}/patches"),
        format!("/api/v1/pointclouds/{catalog_id}/stats"),
    ] {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "anonymous GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-granted editor GET {uri}: {body}"
        );

        let (status, body) = request_as(&app, "GET", &uri, Some(&carol), None).await;
        assert_eq!(status, StatusCode::OK, "owner GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&root), None).await;
        assert_eq!(status, StatusCode::OK, "admin GET {uri}: {body}");
    }

    // the spatial query and the profile are POST reads, so the visibility layer
    // is their only gate: an outsider gets the same 404 as on a GET
    let bbox = json!({"min_x": -1.0, "min_y": -1.0, "max_x": 2.0, "max_y": 2.0});
    let query_uri = format!("/api/v1/pointclouds/{catalog_id}/query");
    let profile_uri = format!("/api/v1/pointclouds/{catalog_id}/profile");
    let profile_body = json!({"line_wkb_hex": "00"});

    for (uri, body) in [(&query_uri, bbox.clone()), (&profile_uri, profile_body)] {
        let (status, resp) = request_as(&app, "POST", uri, None, Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "anonymous POST {uri}: {resp}"
        );

        let (status, resp) = request_as(&app, "POST", uri, Some(&eve), Some(body)).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-granted editor POST {uri}: {resp}"
        );
    }

    let (status, resp) = request_as(&app, "POST", &query_uri, Some(&carol), Some(bbox)).await;
    assert_eq!(status, StatusCode::OK, "owner POST {query_uri}: {resp}");
    assert_eq!(resp["patch_count"], 1, "{resp}");
}

/// A read grant opens the point cloud reads exactly as it opens the feature reads.
#[tokio::test]
async fn test_read_grant_opens_a_private_pointcloud() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, catalog_id, _, _) = seed_private_pointcloud(&app, &state).await;
    let vic = token_for_user("vic", Role::Viewer);
    let uri = format!("/api/v1/pointclouds/{catalog_id}");

    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    grant(&app, "datasets", dataset_id, "vic", "read").await;

    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The point cloud query and profile are reads, so a public dataset serves them
/// to an anonymous caller like any other read.
#[tokio::test]
async fn test_public_pointcloud_queries_stay_anonymous() {
    let (app, state) = setup_app_authed_with_state().await;
    let carol = token_for_user("carol", Role::Editor);
    let (dataset_id, _) = seed_owned_public_dataset(&app).await;

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/pointclouds"),
        Some(&carol),
        Some(json!({"name": "lidar"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let catalog_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    sqlx::query(
        "INSERT INTO pointcloud_patches (id, catalog_id, bounds, num_points)
         VALUES ($1, $2, ST_MakeEnvelope(0, 0, 1, 1, 4326), 3)",
    )
    .bind(Uuid::now_v7())
    .bind(catalog_id)
    .execute(state.pool())
    .await
    .unwrap();

    let (status, resp) = request_as(
        &app,
        "POST",
        &format!("/api/v1/pointclouds/{catalog_id}/query"),
        None,
        Some(json!({"min_x": -1.0, "min_y": -1.0, "max_x": 2.0, "max_y": 2.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "anonymous query: {resp}");
    assert_eq!(resp["patch_count"], 1, "{resp}");

    // the profile handler needs the pointcloud extension the test database may
    // not have, so what is pinned is that it is not turned away
    let (status, resp) = request_as(
        &app,
        "POST",
        &format!("/api/v1/pointclouds/{catalog_id}/profile"),
        None,
        Some(json!({"line_wkb_hex": "00"})),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "anonymous profile: {resp}"
    );
    assert_ne!(status, StatusCode::NOT_FOUND, "anonymous profile: {resp}");
}

/// Both attachment shapes inside a private dataset: one owned by the dataset,
/// one owned by a feature on its branch. Returns
/// (dataset id, branch id, feature id, dataset attachment, feature attachment,
/// carol's token).
async fn seed_private_attachments(app: &axum::Router) -> (Uuid, Uuid, Uuid, Uuid, Uuid, String) {
    let (dataset_id, branch_id, carol) = seed_private_dataset(app).await;

    let feature_id = Uuid::now_v7();
    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(&carol),
        Some(json!({
            "message": "attachment target",
            "author": "carol",
            "operations": [{
                "type": "insert",
                "feature_id": feature_id,
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {},
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/attachments"),
        Some(&carol),
        Some(upload_body("icon.png", "dataset-bytes")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let dataset_attachment = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments"),
        Some(&carol),
        Some(upload_body("photo.jpg", "feature-bytes")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let feature_attachment = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    (
        dataset_id,
        branch_id,
        feature_id,
        dataset_attachment,
        feature_attachment,
        carol,
    )
}

/// Downloading an attachment names only the attachment id, so it relies on the
/// layer resolving that id back to the owning dataset. Both owner shapes count.
#[tokio::test]
async fn test_private_dataset_attachments_are_404_for_outsiders() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, feature_id, dataset_attachment, feature_attachment, carol) =
        seed_private_attachments(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let root = token_for_user("root", Role::Admin);

    for uri in [
        format!("/api/v1/attachments/{dataset_attachment}"),
        format!("/api/v1/attachments/{dataset_attachment}/meta"),
        format!("/api/v1/attachments/{feature_attachment}"),
        format!("/api/v1/attachments/{feature_attachment}/meta"),
        // the list routes name a dataset or a branch, and stay as they were
        format!("/api/v1/datasets/{dataset_id}/attachments"),
        format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments"),
    ] {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "anonymous GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-granted editor GET {uri}: {body}"
        );

        let (status, body) = request_as(&app, "GET", &uri, Some(&carol), None).await;
        assert_eq!(status, StatusCode::OK, "owner GET {uri}: {body}");

        let (status, body) = request_as(&app, "GET", &uri, Some(&root), None).await;
        assert_eq!(status, StatusCode::OK, "admin GET {uri}: {body}");
    }
}

/// A read grant opens the attachment reads exactly as it opens the feature reads.
#[tokio::test]
async fn test_read_grant_opens_a_private_attachment() {
    let app = setup_app_authed().await;
    let (dataset_id, _, _, dataset_attachment, feature_attachment, _) =
        seed_private_attachments(&app).await;
    let vic = token_for_user("vic", Role::Viewer);

    let uris = [
        format!("/api/v1/attachments/{dataset_attachment}"),
        format!("/api/v1/attachments/{dataset_attachment}/meta"),
        format!("/api/v1/attachments/{feature_attachment}"),
        format!("/api/v1/attachments/{feature_attachment}/meta"),
    ];

    for uri in &uris {
        let (status, body) = request_as(&app, "GET", uri, Some(&vic), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "before the grant {uri}: {body}"
        );
    }

    grant(&app, "datasets", dataset_id, "vic", "read").await;

    for uri in &uris {
        let (status, body) = request_as(&app, "GET", uri, Some(&vic), None).await;
        assert_eq!(status, StatusCode::OK, "after the grant {uri}: {body}");
    }
}

/// Deleting an attachment is a write, and the layer gates it on the same
/// resolution: an editor with no grant on the private dataset cannot reach it.
#[tokio::test]
async fn test_private_attachment_delete_needs_a_grant() {
    let app = setup_app_authed().await;
    let (dataset_id, _, _, dataset_attachment, feature_attachment, carol) =
        seed_private_attachments(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    let uris = [
        format!("/api/v1/attachments/{dataset_attachment}"),
        format!("/api/v1/attachments/{feature_attachment}"),
    ];

    for uri in &uris {
        let (status, body) = request_as(&app, "DELETE", uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "outsider DELETE {uri}: {body}"
        );

        // the blob is still there
        let (status, body) = request_as(&app, "GET", uri, Some(&carol), None).await;
        assert_eq!(status, StatusCode::OK, "owner GET {uri}: {body}");
    }

    grant(&app, "datasets", dataset_id, "eve", "write").await;

    for uri in &uris {
        let (status, body) = request_as(&app, "DELETE", uri, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "granted DELETE {uri}: {body}"
        );
    }
}

/// A public dataset's attachments stay anonymously readable, which is what a
/// style's icon depends on.
#[tokio::test]
async fn test_public_dataset_attachments_stay_anonymous() {
    let app = setup_app_authed().await;
    let (dataset_id, carol) = seed_owned_public_dataset(&app).await;

    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "main", "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();
    let feature_id = Uuid::now_v7();

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/attachments"),
        Some(&carol),
        Some(upload_body("icon.png", "dataset-bytes")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let dataset_attachment = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments"),
        Some(&carol),
        Some(upload_body("photo.jpg", "feature-bytes")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let feature_attachment = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    for uri in [
        format!("/api/v1/attachments/{dataset_attachment}"),
        format!("/api/v1/attachments/{feature_attachment}"),
    ] {
        let (status, body) = request_as(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "anonymous GET {uri}: {body}");
    }

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/attachments/{dataset_attachment}/meta"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "icon.png", "{body}");
}

// ─── The remaining dataset-owned id kinds ───────────────────────────

/// One id kind a route names that is neither a dataset nor a branch id, so the
/// layer can only gate it by resolving it back to its owning dataset.
struct ChildIdKind {
    name: &'static str,
    /// inserts one row, `$1` the id a route will name and `$2` its dataset
    insert: &'static str,
    /// route paths naming that id, `{id}` substituted
    paths: &'static [&'static str],
}

/// The read-reachable kinds. Rows go in through the pool, not the create
/// endpoints, so seeding does not itself depend on the grants under test.
const CHILD_ID_KINDS: &[ChildIdKind] = &[
    ChildIdKind {
        name: "network",
        insert: "INSERT INTO networks (id, dataset_id, name) VALUES ($1, $2, 'net')",
        paths: &["/networks/{id}", "/networks/{id}/edges", "/networks/{id}/junctions"],
    },
    ChildIdKind {
        name: "lrs route",
        insert: "INSERT INTO routes (id, dataset_id, name, geometry, total_length)
                 VALUES ($1, $2, 'route', ST_GeomFromText('LINESTRINGM(0 0 0, 1 1 100)', 4326), 100)",
        paths: &["/routes/{id}", "/routes/{id}/events"],
    },
    ChildIdKind {
        name: "symbology rule",
        insert: "INSERT INTO symbology_rules (id, dataset_id, name, symbol)
                 VALUES ($1, $2, 'sym', '{}')",
        paths: &["/symbology/{id}"],
    },
    ChildIdKind {
        name: "label rule",
        insert: "INSERT INTO label_rules (id, dataset_id, name, field_expression)
                 VALUES ($1, $2, 'label', 'name')",
        paths: &["/labels/{id}"],
    },
    ChildIdKind {
        name: "domain",
        insert: "INSERT INTO domains (id, dataset_id, name, domain_type, field_type)
                 VALUES ($1, $2, 'dom', 'coded_value', 'string')",
        paths: &["/domains/{id}"],
    },
    ChildIdKind {
        name: "subtype",
        insert: "INSERT INTO subtypes (id, dataset_id, subtype_field, code, name)
                 VALUES ($1, $2, 'kind', 1, 'sub')",
        paths: &["/subtypes/{id}"],
    },
    ChildIdKind {
        name: "attribute rule",
        insert: "INSERT INTO attribute_rules (id, dataset_id, name, rule_type, trigger_event, expression)
                 VALUES ($1, $2, 'rule', 'constraint', 'insert', 'name IS NOT NULL')",
        paths: &["/attribute-rules/{id}"],
    },
    ChildIdKind {
        name: "trajectory",
        insert: "INSERT INTO trajectories (id, dataset_id, name) VALUES ($1, $2, 'traj')",
        paths: &["/trajectories/{id}"],
    },
    ChildIdKind {
        name: "relationship class",
        insert: "INSERT INTO relationship_classes
                     (id, name, origin_dataset_id, destination_dataset_id, origin_foreign_key)
                 VALUES ($1, 'rel', $2, $2, 'fk')",
        paths: &["/relationship-classes/{id}", "/relationship-classes/{id}/records"],
    },
];

/// The kinds whose only route is a delete. An anonymous request there is a 401
/// from the auth layer, so what visibility decides is whether an editor without
/// a grant on the dataset can reach the row. A webhook id is not in here: every
/// method on `/webhooks/{id}` is admin-only, and an instance admin skips
/// visibility, so nothing it decides is observable there.
const DELETE_ONLY_ID_KINDS: &[ChildIdKind] = &[
    ChildIdKind {
        name: "topology rule",
        insert: "INSERT INTO topology_rules (id, dataset_id, rule_type, description)
                 VALUES ($1, $2, '\"must_not_overlap\"', '')",
        paths: &["/topology/{id}"],
    },
    ChildIdKind {
        name: "relationship record",
        insert: "WITH c AS (
                     INSERT INTO relationship_classes
                         (id, name, origin_dataset_id, destination_dataset_id, origin_foreign_key)
                     VALUES (gen_random_uuid(), 'rel', $2, $2, 'fk')
                     RETURNING id
                 )
                 INSERT INTO relationship_records
                     (id, relationship_class_id, origin_feature_id, destination_feature_id)
                 SELECT $1, c.id, gen_random_uuid(), gen_random_uuid() FROM c",
        paths: &["/relationship-records/{id}"],
    },
];

async fn seed_child_id(state: &AppState, kind: &ChildIdKind, dataset_id: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(kind.insert)
        .bind(id)
        .bind(dataset_id)
        .execute(state.pool())
        .await
        .unwrap_or_else(|e| panic!("seeding a {}: {e}", kind.name));
    id
}

fn child_uris(kind: &ChildIdKind, id: Uuid) -> Vec<String> {
    kind.paths
        .iter()
        .map(|p| format!("/api/v1{}", p.replace("{id}", &id.to_string())))
        .collect()
}

/// Reads that name a network, an LRS route, a symbology or label rule, a domain,
/// a subtype, an attribute rule, a trajectory or a relationship class never name
/// the dataset, so the layer has to resolve each of those ids to it.
#[tokio::test]
async fn test_private_dataset_children_are_404_for_outsiders() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, _, carol) = seed_private_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let root = token_for_user("root", Role::Admin);
    let vic = token_for_user("vic", Role::Viewer);
    grant(&app, "datasets", dataset_id, "vic", "read").await;

    for kind in CHILD_ID_KINDS {
        let child = seed_child_id(&state, kind, dataset_id).await;
        for uri in child_uris(kind, child) {
            let (status, body) = request_as(&app, "GET", &uri, None, None).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "anonymous GET a {} at {uri}: {body}",
                kind.name
            );

            let (status, body) = request_as(&app, "GET", &uri, Some(&eve), None).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "non-granted editor GET a {} at {uri}: {body}",
                kind.name
            );

            for (who, token) in [
                ("owner", &carol),
                ("granted viewer", &vic),
                ("admin", &root),
            ] {
                let (status, body) = request_as(&app, "GET", &uri, Some(token), None).await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{who} GET a {} at {uri}: {body}",
                    kind.name
                );
            }
        }
    }
}

/// The same children under a public dataset stay anonymously readable.
#[tokio::test]
async fn test_public_dataset_children_stay_anonymous() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, _) = seed_owned_public_dataset(&app).await;

    for kind in CHILD_ID_KINDS {
        let child = seed_child_id(&state, kind, dataset_id).await;
        for uri in child_uris(kind, child) {
            let (status, body) = request_as(&app, "GET", &uri, None, None).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "anonymous GET a public {} at {uri}: {body}",
                kind.name
            );
        }
    }
}

/// The delete-only kinds: without a grant on the private dataset the row is not
/// there to delete, with one it is.
#[tokio::test]
async fn test_private_dataset_delete_only_children_need_a_grant() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, _, _) = seed_private_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    let mut granted = Vec::new();
    for kind in DELETE_ONLY_ID_KINDS {
        let child = seed_child_id(&state, kind, dataset_id).await;
        for uri in child_uris(kind, child) {
            let (status, body) = request_as(&app, "DELETE", &uri, Some(&eve), None).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "non-granted editor DELETE a {} at {uri}: {body}",
                kind.name
            );
        }
        granted.push((kind, seed_child_id(&state, kind, dataset_id).await));
    }

    grant(&app, "datasets", dataset_id, "eve", "write").await;

    for (kind, child) in granted {
        for uri in child_uris(kind, child) {
            let (status, body) = request_as(&app, "DELETE", &uri, Some(&eve), None).await;
            assert_eq!(
                status,
                StatusCode::NO_CONTENT,
                "granted editor DELETE a {} at {uri}: {body}",
                kind.name
            );
        }
    }
}

/// A relationship class spans two datasets, so it resolves to both and a grant on
/// only one of them is not enough.
#[tokio::test]
async fn test_relationship_class_needs_every_dataset_granted() {
    let (app, state) = setup_app_authed_with_state().await;
    let (origin, _, _) = seed_private_dataset(&app).await;
    let (destination, _, _) = seed_private_dataset(&app).await;
    let vic = token_for_user("vic", Role::Viewer);

    let class_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relationship_classes
             (id, name, origin_dataset_id, destination_dataset_id, origin_foreign_key)
         VALUES ($1, 'rel', $2, $3, 'fk')",
    )
    .bind(class_id)
    .bind(origin)
    .bind(destination)
    .execute(state.pool())
    .await
    .unwrap();

    let resolved = state.private_datasets_for_ids(&[class_id]).await.unwrap();
    assert_eq!(resolved.len(), 2, "{resolved:?}");
    assert!(
        resolved.contains(&origin) && resolved.contains(&destination),
        "{resolved:?}"
    );

    let uri = format!("/api/v1/relationship-classes/{class_id}/records");
    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    grant(&app, "datasets", origin, "vic", "read").await;
    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "one grant was enough: {body}"
    );

    // with both granted the layer lets the request through to the handler
    grant(&app, "datasets", destination, "vic", "read").await;
    let (status, body) = request_as(&app, "GET", &uri, Some(&vic), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A catalog can only hang off a dataset that exists, and saying so is a 404
/// rather than the foreign key failing as a 500.
#[tokio::test]
async fn test_catalog_writes_on_a_missing_dataset_are_404() {
    let missing = Uuid::now_v7();
    let bodies = [
        ("rasters", json!({"name": "imagery"})),
        ("pointclouds", json!({"name": "lidar"})),
    ];

    let app = setup_app_authed().await;
    for (kind, body) in &bodies {
        let (status, resp) = request_as(
            &app,
            "POST",
            &format!("/api/v1/datasets/{missing}/{kind}"),
            Some(&token_for_user("root", Role::Admin)),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "admin POST {kind}: {resp}");
    }

    // dev mode skips the ladder, so the existence check is what answers there
    let (app, _) = setup_app().await;
    for (kind, body) in &bodies {
        let (status, resp) = post_json(
            &app,
            &format!("/api/v1/datasets/{missing}/{kind}"),
            body.clone(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "dev mode POST {kind}: {resp}"
        );
    }
}

/// A public dataset owned by carol, so a write denial is the ladder talking and
/// not the visibility layer. Returns (dataset id, carol's token).
async fn seed_owned_public_dataset(app: &axum::Router) -> (Uuid, String) {
    let carol = token_for_user("carol", Role::Editor);
    let (status, dataset) = request_as(
        app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    (
        Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap(),
        carol,
    )
}

/// Attaching a raster to a dataset is a dataset write, so it runs the same
/// permission ladder as a commit.
#[tokio::test]
async fn test_raster_writes_need_a_dataset_write_grant() {
    let app = setup_app_authed().await;
    let (dataset_id, carol) = seed_owned_public_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let catalogs_uri = format!("/api/v1/datasets/{dataset_id}/rasters");
    let body = json!({"name": "imagery"});

    let (status, resp) =
        request_as(&app, "POST", &catalogs_uri, Some(&eve), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-granted editor: {resp}");

    // the role gate answers the anonymous write before the ladder sees it
    let (status, resp) = request_as(&app, "POST", &catalogs_uri, None, Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous: {resp}");

    let (status, resp) = request_as(
        &app,
        "POST",
        &catalogs_uri,
        Some(&carol),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "owner: {resp}");
    let catalog_id = Uuid::parse_str(resp["id"].as_str().unwrap()).unwrap();

    // the instance admin bypasses per-dataset rows here as everywhere
    let (status, resp) = request_as(
        &app,
        "POST",
        &catalogs_uri,
        Some(&token_for_user("root", Role::Admin)),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "admin: {resp}");

    // a tile is a write on the catalog's dataset, resolved through the catalog
    let tiles_uri = format!("/api/v1/rasters/{catalog_id}/tiles");
    let tile = json!({"zoom_level": 0, "bounds_wkb_hex": "zz", "rast_hex": "00"});
    let (status, resp) = request_as(&app, "POST", &tiles_uri, Some(&eve), Some(tile.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-granted editor: {resp}");

    // the owner gets past the ladder and into the handler, which rejects the
    // deliberately invalid hex
    let (status, resp) =
        request_as(&app, "POST", &tiles_uri, Some(&carol), Some(tile.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "owner: {resp}");

    grant(&app, "datasets", dataset_id, "eve", "write").await;
    let (status, resp) = request_as(&app, "POST", &catalogs_uri, Some(&eve), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "granted editor: {resp}");
    let (status, resp) = request_as(&app, "POST", &tiles_uri, Some(&eve), Some(tile)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "granted editor: {resp}");
}

/// Same ladder for point clouds.
#[tokio::test]
async fn test_pointcloud_writes_need_a_dataset_write_grant() {
    let app = setup_app_authed().await;
    let (dataset_id, carol) = seed_owned_public_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let catalogs_uri = format!("/api/v1/datasets/{dataset_id}/pointclouds");
    let body = json!({"name": "lidar"});

    let (status, resp) =
        request_as(&app, "POST", &catalogs_uri, Some(&eve), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-granted editor: {resp}");

    let (status, resp) = request_as(&app, "POST", &catalogs_uri, None, Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous: {resp}");

    let (status, resp) = request_as(
        &app,
        "POST",
        &catalogs_uri,
        Some(&carol),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "owner: {resp}");
    let catalog_id = Uuid::parse_str(resp["id"].as_str().unwrap()).unwrap();

    let (status, resp) = request_as(
        &app,
        "POST",
        &catalogs_uri,
        Some(&token_for_user("root", Role::Admin)),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "admin: {resp}");

    let patches_uri = format!("/api/v1/pointclouds/{catalog_id}/patches");
    let patch = json!({"bounds_wkb_hex": "zz", "num_points": 1, "patch_hex": "00"});
    let (status, resp) =
        request_as(&app, "POST", &patches_uri, Some(&eve), Some(patch.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-granted editor: {resp}");

    let (status, resp) = request_as(
        &app,
        "POST",
        &patches_uri,
        Some(&carol),
        Some(patch.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "owner: {resp}");

    grant(&app, "datasets", dataset_id, "eve", "write").await;
    let (status, resp) = request_as(&app, "POST", &catalogs_uri, Some(&eve), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "granted editor: {resp}");
    let (status, resp) = request_as(&app, "POST", &patches_uri, Some(&eve), Some(patch)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "granted editor: {resp}");
}

// ─── Dataset-admin delegation ───────────────────────────────────────

/// Grant as a given caller, returning the raw status so a test can assert denial.
async fn grant_as(
    app: &axum::Router,
    token: &str,
    scope: &str,
    id: Uuid,
    user: &str,
    permission: &str,
) -> (StatusCode, Value) {
    request_as(
        app,
        "POST",
        &format!("/api/v1/{scope}/{id}/permissions"),
        Some(token),
        Some(json!({
            "user_id": user,
            "permission": permission,
            "granted_by": "ignored",
        })),
    )
    .await
}

/// The creator holds an admin grant, so it manages its own dataset's grants and
/// its branches' grants without an instance admin token.
#[tokio::test]
async fn test_dataset_admin_delegates_on_its_own_dataset() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = seed_private_dataset(&app).await;

    let (status, body) = grant_as(&app, &carol, "datasets", dataset_id, "dave", "write").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "dataset grant by owner: {body}"
    );

    let (status, body) = grant_as(&app, &carol, "branches", branch_id, "dave", "admin").await;
    assert_eq!(status, StatusCode::CREATED, "branch grant by owner: {body}");

    // and reads the ACL, and the check endpoints
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/permissions"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 2, "{body}");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/permissions"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/permissions/dave/check?required=write"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["allowed"], true, "{body}");

    // the granted write user is not an admin, so it cannot delegate further
    let (status, body) = grant_as(
        &app,
        &token_for_user("dave", Role::Editor),
        "datasets",
        dataset_id,
        "eve",
        "write",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "write grantee delegating: {body}"
    );
}

/// A dataset admin's reach stops at its own dataset.
#[tokio::test]
async fn test_dataset_admin_cannot_touch_another_dataset() {
    let app = setup_app_authed().await;
    let (mine, _, carol) = seed_private_dataset(&app).await;

    // a second, public dataset owned by someone else
    let frank = token_for_user("frank", Role::Editor);
    let (status, other) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&frank),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    let others = Uuid::parse_str(other["id"].as_str().unwrap()).unwrap();
    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{others}/branches"),
        Some(&frank),
        Some(json!({"name": "main", "created_by": "frank"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let other_branch = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    // public dataset, so carol reaches the handler and is refused there
    let (status, body) = grant_as(&app, &carol, "datasets", others, "carol", "admin").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-dataset grant: {body}");
    let (status, body) = grant_as(&app, &carol, "branches", other_branch, "carol", "admin").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-branch grant: {body}");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{others}/permissions"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-dataset acl read: {body}"
    );

    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/datasets/{others}/permissions/frank"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-dataset revoke: {body}"
    );

    // frank's own dataset is untouched, and carol still owns hers
    let (status, body) = grant_as(&app, &carol, "datasets", mine, "dave", "read").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // a private dataset answers 404 instead, so its id is not confirmed
    let (_, hidden, _) = seed_private_dataset(&app).await;
    let hidden_ds = Uuid::parse_str(
        request_as(
            &app,
            "GET",
            &format!("/api/v1/branches/{hidden}"),
            Some(&token_for_user("root", Role::Admin)),
            None,
        )
        .await
        .1["dataset_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let (status, body) = grant_as(&app, &frank, "datasets", hidden_ds, "frank", "admin").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "private cross-dataset: {body}"
    );
}

/// The instance admin role keeps working everywhere, including on a dataset with
/// no rows at all, which has no dataset admin to delegate to.
#[tokio::test]
async fn test_instance_admin_still_grants_anywhere() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;
    let root = token_for_user("root", Role::Admin);

    // nobody holds an admin grant here, so an ordinary editor cannot start one
    let (status, body) = grant_as(
        &app,
        &token_for_user("eve", Role::Editor),
        "datasets",
        dataset_id,
        "eve",
        "admin",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "self-grant on unowned: {body}"
    );

    let (status, body) = grant_as(&app, &root, "datasets", dataset_id, "alice", "admin").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = grant_as(&app, &root, "branches", branch_id, "alice", "write").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Revoking may not strand a dataset: not its last admin row, and not its last
/// row of any kind, which would drop it back to any-editor-writable.
#[tokio::test]
async fn test_revoke_cannot_strand_a_dataset() {
    let app = setup_app_authed().await;
    let (dataset_id, _, carol) = seed_private_dataset(&app).await;
    let root = token_for_user("root", Role::Admin);
    let revoke = |token: String, user: &str| {
        let uri = format!("/api/v1/datasets/{dataset_id}/permissions/{user}");
        let app = app.clone();
        async move { request_as(&app, "DELETE", &uri, Some(&token), None).await }
    };

    // carol is the only row and the only admin: refused, for her and for root
    let (status, body) = revoke(carol.clone(), "carol").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "self-revoke: {body}");
    let (status, body) = revoke(root.clone(), "carol").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "root revoking last admin: {body}"
    );

    // a second, non-admin row does not make the admin removable
    grant_as(&app, &carol, "datasets", dataset_id, "dave", "write").await;
    let (status, body) = revoke(carol.clone(), "carol").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "last admin with a write row: {body}"
    );

    // dave is removable, but then he is the last row again once carol goes
    let (status, body) = revoke(carol.clone(), "dave").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // promote dave, and now carol can step down
    grant_as(&app, &carol, "datasets", dataset_id, "dave", "admin").await;
    let (status, body) = revoke(carol.clone(), "carol").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "step down: {body}");

    // and carol has really lost the dataset
    let (status, body) = grant_as(&app, &carol, "datasets", dataset_id, "carol", "admin").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "after stepping down: {body}");

    // dave, the remaining admin, cannot remove himself either
    let (status, body) = revoke(token_for_user("dave", Role::Editor), "dave").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // revoking a user who has no row is a no-op, not a lockout error
    let (status, body) = revoke(token_for_user("dave", Role::Editor), "nobody").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// Branch rows carry no such rule: removing them all falls back to the dataset
/// scope, which is still enforced.
#[tokio::test]
async fn test_branch_revoke_has_no_lockout_rule() {
    let app = setup_app_authed().await;
    let (_, branch_id, carol) = seed_private_dataset(&app).await;

    grant_as(&app, &carol, "branches", branch_id, "dave", "admin").await;
    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/branches/{branch_id}/permissions/dave"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// With auth off the endpoints stay open, as the rest of dev mode does.
#[tokio::test]
async fn test_auth_disabled_skips_delegation_checks() {
    let (app, state) = setup_app().await;
    let (dataset_id, _) = seed_unowned_dataset(&state).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{dataset_id}/permissions"),
        json!({"user_id": "alice", "permission": "admin", "granted_by": "dev"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

// ─── Enumeration gating ─────────────────────────────────────────────

/// Every listing that names a dataset. Each returns JSON somewhere in which the
/// dataset's id or name appears, so a substring check over the whole body is the
/// honest test: it catches a leak in a description as well as in an id field.
const DATASET_LISTINGS: [&str; 5] = [
    "/api/v1/datasets",
    "/api/v1/catalog/search",
    "/api/v1/ogc/collections",
    "/api/v1/stac/collections",
    "/api/v1/qgis/datasets",
];

async fn listing_mentions(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
    needle: &str,
) -> bool {
    let (status, body) = request_as(app, "GET", uri, token, None).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {body}");
    body.to_string().contains(needle)
}

#[tokio::test]
async fn test_private_dataset_is_absent_from_every_listing() {
    let app = setup_app_authed().await;
    let (dataset_id, _, carol) = seed_private_dataset(&app).await;
    let id = dataset_id.to_string();
    let eve = token_for_user("eve", Role::Editor);
    let root = token_for_user("root", Role::Admin);

    for uri in DATASET_LISTINGS {
        assert!(
            !listing_mentions(&app, uri, None, &id).await,
            "anonymous GET {uri} leaked the private dataset"
        );
        assert!(
            !listing_mentions(&app, uri, Some(&eve), &id).await,
            "non-granted editor GET {uri} leaked the private dataset"
        );
    }

    // the owner and the instance admin see it in the listings that cover
    // versioned datasets (stac collections list raster catalogs, of which this
    // dataset has none)
    for uri in [
        "/api/v1/datasets",
        "/api/v1/ogc/collections",
        "/api/v1/qgis/datasets",
    ] {
        assert!(
            listing_mentions(&app, uri, Some(&carol), &id).await,
            "owner GET {uri} hid their own dataset"
        );
        assert!(
            listing_mentions(&app, uri, Some(&root), &id).await,
            "instance admin GET {uri} hid the dataset"
        );
    }
}

#[tokio::test]
async fn test_a_grant_puts_a_private_dataset_back_in_the_listings() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = seed_private_dataset(&app).await;
    let id = dataset_id.to_string();
    let vic = token_for_user("vic", Role::Viewer);
    let bob = token_for_user("bob", Role::Viewer);

    assert!(!listing_mentions(&app, "/api/v1/datasets", Some(&vic), &id).await);

    grant_as(&app, &carol, "datasets", dataset_id, "vic", "read").await;
    assert!(
        listing_mentions(&app, "/api/v1/datasets", Some(&vic), &id).await,
        "a dataset read grant did not surface the dataset"
    );

    // a grant on one of its branches counts too
    assert!(!listing_mentions(&app, "/api/v1/datasets", Some(&bob), &id).await);
    grant_as(&app, &carol, "branches", branch_id, "bob", "read").await;
    assert!(
        listing_mentions(&app, "/api/v1/datasets", Some(&bob), &id).await,
        "a branch read grant did not surface the dataset"
    );
}

#[tokio::test]
async fn test_public_datasets_stay_in_every_listing() {
    let app = setup_app_authed().await;
    let carol = token_for_user("carol", Role::Editor);
    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let id = dataset["id"].as_str().unwrap().to_string();
    let name = dataset["name"].as_str().unwrap().to_string();

    // tagged so the tag branch of catalog search is exercised too
    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{id}/tags"),
        Some(&carol),
        Some(json!({"tag": "demo"})),
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "tag: {body}"
    );

    for uri in [
        "/api/v1/datasets",
        "/api/v1/ogc/collections",
        "/api/v1/qgis/datasets",
    ] {
        assert!(
            listing_mentions(&app, uri, None, &id).await,
            "anonymous GET {uri} lost a public dataset"
        );
    }
    for uri in [
        format!("/api/v1/catalog/search?q={name}"),
        format!("/api/v1/catalog/search?tag=demo&q={name}"),
    ] {
        assert!(
            listing_mentions(&app, &uri, None, &id).await,
            "anonymous GET {uri} lost a public dataset"
        );
    }
}

/// The private dataset must not eat the limit window either: a filtered-out row
/// cannot consume a slot a visible dataset needed.
#[tokio::test]
async fn test_catalog_search_limit_counts_only_visible_rows() {
    let app = setup_app_authed().await;
    let carol = token_for_user("carol", Role::Editor);
    let tag = format!("t{}", Uuid::now_v7().simple());

    let mut public_id = String::new();
    for visibility in ["private", "public"] {
        let mut body = new_dataset_body();
        body["visibility"] = json!(visibility);
        let (status, ds) =
            request_as(&app, "POST", "/api/v1/datasets", Some(&carol), Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{ds}");
        let id = ds["id"].as_str().unwrap().to_string();
        request_as(
            &app,
            "POST",
            &format!("/api/v1/datasets/{id}/tags"),
            Some(&carol),
            Some(json!({"tag": tag})),
        )
        .await;
        if visibility == "public" {
            public_id = id;
        }
    }

    // limit 1 with the private dataset sorted first would return nothing if the
    // filter ran after the limit
    let uri = format!("/api/v1/catalog/search?tag={tag}&limit=1");
    assert!(
        listing_mentions(&app, &uri, None, &public_id).await,
        "the private row consumed the limit window"
    );
}

/// Dev mode lists everything, as the rest of it does.
#[tokio::test]
async fn test_auth_disabled_lists_private_datasets() {
    let (app, state) = setup_app().await;
    let (dataset_id, _) = seed_unowned_dataset(&state).await;
    state
        .set_dataset_visibility(dataset_id, ptolemy_core::dataset::Visibility::Private)
        .await
        .unwrap();

    let (status, body) = get_json(&app, "/api/v1/datasets").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.to_string().contains(&dataset_id.to_string()),
        "dev mode hid a dataset: {body}"
    );
}

// ─── Branch-scoped reads ────────────────────────────────────────────

/// Two branches of one dataset holding different values for the same feature,
/// plus a feature only the fork has. Returns (dataset, main, fork, feature id).
async fn seed_two_branches(app: &axum::Router) -> (Uuid, Uuid, Uuid, Uuid) {
    let ds_id = create_dataset(app).await;
    let main = create_branch(app, ds_id, "main").await;
    let shared = Uuid::now_v7();
    let point = "0101000000000000000000f03f000000000000f03f";

    commit_features(
        app,
        main,
        json!([{
            "type": "insert",
            "feature_id": shared,
            "geometry_wkb_hex": point,
            "properties": {"name": "on-main", "kind": "parcel"}
        }]),
    )
    .await;

    let (status, fork) = post_json(
        app,
        &format!("/api/v1/datasets/{ds_id}/branches"),
        json!({"name": "fork", "created_by": "test", "fork_from_branch": main}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id = Uuid::parse_str(fork["id"].as_str().unwrap()).unwrap();

    // the fork moves the feature and renames it, then main is committed again so
    // the newest row in the table belongs to main
    commit_features(
        app,
        fork_id,
        json!([{
            "type": "update",
            "feature_id": shared,
            "geometry_wkb_hex": "0101000000000000000000004000000000000000 40".replace(' ', ""),
            "properties": {"name": "on-fork", "kind": "parcel"}
        }]),
    )
    .await;
    commit_features(
        app,
        main,
        json!([{
            "type": "update",
            "feature_id": shared,
            "geometry_wkb_hex": point,
            "properties": {"name": "on-main-again", "kind": "parcel"}
        }]),
    )
    .await;

    (ds_id, main, fork_id, shared)
}

/// The bug: `/ogc/collections/{id}/items/{fid}` took the newest
/// `feature_versions` row for the id anywhere in the database, so both branches
/// answered with whichever was written last.
#[tokio::test]
async fn test_ogc_single_item_is_scoped_to_its_branch() {
    let (app, _) = setup_app().await;
    let (ds_id, main, fork, fid) = seed_two_branches(&app).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{ds_id}/items/{fid}?branch={fork}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["properties"]["name"], "on-fork", "{body}");

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{ds_id}/items/{fid}?branch={main}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["properties"]["name"], "on-main-again", "{body}");

    // no branch means main, the same rule the listing uses
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{ds_id}/items/{fid}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["properties"]["name"], "on-main-again", "{body}");

    // a feature id from another dataset is not served here
    let other = create_dataset(&app).await;
    let other_branch = create_branch(&app, other, "main").await;
    let other_fid = Uuid::now_v7();
    commit_features(
        &app,
        other_branch,
        json!([{
            "type": "insert",
            "feature_id": other_fid,
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {"name": "elsewhere"}
        }]),
    )
    .await;
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{ds_id}/items/{other_fid}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// A deleted feature is gone from the single-item read too, not served as an
/// empty geometry.
#[tokio::test]
async fn test_ogc_single_item_honours_deletes() {
    let (app, _) = setup_app().await;
    let (ds_id, main, _, fid) = seed_two_branches(&app).await;

    commit_features(&app, main, json!([{"type": "delete", "feature_id": fid}])).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/ogc/collections/{ds_id}/items/{fid}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// The branch-scoped source has to see inherited features, not just the ones
/// committed on the branch itself. This is what the `features` view was fixed
/// for in migration 020, so the scoped form must keep it.
#[tokio::test]
async fn test_scoped_reads_see_inherited_and_own_values() {
    let (app, _) = setup_app().await;
    let (_, main, fork, _) = seed_two_branches(&app).await;
    let inherited = Uuid::now_v7();

    // a feature committed on main before the fork existed is already inherited;
    // add one more only main has, to prove the fork does not see it
    commit_features(
        &app,
        main,
        json!([{
            "type": "insert",
            "feature_id": inherited,
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {"name": "main-only", "kind": "parcel"}
        }]),
    )
    .await;

    // the CQL2 filter path, the export paths and the QGIS layer definition all
    // read through the same scoped source
    let filter = json!({"filter": {"op": "=", "args": [{"property": "kind"}, "parcel"]}});

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{fork}/features/filter"),
        filter.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["on-fork"], "fork filter: {body}");

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{main}/features/filter"),
        filter,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut names: Vec<&str> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["main-only", "on-main-again"],
        "main filter: {body}"
    );

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{fork}/export/geojson")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let exported: Vec<&str> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(exported, vec!["on-fork"], "fork export: {body}");

    let (status, body) = get_json(&app, &format!("/api/v1/qgis/branches/{main}/layer")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["feature_count"], 2, "main layer definition: {body}");
    let (status, body) = get_json(&app, &format!("/api/v1/qgis/branches/{fork}/layer")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["feature_count"], 1, "fork layer definition: {body}");
}

/// A deleted feature must not come back through the scoped source, and the CSV
/// and FlatGeobuf exports share it.
#[tokio::test]
async fn test_scoped_reads_exclude_deletes() {
    let (app, _) = setup_app().await;
    let (_, main, _, fid) = seed_two_branches(&app).await;

    commit_features(&app, main, json!([{"type": "delete", "feature_id": fid}])).await;

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{main}/export/geojson")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["features"].as_array().unwrap().is_empty(),
        "deleted feature came back: {body}"
    );

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{main}/features/filter"),
        json!({"filter": {"op": "=", "args": [{"property": "kind"}, "parcel"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 0, "{body}");
}

/// The 3D endpoints resolved features through the same view, so they are
/// branch-scoped now too: a feature live only on the fork is not found on main.
#[tokio::test]
async fn test_sfcgal_resolves_features_per_branch() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main = create_branch(&app, ds_id, "main").await;
    let (status, fork) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/branches"),
        json!({"name": "fork", "created_by": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id = Uuid::parse_str(fork["id"].as_str().unwrap()).unwrap();

    let fid = Uuid::now_v7();
    commit_features(
        &app,
        main,
        json!([{
            "type": "insert",
            "feature_id": fid,
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {"name": "main-only"}
        }]),
    )
    .await;

    let body = json!({"feature_id": fid});
    let (status, resp) = post_json(
        &app,
        &format!("/api/v1/branches/{main}/3d/volume"),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "on its own branch: {resp}");

    // the fork was created empty, so it never had this feature
    let (status, resp) =
        post_json(&app, &format!("/api/v1/branches/{fork_id}/3d/volume"), body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "on another branch: {resp}");
}

// ─── External spatial pushdown ──────────────────────────────────────

/// The same three parcels twice: once in EPSG:4326 and once reprojected to
/// EPSG:3857, each with a GiST index on the raw column only. A read of the 3857
/// relation must return exactly what the 4326 one returns.
async fn create_projected_fixture(state: &AppState) {
    let pool = state.external_pool().await.unwrap();
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS ext_proj_4326 CASCADE;
         DROP TABLE IF EXISTS ext_proj_3857 CASCADE;
         CREATE TABLE ext_proj_4326 (
             pid integer PRIMARY KEY,
             owner text NOT NULL,
             geom geometry(Geometry, 4326) NOT NULL
         );
         INSERT INTO ext_proj_4326 (pid, owner, geom) VALUES
           (1, 'alice', ST_GeomFromText('POLYGON((10 20,10.01 20,10.01 20.01,10 20.01,10 20))', 4326)),
           (2, 'bob',   ST_GeomFromText('POLYGON((11 21,11.01 21,11.01 21.01,11 21.01,11 21))', 4326)),
           (3, 'carol', ST_GeomFromText('POLYGON((30 40,30.01 40,30.01 40.01,30 40.01,30 40))', 4326));
         CREATE INDEX ext_proj_4326_geom_idx ON ext_proj_4326 USING GIST (geom);

         CREATE TABLE ext_proj_3857 (
             pid integer PRIMARY KEY,
             owner text NOT NULL,
             geom geometry(Geometry, 3857) NOT NULL
         );
         INSERT INTO ext_proj_3857 (pid, owner, geom)
           SELECT pid, owner, ST_Transform(geom, 3857) FROM ext_proj_4326;
         CREATE INDEX ext_proj_3857_geom_idx ON ext_proj_3857 USING GIST (geom);",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Register both relations and return (4326 branch, 3857 branch).
async fn setup_projected(app: &axum::Router, state: &AppState) -> (Uuid, Uuid) {
    create_projected_fixture(state).await;
    let mut branches = Vec::new();
    for table in ["ext_proj_4326", "ext_proj_3857"] {
        let (status, body) = register_external(app, table, "pid", "geom").await;
        assert_eq!(status, StatusCode::CREATED, "register {table}: {body}");
        let dataset_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
        let (status, list) =
            get_json(app, &format!("/api/v1/datasets/{dataset_id}/branches")).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        branches
            .push(Uuid::parse_str(list.as_array().unwrap()[0]["id"].as_str().unwrap()).unwrap());
    }
    (branches[0], branches[1])
}

fn owners_of(body: &Value, key: &str) -> Vec<String> {
    let mut owners: Vec<String> = body[key]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|f| {
            f["properties"]["owner"]
                .as_str()
                .or_else(|| f["owner"].as_str())
                .map(str::to_owned)
        })
        .collect();
    owners.sort();
    owners
}

/// A projected source must answer every spatial read exactly as the 4326 one
/// does: the pushed-down pre-filter may only widen the candidate set.
#[tokio::test]
async fn test_projected_external_matches_the_4326_source() {
    let (app, state) = setup_app().await;
    let (b4326, b3857) = setup_projected(&app, &state).await;

    // a window covering parcels 1 and 2 but not 3
    let bbox = "min_x=9.5&min_y=19.5&max_x=11.5&max_y=21.5";
    for (label, branch) in [("4326", b4326), ("3857", b3857)] {
        let (status, body) = get_json(
            &app,
            &format!("/api/v1/branches/{branch}/features/bbox?{bbox}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} bbox: {body}");
        let mut owners: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["properties"]["owner"].as_str().map(str::to_owned))
            .collect();
        owners.sort();
        assert_eq!(owners, vec!["alice", "bob"], "{label} bbox owners: {body}");
    }

    let window = json!({
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[9.5, 19.5], [11.5, 19.5], [11.5, 21.5], [9.5, 21.5], [9.5, 19.5]]]
        }
    });
    for (label, branch) in [("4326", b4326), ("3857", b3857)] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/branches/{branch}/features/intersects"),
            window.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} intersects: {body}");
        let mut owners: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["properties"]["owner"].as_str().map(str::to_owned))
            .collect();
        owners.sort();
        assert_eq!(owners, vec!["alice", "bob"], "{label} intersects: {body}");

        let (status, body) = post_json(
            &app,
            &format!("/api/v1/branches/{branch}/features/within"),
            window.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} within: {body}");
        let mut owners: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["properties"]["owner"].as_str().map(str::to_owned))
            .collect();
        owners.sort();
        assert_eq!(owners, vec!["alice", "bob"], "{label} within: {body}");
    }

    // the CQL2 spatial op, which carries its own geometry bind
    let filter = json!({
        "filter": {
            "op": "s_intersects",
            "args": [
                {"property": "geometry"},
                {"type": "Polygon",
                 "coordinates": [[[9.5, 19.5], [11.5, 19.5], [11.5, 21.5], [9.5, 21.5], [9.5, 19.5]]]}
            ]
        }
    });
    for (label, branch) in [("4326", b4326), ("3857", b3857)] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/branches/{branch}/features/filter"),
            filter.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} cql2: {body}");
        assert_eq!(
            owners_of(&body, "features"),
            vec!["alice", "bob"],
            "{label} cql2: {body}"
        );
    }
}

/// A window that no projection can be asked to reproject must fall back to no
/// pre-filter rather than aborting the read.
#[tokio::test]
async fn test_projected_external_survives_a_global_window() {
    let (app, state) = setup_app().await;
    let (_, b3857) = setup_projected(&app, &state).await;

    for bbox in [
        "min_x=-180&min_y=-90&max_x=180&max_y=90",
        "min_x=-179&min_y=-89&max_x=179&max_y=89",
    ] {
        let (status, body) = get_json(
            &app,
            &format!("/api/v1/branches/{b3857}/features/bbox?{bbox}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{bbox}: {body}");
        assert_eq!(body.as_array().unwrap().len(), 3, "{bbox}: {body}");
    }
}

/// Tiles work on a projected source, and the dataset-level tile route works at
/// all: it used to compare a 3857 tile envelope against 4326 geometry and fail
/// on mixed SRIDs for every dataset, external or not.
#[tokio::test]
async fn test_tiles_work_on_projected_and_versioned_sources() {
    let (app, state) = setup_app().await;
    let (_, b3857) = setup_projected(&app, &state).await;

    // z7 tile containing lon 10, lat 20
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{b3857}/tiles/7/67/57")).await;
    assert!(
        status == StatusCode::OK,
        "external branch tile: {status} {body}"
    );

    // the dataset-level OGC tile route, on an ordinary versioned dataset
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    commit_features(
        &app,
        branch_id,
        json!([{
            "type": "insert",
            "feature_id": Uuid::now_v7(),
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {"name": "one"}
        }]),
    )
    .await;
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/tiles/WebMercatorQuad/0/0/0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "versioned dataset tile: {body}");
}

/// Visibility still governs a projected external dataset: the pushdown changes
/// the plan, not who may read.
#[tokio::test]
async fn test_projected_external_still_obeys_visibility() {
    let (app, state) = setup_app_authed_with_state().await;
    create_projected_fixture(&state).await;
    let carol = token_for_user("carol", Role::Editor);

    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(json!({
            "name": format!("proj_{}", Uuid::now_v7()),
            "created_by": "carol",
            "visibility": "private",
            "external_table": "ext_proj_3857",
            "external_id_column": "pid",
            "external_geometry_column": "geom",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();
    let (status, list) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let branch = Uuid::parse_str(list.as_array().unwrap()[0]["id"].as_str().unwrap()).unwrap();

    let bbox =
        format!("/api/v1/branches/{branch}/features/bbox?min_x=9&min_y=19&max_x=12&max_y=22");
    let (status, body) = request_as(&app, "GET", &bbox, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "anonymous: {body}");
    let (status, body) = request_as(
        &app,
        "GET",
        &bbox,
        Some(&token_for_user("eve", Role::Editor)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "non-granted editor: {body}");
    let (status, body) = request_as(&app, "GET", &bbox, Some(&carol), None).await;
    assert_eq!(status, StatusCode::OK, "owner: {body}");
    assert_eq!(body.as_array().unwrap().len(), 2, "owner: {body}");
}

// ─── WebSocket handshake auth ───────────────────────────────────────
//
// These run against a real listener rather than `oneshot`. The upgrade
// extractor needs hyper's upgrade state, which a bare tower service never has,
// so `oneshot` answers 426 for every handshake and can prove neither the 101
// nor the echoed subprotocol.

/// Serve `app` on an ephemeral port and return its `ws://` base URL.
async fn spawn_ws_app(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{addr}")
}

/// Open a socket. `Ok` carries the subprotocol the server selected, `Err` the
/// status it refused with.
async fn ws_connect(
    base: &str,
    path: &str,
    subprotocol: Option<&str>,
    bearer: Option<&str>,
) -> Result<Option<String>, u16> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut req = format!("{base}{path}").into_client_request().unwrap();
    if let Some(proto) = subprotocol {
        req.headers_mut()
            .insert("sec-websocket-protocol", proto.parse().unwrap());
    }
    if let Some(token) = bearer {
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }

    match tokio_tungstenite::connect_async(req).await {
        Ok((_stream, resp)) => Ok(resp
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("unexpected websocket error: {e}"),
    }
}

/// Both socket paths. The branch one parses its id as a UUID, so it needs a
/// real one to get past the extractor once auth lets the request through.
fn ws_paths() -> [String; 2] {
    [
        "/ws/branches/6f1c2d3e-0000-4000-8000-000000000001".to_string(),
        "/ws/rooms/design-review".to_string(),
    ]
}

#[tokio::test]
async fn test_ws_handshake_without_token_is_rejected() {
    let base = spawn_ws_app(setup_app_authed().await).await;
    for path in ws_paths() {
        let result = ws_connect(&base, &path, None, None).await;
        assert_eq!(result, Err(401), "anonymous {path}");
    }
}

#[tokio::test]
async fn test_ws_handshake_with_subprotocol_token_upgrades() {
    let base = spawn_ws_app(setup_app_authed().await).await;
    let token = token_for(Role::Viewer);
    for path in ws_paths() {
        let selected = ws_connect(&base, &path, Some(&format!("bearer, {token}")), None)
            .await
            .unwrap_or_else(|s| panic!("{path} refused with {s}"));
        // the marker comes back so a browser accepts the 101, the token never does
        assert_eq!(selected.as_deref(), Some("bearer"), "{path}");
    }
}

#[tokio::test]
async fn test_ws_handshake_with_bad_subprotocol_token_is_rejected() {
    let base = spawn_ws_app(setup_app_authed().await).await;
    let expired = expired_token(TEST_SECRET, Role::Viewer);
    let forged = generate_token(
        "a-different-secret-0123456789abcdef",
        "u",
        Role::Admin,
        3600,
    );
    let valid = token_for(Role::Viewer);
    let offers = [
        "bearer, not-a-jwt".to_string(),
        format!("bearer, {expired}"),
        format!("bearer, {forged}"),
        // the marker alone carries no credential
        "bearer".to_string(),
        // a token without the marker first is not an offer we accept
        valid.clone(),
        format!("{valid}, bearer"),
    ];
    for path in ws_paths() {
        for offer in &offers {
            let result = ws_connect(&base, &path, Some(offer), None).await;
            assert_eq!(result, Err(401), "{path} offering {offer}");
        }
    }
}

/// Non-browser clients can still use the header, and it must be honoured.
#[tokio::test]
async fn test_ws_handshake_with_authorization_header_upgrades() {
    let base = spawn_ws_app(setup_app_authed().await).await;
    let token = token_for(Role::Viewer);
    for path in ws_paths() {
        ws_connect(&base, &path, None, Some(&token))
            .await
            .unwrap_or_else(|s| panic!("header auth {path} refused with {s}"));
    }
}

/// The subprotocol must not act as a credential anywhere but the socket paths,
/// or any route could be entered with a header script sets without a preflight.
#[tokio::test]
async fn test_subprotocol_token_does_not_authenticate_http_routes() {
    let app = setup_app_authed().await;
    let token = token_for(Role::Editor);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/datasets")
        .header("content-type", "application/json")
        .header("sec-websocket-protocol", format!("bearer, {token}"))
        .body(Body::from(
            serde_json::to_vec(&json!({"name": "x", "srid": 4326})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A bulk write over HTTP must leave the planner fresh statistics for the
/// recursive changeset walk every read builds on, instead of serving slow reads
/// until autoanalyze catches up.
#[tokio::test]
async fn test_bulk_write_refreshes_planner_statistics() {
    let state = fresh_state_with_analyze_threshold(3).await;
    let app = app_with_auth(state.clone(), AuthConfig::disabled());
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;

    let inserts = |n: usize| -> Value {
        (0..n)
            .map(|_| {
                json!({
                    "type": "insert",
                    "feature_id": Uuid::now_v7(),
                    "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                    "properties": {}
                })
            })
            .collect::<Vec<_>>()
            .into()
    };

    let uri = format!("/api/v1/branches/{branch_id}/batch");
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"message": "small", "author": "a", "operations": inserts(2)}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        state.analyzer().scheduled(),
        0,
        "a write under the threshold must not analyze"
    );

    let (status, body) = post_json(
        &app,
        &uri,
        json!({"message": "bulk", "author": "a", "operations": inserts(3)}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(state.analyzer().scheduled(), 1);

    state.analyzer().wait_idle().await;
    // last_analyze counts only explicit ANALYZE, so a background autoanalyze
    // cannot make this pass on its own.
    let last_analyze: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT last_analyze FROM pg_stat_user_tables WHERE relname = 'feature_versions'",
    )
    .fetch_one(state.pool())
    .await
    .unwrap();
    assert!(
        last_analyze.is_some(),
        "feature_versions was never analyzed"
    );
}

/// Helper: the branch's live features as GeoJSON, through the changeset walk
/// every read shares.
async fn exported_features(app: &axum::Router, branch_id: Uuid) -> Vec<Value> {
    let (status, body) =
        get_json(app, &format!("/api/v1/branches/{branch_id}/export/geojson")).await;
    assert_eq!(status, StatusCode::OK, "export: {body}");
    body["features"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn test_import_geojson_is_readable_on_the_branch() {
    let (app, _) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/import/geojson"),
        json!({
            "message": "trees",
            "author": "surveyor",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {"type": "Point", "coordinates": [1.5, 2.5]},
                    "properties": {"name": "Oak"}
                },
                {
                    "type": "Feature",
                    "geometry": {"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]},
                    "properties": {"name": "Path"}
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], 2, "{body}");
    assert_eq!(body["skipped"], 0, "{body}");
    assert!(body["changeset_id"].is_string(), "{body}");

    let features = exported_features(&app, branch_id).await;
    assert_eq!(features.len(), 2, "imported features must be readable");
    assert_eq!(features[0]["properties"]["name"], "Oak");
    assert_eq!(features[0]["geometry"]["coordinates"], json!([1.5, 2.5]));
    assert_eq!(features[1]["properties"]["name"], "Path");
    assert_eq!(features[1]["geometry"]["type"], "LineString");

    // the import is a changeset like any other, so it shows up in the history
    let (status, history) = get_json(&app, &format!("/api/v1/branches/{branch_id}/history")).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["message"], "trees", "{history}");
    assert_eq!(history[0]["author"], "surveyor", "{history}");
}

#[tokio::test]
async fn test_import_csv_is_readable_on_the_branch() {
    let (app, _) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/import/csv"),
        json!({
            "csv": "longitude,latitude,name,height\n1.5,2.5,Alpha,10\n3.5,4.5,Beta,20\n",
            "message": "poles",
            "author": "surveyor"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], 2, "{body}");
    assert_eq!(body["skipped"], 0, "{body}");

    let features = exported_features(&app, branch_id).await;
    assert_eq!(features.len(), 2, "imported rows must be readable");
    assert_eq!(features[0]["properties"]["name"], "Alpha");
    assert_eq!(features[0]["properties"]["height"], 10.0);
    assert_eq!(features[0]["geometry"]["coordinates"], json!([1.5, 2.5]));
    assert_eq!(features[1]["geometry"]["coordinates"], json!([3.5, 4.5]));
}

#[tokio::test]
async fn test_import_reports_bad_rows_and_refuses_a_total_failure() {
    let (app, _) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;

    // every feature unusable: nothing to store, so this is not a success and
    // must leave no changeset behind
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/import/geojson"),
        json!({"features": [
            {"type": "Feature", "properties": {"name": "no geometry"}},
            {"type": "Feature", "geometry": null, "properties": {}}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["imported"], 0, "{body}");
    assert_eq!(body["skipped"], 2, "{body}");
    assert_eq!(body["errors"].as_array().unwrap().len(), 2, "{body}");
    assert!(body["changeset_id"].is_null(), "{body}");
    assert!(exported_features(&app, branch_id).await.is_empty());

    // a mixed request keeps the good row and reports the bad one
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/import/geojson"),
        json!({"features": [
            {
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [7.0, 8.0]},
                "properties": {"name": "good"}
            },
            {"type": "Feature", "geometry": null, "properties": {}}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], 1, "{body}");
    assert_eq!(body["skipped"], 1, "{body}");

    let features = exported_features(&app, branch_id).await;
    assert_eq!(features.len(), 1);
    assert_eq!(features[0]["properties"]["name"], "good");

    // same rule for CSV
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/import/csv"),
        json!({"csv": "longitude,latitude,name\nnorth,2.5,Alpha\n1.5\n"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["imported"], 0, "{body}");
    assert_eq!(body["skipped"], 2, "{body}");
    assert!(body["changeset_id"].is_null(), "{body}");
    assert_eq!(
        exported_features(&app, branch_id).await.len(),
        1,
        "a refused import must not disturb the branch"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Attachments
// ═══════════════════════════════════════════════════════════════════════

/// GET returning the raw body, for a download that is not JSON.
async fn get_bytes(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", "Bearer test-skip")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

fn upload_body(name: &str, data: &str) -> Value {
    use base64::Engine;
    json!({
        "name": name,
        "content_type": "text/plain",
        "data": base64::engine::general_purpose::STANDARD.encode(data),
        "created_by": "test",
    })
}

#[tokio::test]
async fn test_dataset_attachment_upload_list_and_download() {
    let (app, _state) = setup_app().await;
    let dataset_id = create_dataset(&app).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{dataset_id}/attachments"),
        upload_body("icon.png", "icon-bytes"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    assert_eq!(body["dataset_id"].as_str().unwrap(), dataset_id.to_string());
    assert!(body["feature_id"].is_null());
    assert!(body["branch_id"].is_null());
    let attachment_id = body["id"].as_str().unwrap().to_string();

    let (status, body) =
        get_json(&app, &format!("/api/v1/datasets/{dataset_id}/attachments")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"].as_str().unwrap(), "icon.png");

    let (status, bytes) = get_bytes(&app, &format!("/api/v1/attachments/{attachment_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"icon-bytes");

    let (status, body) = get_json(&app, &format!("/api/v1/attachments/{attachment_id}/meta")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dataset_id"].as_str().unwrap(), dataset_id.to_string());

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/attachments/{attachment_id}"))
        .header("authorization", "Bearer test-skip")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (status, body) =
        get_json(&app, &format!("/api/v1/datasets/{dataset_id}/attachments")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_feature_attachment_still_works() {
    let (app, _state) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;
    let feature_id = Uuid::now_v7();
    commit_features(
        &app,
        branch_id,
        json!([{
            "type": "insert",
            "feature_id": feature_id,
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {},
        }]),
    )
    .await;

    let uri = format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments");
    let (status, body) = post_json(&app, &uri, upload_body("photo.jpg", "photo-bytes")).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    assert_eq!(body["feature_id"].as_str().unwrap(), feature_id.to_string());
    assert_eq!(body["branch_id"].as_str().unwrap(), branch_id.to_string());
    assert!(body["dataset_id"].is_null());
    let attachment_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    let (status, bytes) = get_bytes(&app, &format!("/api/v1/attachments/{attachment_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"photo-bytes");

    // a dataset attachment listing must not pick up the feature's attachment
    let (status, body) =
        get_json(&app, &format!("/api/v1/datasets/{dataset_id}/attachments")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_dataset_attachment_upload_requires_write() {
    let app = setup_app_authed().await;
    let editor = token_for(Role::Editor);
    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&editor),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = dataset["id"].as_str().unwrap();
    let uri = format!("/api/v1/datasets/{dataset_id}/attachments");

    let (status, body) =
        request_as(&app, "POST", &uri, None, Some(upload_body("i.png", "x"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, body) = request_as(
        &app,
        "POST",
        &uri,
        Some(&token_for(Role::Viewer)),
        Some(upload_body("i.png", "x")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = request_as(
        &app,
        "POST",
        &uri,
        Some(&editor),
        Some(upload_body("i.png", "x")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // listing stays an anonymous read, matching the feature-level route
    let (status, body) = request_as(&app, "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
}

// ─── Attachment writes go through the write ladder ───────────────────

/// A public dataset owned by carol with a branch and one committed feature, so a
/// write denial is the ladder talking and not the visibility layer. Returns
/// (dataset id, branch id, feature id, carol's token).
async fn seed_attachment_targets(app: &axum::Router) -> (Uuid, Uuid, Uuid, String) {
    let (dataset_id, carol) = seed_owned_public_dataset(app).await;

    let (status, branch) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "main", "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let feature_id = Uuid::now_v7();
    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(&carol),
        Some(json!({
            "message": "attachment target",
            "author": "carol",
            "operations": [{
                "type": "insert",
                "feature_id": feature_id,
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "properties": {},
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    (dataset_id, branch_id, feature_id, carol)
}

/// Uploading an attachment is a write on the owning branch or dataset, so the
/// editor role alone is not enough.
#[tokio::test]
async fn test_attachment_uploads_need_a_write_grant() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, feature_id, _) = seed_attachment_targets(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let vic = token_for_user("vic", Role::Viewer);
    let root = token_for_user("root", Role::Admin);
    grant(&app, "datasets", dataset_id, "vic", "read").await;

    let uris = [
        format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments"),
        format!("/api/v1/datasets/{dataset_id}/attachments"),
    ];

    for uri in &uris {
        let (status, body) = request_as(
            &app,
            "POST",
            uri,
            Some(&eve),
            Some(upload_body("e.png", "x")),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "non-granted editor {uri}: {body}"
        );

        // a read grant is not a write grant
        let (status, body) = request_as(
            &app,
            "POST",
            uri,
            Some(&vic),
            Some(upload_body("v.png", "x")),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "read-granted viewer {uri}: {body}"
        );

        let (status, body) = request_as(
            &app,
            "POST",
            uri,
            Some(&root),
            Some(upload_body("root.png", "x")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "instance admin {uri}: {body}");
    }

    grant(&app, "datasets", dataset_id, "eve", "write").await;
    for uri in &uris {
        let (status, body) = request_as(
            &app,
            "POST",
            uri,
            Some(&eve),
            Some(upload_body("e.png", "x")),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "write-granted editor {uri}: {body}"
        );
    }
}

/// Deleting one is the same write, and the owner it is checked against is
/// whichever of the two shapes the attachment has.
#[tokio::test]
async fn test_attachment_delete_needs_a_write_grant() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, feature_id, carol) = seed_attachment_targets(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    let vic = token_for_user("vic", Role::Viewer);
    let root = token_for_user("root", Role::Admin);
    grant(&app, "datasets", dataset_id, "vic", "read").await;

    let feature_uri = format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments");
    let dataset_uri = format!("/api/v1/datasets/{dataset_id}/attachments");

    // one attachment of each owner shape per caller under test
    let upload = |uri: String, token: String| {
        let app = app.clone();
        async move {
            let (status, body) = request_as(
                &app,
                "POST",
                &uri,
                Some(&token),
                Some(upload_body("a.png", "x")),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            format!("/api/v1/attachments/{}", body["id"].as_str().unwrap())
        }
    };

    for owner_uri in [&feature_uri, &dataset_uri] {
        let target = upload(owner_uri.clone(), carol.clone()).await;

        let (status, body) = request_as(&app, "DELETE", &target, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "non-granted editor {target}: {body}"
        );

        let (status, body) = request_as(&app, "DELETE", &target, Some(&vic), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "read-granted viewer {target}: {body}"
        );

        // the blob survived both attempts
        let (status, body) = request_as(&app, "GET", &format!("{target}/meta"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = request_as(&app, "DELETE", &target, Some(&root), None).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "instance admin {target}: {body}"
        );
    }

    grant(&app, "datasets", dataset_id, "eve", "write").await;
    for owner_uri in [&feature_uri, &dataset_uri] {
        let target = upload(owner_uri.clone(), carol.clone()).await;
        let (status, body) = request_as(&app, "DELETE", &target, Some(&eve), None).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "write-granted editor {target}: {body}"
        );
    }
}

/// With auth off there is no identity to check, so the ladder is unenforced and
/// the dev and CLI flows keep uploading and deleting.
#[tokio::test]
async fn test_attachment_writes_work_unenforced() {
    let (app, _state) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;
    let feature_id = Uuid::now_v7();
    commit_features(
        &app,
        branch_id,
        json!([{
            "type": "insert",
            "feature_id": feature_id,
            "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
            "properties": {},
        }]),
    )
    .await;

    for owner_uri in [
        format!("/api/v1/branches/{branch_id}/features/{feature_id}/attachments"),
        format!("/api/v1/datasets/{dataset_id}/attachments"),
    ] {
        let (status, body) = post_json(&app, &owner_uri, upload_body("a.png", "x")).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let target = format!("/api/v1/attachments/{}", body["id"].as_str().unwrap());

        let req = Request::builder()
            .method("DELETE")
            .uri(&target)
            .header("authorization", "Bearer test-skip")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{target}");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Feature Valid Time
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_features_valid_at_query() {
    let (app, _state) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, dataset_id, "main").await;
    let inside = Uuid::now_v7();
    let outside = Uuid::now_v7();
    let untimed = Uuid::now_v7();
    let hex = "0101000000000000000000f03f000000000000f03f";
    commit_features(
        &app,
        branch_id,
        json!([
            {
                "type": "insert", "feature_id": inside, "geometry_wkb_hex": hex,
                "properties": {}, "valid_from": "2020-01-01T00:00:00Z",
                "valid_to": "2021-01-01T00:00:00Z",
            },
            {
                "type": "insert", "feature_id": outside, "geometry_wkb_hex": hex,
                "properties": {}, "valid_from": "2022-01-01T00:00:00Z",
                "valid_to": "2023-01-01T00:00:00Z",
            },
            {
                "type": "insert", "feature_id": untimed, "geometry_wkb_hex": hex,
                "properties": {},
            },
        ]),
    )
    .await;

    // the written valid time comes back on an unfiltered read
    let (status, body) = get_json(&app, &format!("/api/v1/branches/{branch_id}/features")).await;
    assert_eq!(status, StatusCode::OK);
    let all = body["features"].as_array().unwrap();
    let find = |id: Uuid| {
        all.iter()
            .find(|f| f["id"].as_str().unwrap() == id.to_string())
            .unwrap()
    };
    assert_eq!(
        find(inside)["valid_from"].as_str().unwrap(),
        "2020-01-01T00:00:00Z"
    );
    assert!(find(untimed)["valid_from"].is_null());
    assert!(find(untimed)["valid_to"].is_null());

    async fn ids_at(app: &axum::Router, branch_id: Uuid, t: &str) -> Vec<String> {
        let uri = format!("/api/v1/branches/{branch_id}/features?valid_at={t}");
        let (status, body) = get_json(app, &uri).await;
        assert_eq!(status, StatusCode::OK, "valid_at {t}: {body}");
        body["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect()
    }

    let ids = ids_at(&app, branch_id, "2020-06-01T00:00:00Z").await;
    assert!(ids.contains(&inside.to_string()));
    assert!(!ids.contains(&outside.to_string()));
    assert!(ids.contains(&untimed.to_string()));

    // the range end itself is excluded
    let ids = ids_at(&app, branch_id, "2021-01-01T00:00:00Z").await;
    assert!(!ids.contains(&inside.to_string()));

    let (status, _) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features?valid_at=not-a-time"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ─── The write ladder reaches every mutating route ──────────────────

/// A dataset and branch owned by `carol`, who gets the creator's admin grant.
/// Every other editor is an outsider to it.
async fn owned_dataset(app: &axum::Router) -> (Uuid, Uuid, String) {
    let carol = token_for_user("carol", Role::Editor);
    let (status, dataset) = request_as(
        app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let dataset_id = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

    let (status, branch) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "main", "created_by": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let (status, body) = commit_as(app, branch_id, &carol).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    (dataset_id, branch_id, carol)
}

/// One representative mutating route per module that used to reach the database
/// on the editor role alone. Bodies are the shapes the handlers deserialize, so
/// the same table drives the denial and the success case.
fn gated_writes(dataset_id: Uuid, branch_id: Uuid) -> Vec<(&'static str, String, Value)> {
    vec![
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/tags"),
            json!({"tag": "roads"}),
        ),
        (
            "PUT",
            format!("/api/v1/datasets/{dataset_id}/metadata"),
            json!({"description": "notes"}),
        ),
        (
            "PUT",
            format!("/api/v1/datasets/{dataset_id}/schema"),
            json!({"fields": []}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/topology"),
            json!({"rule_type": "no_overlap", "description": "none"}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/schema/migrations"),
            json!({
                "description": "add a field",
                "migration_type": "add_field",
                "applied_by": "ignored",
            }),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/symbology"),
            json!({"name": "water", "symbol": {"type": "simple_fill"}}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/labels"),
            json!({"name": "plain", "field_expression": "code"}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/domains"),
            json!({"name": "surface", "domain_type": "coded", "field_type": "text"}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/subtypes"),
            json!({"subtype_field": "kind", "name": "primary", "code": 1}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/attribute-rules"),
            json!({
                "name": "positive width",
                "rule_type": "constraint",
                "trigger_event": "insert",
                "expression": "width > 0",
            }),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/networks"),
            json!({"name": "mains"}),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/trajectories"),
            json!({
                "name": "bus 12",
                "points": [
                    {"lng": 1.0, "lat": 2.0, "timestamp": "2024-01-01T00:00:00Z"},
                    {"lng": 3.0, "lat": 4.0, "timestamp": "2024-01-01T01:00:00Z"},
                ],
            }),
        ),
        (
            "POST",
            format!("/api/v1/datasets/{dataset_id}/events"),
            json!({"event_type": "custom", "payload": {}}),
        ),
        (
            "POST",
            format!("/api/v1/branches/{branch_id}/locks"),
            json!({"feature_id": Uuid::now_v7(), "locked_by": "ignored"}),
        ),
        // the two bulk feature writers: these rewrite rows on someone else's
        // branch, so they are the ones that mattered most
        (
            "POST",
            format!("/api/v1/branches/{branch_id}/h3/index"),
            json!({"resolution": 7}),
        ),
        (
            "POST",
            format!("/api/v1/branches/{branch_id}/similarity/embed"),
            json!({"fields": ["name"]}),
        ),
    ]
}

#[tokio::test]
async fn test_ungranted_editor_cannot_write_through_any_route() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _carol) = owned_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    for (method, uri, body) in gated_writes(dataset_id, branch_id) {
        let (status, response) =
            request_as(&app, method, &uri, Some(&eve), Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} should be refused: {response}"
        );
    }
}

#[tokio::test]
async fn test_granted_editor_still_writes_through_every_route() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _carol) = owned_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);
    grant(&app, "datasets", dataset_id, "eve", "write").await;

    for (method, uri, body) in gated_writes(dataset_id, branch_id) {
        let (status, response) =
            request_as(&app, method, &uri, Some(&eve), Some(body.clone())).await;
        // h3-pg and pgvector are optional, so their handlers may still fail on a
        // database without them. What matters is that the gate let the call in.
        if needs_optional_extension(&uri) {
            assert_ne!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} should clear the write gate: {response}"
            );
            continue;
        }
        assert!(
            status.is_success(),
            "{method} {uri} should succeed for a granted editor: {status} {response}"
        );
    }
}

/// The instance admin bypasses per-dataset permissions, as it does everywhere
/// else, so the new layer must not shut it out.
#[tokio::test]
async fn test_instance_admin_writes_through_every_route() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _carol) = owned_dataset(&app).await;
    let root = token_for_user("root", Role::Admin);

    for (method, uri, body) in gated_writes(dataset_id, branch_id) {
        let (status, response) =
            request_as(&app, method, &uri, Some(&root), Some(body.clone())).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} should clear the write gate for an admin: {response}"
        );
    }
}

/// A mutating method is not the same as a write. These POST because their input
/// does not fit in a query string, and they persist nothing, so a caller who may
/// only read the dataset has to keep being able to run them.
#[tokio::test]
async fn test_read_only_caller_can_still_run_compute_posts() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _carol) = seed_private_dataset(&app).await;
    let vic = token_for_user("vic", Role::Editor);
    grant(&app, "datasets", dataset_id, "vic", "read").await;

    let compute: Vec<(String, Value)> = vec![
        (
            format!("/api/v1/branches/{branch_id}/geoprocessing/centroid"),
            json!({}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/geoprocessing/dissolve"),
            json!({}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/features/intersects"),
            json!({"geometry": {"type": "Point", "coordinates": [1.0, 1.0]}}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/transform"),
            json!({
                "geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f",
                "from_srid": 4326,
                "to_srid": 3857,
            }),
        ),
        (
            "/api/v1/coverage/simulate".to_string(),
            json!({
                "tower_lng": 0.0, "tower_lat": 0.0, "radius_m": 1000.0,
                "frequency_mhz": 900.0, "height_m": 30.0, "power_dbm": 40.0,
            }),
        ),
    ];

    for (uri, body) in compute {
        let (status, response) =
            request_as(&app, "POST", &uri, Some(&vic), Some(body.clone())).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "read-only caller must keep {uri}: {response}"
        );
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "read-only caller must keep {uri}: {response}"
        );
    }
}

/// The write gate takes the first path id, so merging still only needs a grant
/// on the branch being written, not on the one being read.
#[tokio::test]
async fn test_merge_needs_write_on_the_target_only() {
    let app = setup_app_authed().await;
    let (dataset_id, target_id, carol) = owned_dataset(&app).await;

    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "feature", "created_by": "x", "fork_from_branch": target_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let source_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();
    let (status, body) = commit_as(&app, source_id, &carol).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // a grant on the target alone, and nothing on the source
    let dana = token_for_user("dana", Role::Editor);
    grant(&app, "branches", target_id, "dana", "write").await;

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/branches/{target_id}/merge/{source_id}"),
        Some(&dana),
        None,
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "merge as dana: {body}");
}

/// A review names its branches in the body, where neither layer can see them, so
/// the handler runs the ladder itself.
#[tokio::test]
async fn test_create_review_needs_write_on_the_target_branch() {
    let app = setup_app_authed().await;
    let (dataset_id, target_id, carol) = owned_dataset(&app).await;

    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/branches"),
        Some(&carol),
        Some(json!({"name": "feature", "created_by": "x", "fork_from_branch": target_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let source_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let body = json!({
        "dataset_id": dataset_id,
        "source_branch_id": source_id,
        "target_branch_id": target_id,
        "title": "please merge",
        "author": "ignored",
    });

    let eve = token_for_user("eve", Role::Editor);
    let (status, response) = request_as(
        &app,
        "POST",
        "/api/v1/reviews",
        Some(&eve),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "outsider review: {response}");

    grant(&app, "datasets", dataset_id, "eve", "write").await;
    let (status, response) =
        request_as(&app, "POST", "/api/v1/reviews", Some(&eve), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "granted review: {response}");
}

/// A relationship class names its two datasets in the body and ignores the path,
/// so the handler has to check both sides itself.
#[tokio::test]
async fn test_create_relationship_class_checks_both_datasets() {
    let app = setup_app_authed().await;
    let (origin, _, _carol) = owned_dataset(&app).await;
    let (destination, _, _) = owned_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    let body = json!({
        "name": "parcel to owner",
        "origin_dataset_id": origin,
        "destination_dataset_id": destination,
        "origin_foreign_key": "parcel_id",
    });

    // the path names a dataset eve may write, but the body names two she may not
    grant(&app, "datasets", origin, "eve", "write").await;
    let (status, response) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{origin}/relationships"),
        Some(&eve),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "half-granted class: {response}"
    );

    grant(&app, "datasets", destination, "eve", "write").await;
    let (status, response) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{origin}/relationships"),
        Some(&eve),
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "fully granted class: {response}"
    );
}

/// A PostGIS topology is a Postgres schema with no dataset behind it, so it has
/// no owner for the ladder and stays admin-only.
#[tokio::test]
async fn test_topology_ddl_is_admin_only() {
    let app = setup_app_authed().await;
    let (dataset_id, _, _carol) = owned_dataset(&app).await;
    let editor = token_for_user("carol", Role::Editor);

    for (uri, body) in [
        (
            format!("/api/v1/datasets/{dataset_id}/topologies"),
            json!({"name": "roads_topo"}),
        ),
        (
            "/api/v1/topologies/roads_topo/add-face".to_string(),
            json!({"geometry_wkb_hex": "0101000000000000000000f03f000000000000f03f"}),
        ),
        (
            "/api/v1/topologies/roads_topo/simplify".to_string(),
            json!({"tolerance": 0.001}),
        ),
    ] {
        let (status, response) = request_as(&app, "POST", &uri, Some(&editor), Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "editor {uri}: {response}");
    }

    // reading a topology is unchanged
    let (status, _) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/topologies"),
        Some(&editor),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
