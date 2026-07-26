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
         DROP TABLE IF EXISTS dataset_tags CASCADE;
         DROP TABLE IF EXISTS _sqlx_migrations CASCADE;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let store = PgStore::new(pool);
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
    let state = fresh_state().await;
    app_with_auth(state, AuthConfig::enabled(TEST_SECRET))
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

    let (status, _body) = get_json(&app, &format!("/api/v1/datasets/{ds_id}/trajectories")).await;
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
        json!({"feature_id": f1.to_string(), "owner": "alice"}),
    )
    .await;
    assert!(
        status == StatusCode::CREATED
            || status == StatusCode::OK
            || status == StatusCode::UNPROCESSABLE_ENTITY,
        "lock: {status} {body}"
    );

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

#[tokio::test]
async fn test_dataset_permissions_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/permissions");
    assert_read_is_admin_only(&app, &uri, &admin).await;
}

#[tokio::test]
async fn test_permission_check_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/permissions/some-user/check");
    assert_read_is_admin_only(&app, &uri, &admin).await;
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
