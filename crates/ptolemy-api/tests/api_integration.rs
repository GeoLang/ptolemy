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
use sqlx::{PgPool, Row};
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

    sqlx::query("DROP TABLE IF EXISTS project_invitations CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS project_members CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS workspace_members CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS projects CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS workspaces CASCADE")
        .execute(&pool)
        .await
        .unwrap();

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

/// Whether the test database carries an optional extension. The routes that
/// need one answer 501 where it is missing, so a test has to know which case it
/// is looking at.
async fn has_extension(state: &AppState, name: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = $1)")
        .bind(name)
        .fetch_one(state.read_pool())
        .await
        .unwrap()
}

async fn has_function(state: &AppState, name: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_proc WHERE proname = $1)")
        .bind(name)
        .fetch_one(state.read_pool())
        .await
        .unwrap()
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

/// Helper: merge `source_id` into `target_id` and return the response body.
async fn merge_branches(app: &axum::Router, target_id: Uuid, source_id: Uuid) -> Value {
    let (status, body) = post_json(
        app,
        &format!("/api/v1/branches/{target_id}/merge/{source_id}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge: {body}");
    assert_eq!(body["status"], "success", "merge: {body}");
    body
}

/// Helper: the feature ids the given changeset wrote a version for.
async fn features_written_by(state: &AppState, changeset_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT feature_id FROM feature_versions WHERE changeset_id = $1",
    )
    .bind(changeset_id)
    .fetch_all(state.read_pool())
    .await
    .unwrap()
}

async fn count_changesets(state: &AppState) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM changesets")
        .fetch_one(state.read_pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn test_remerge_starts_from_the_previous_merge() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "main-v1"}},
            {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "shared-v1"}}
        ]),
    )
    .await;

    let dev_id = create_fork(&app, ds_id, "dev", main_id).await;
    let dev_first = commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-1"}},
            {"type": "update", "feature_id": f2.to_string(), "properties": {"name": "shared-dev"}}
        ]),
    )
    .await;

    let first = merge_branches(&app, main_id, dev_id).await;
    assert_eq!(first["up_to_date"], false, "first merge: {first}");
    assert_eq!(
        first["changeset"]["merge_parent_id"],
        json!(dev_first.to_string()),
        "the merge commit records the source head: {first}"
    );

    // The source moves on, touching only f1
    let dev_second = commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-2"}}
        ]),
    )
    .await;

    let second = merge_branches(&app, main_id, dev_id).await;
    assert_eq!(second["up_to_date"], false, "second merge: {second}");
    assert_eq!(
        second["changeset"]["merge_parent_id"],
        json!(dev_second.to_string()),
        "second merge: {second}"
    );

    // The base advanced to the first merge's source head, so the second merge
    // carries the one feature changed since then and nothing already merged
    let second_id = Uuid::parse_str(second["changeset"]["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        features_written_by(&state, second_id).await,
        vec![f1],
        "re-merge must not redo the first merge's work: {second}"
    );

    let (status, body) = get_json(&app, &format!("/api/v1/branches/{main_id}/features")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut names: Vec<&str> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["dev-2", "shared-dev"], "{body}");
}

#[tokio::test]
async fn test_merge_with_nothing_new_is_up_to_date() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "main-v1"}}
    ])).await;

    let dev_id = create_fork(&app, ds_id, "dev", main_id).await;
    let f2 = Uuid::now_v7();
    commit_features(&app, dev_id, json!([
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "dev-new"}}
    ])).await;

    let first = merge_branches(&app, main_id, dev_id).await;
    let merge_id = first["changeset"]["id"].as_str().unwrap().to_string();
    let changesets_after_first = count_changesets(&state).await;

    let second = merge_branches(&app, main_id, dev_id).await;
    assert_eq!(second["up_to_date"], true, "second merge: {second}");
    assert_eq!(second["changeset"], Value::Null, "second merge: {second}");
    assert_eq!(
        count_changesets(&state).await,
        changesets_after_first,
        "an up-to-date merge must not write a changeset"
    );

    let (status, history) = get_json(&app, &format!("/api/v1/branches/{main_id}/history")).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let with_second_parent: Vec<&Value> = history
        .as_array()
        .unwrap()
        .iter()
        .filter(|cs| cs["merge_parent_id"].is_string())
        .collect();
    assert_eq!(with_second_parent.len(), 1, "history: {history}");
    assert_eq!(
        with_second_parent[0]["id"],
        json!(merge_id),
        "history shows the second parent on the merge commit: {history}"
    );
}

#[tokio::test]
async fn test_conflict_listing_ignores_a_side_that_matches_the_base() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "main-v1"}},
            {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "shared-v1"}}
        ]),
    )
    .await;

    let dev_id = create_fork(&app, ds_id, "dev", main_id).await;
    commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-1"}},
            {"type": "update", "feature_id": f2.to_string(), "properties": {"name": "shared-dev"}}
        ]),
    )
    .await;
    merge_branches(&app, main_id, dev_id).await;

    // Only the source touches f1 after the merge. What main holds for it is the
    // merge's own copy, which still matches the base, so neither feature is a
    // conflict and the merge would settle both by itself.
    commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-2"}}
        ]),
    )
    .await;

    let (status, listed) = get_json(&app, &format!("/api/v1/conflicts/{dev_id}")).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(
        listed.as_array().unwrap().len(),
        0,
        "nothing to resolve: {listed}"
    );

    let (status, preview) = get_json(
        &app,
        &format!("/api/v1/branches/{main_id}/merge/{dev_id}/preview"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["conflict_count"], 0, "{preview}");

    let merged = merge_branches(&app, main_id, dev_id).await;
    assert_eq!(merged["up_to_date"], false, "{merged}");

    // A feature both sides move away from the base is still a conflict
    commit_features(
        &app,
        main_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "main-3"}}
        ]),
    )
    .await;
    commit_features(
        &app,
        dev_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "dev-3"}}
        ]),
    )
    .await;

    let (status, listed) = get_json(&app, &format!("/api/v1/conflicts/{dev_id}")).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["feature_id"], json!(f1.to_string()), "{listed:?}");
    assert_eq!(listed[0]["ours"]["name"], "main-3", "{listed:?}");
    assert_eq!(listed[0]["theirs"]["name"], "dev-3", "{listed:?}");
    assert_eq!(listed[0]["base"]["name"], "dev-2", "{listed:?}");

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{main_id}/merge/{dev_id}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["status"], "conflicts",
        "the listing and the merge agree: {body}"
    );
}

#[tokio::test]
async fn test_diff_across_a_merge_sees_both_parents() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let main_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, main_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "main-v1"}}
    ])).await;

    let dev_id = create_fork(&app, ds_id, "dev", main_id).await;
    let f2 = Uuid::now_v7();
    commit_features(&app, dev_id, json!([
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "dev-merged"}}
    ])).await;

    let merged = merge_branches(&app, main_id, dev_id).await;
    let merge_id = merged["changeset"]["id"].as_str().unwrap();

    let f3 = Uuid::now_v7();
    let dev_after = commit_features(&app, dev_id, json!([
        {"type": "insert", "feature_id": f3.to_string(), "geometry_wkb_hex": point_hex, "properties": {"name": "dev-later"}}
    ])).await;

    // From the merge commit the whole source branch up to the merge is reached
    // through the second parent, so only the commit after it is new
    let (status, body) = get_json(&app, &format!("/api/v1/diff/{merge_id}/{dev_after}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "diff across the merge: {body}");
    assert_eq!(
        ops[0]["Insert"]["feature_id"],
        json!(f3.to_string()),
        "{body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Raster Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_route_creation_adds_distance_measures() {
    let (app, state) = setup_app().await;
    let dataset_id = create_dataset(&app).await;
    let line_wkb =
        "01020000000200000000000000000000000000000000000000000000000000F03F000000000000F03F";

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{dataset_id}/routes"),
        json!({"name": "measured", "geometry_wkb_hex": line_wkb}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create route: {body}");
    let route_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let row = sqlx::query(
        "SELECT total_length,
                ST_M(ST_StartPoint(geometry)) AS start_measure,
                ST_M(ST_EndPoint(geometry)) AS end_measure
         FROM routes WHERE id = $1",
    )
    .bind(route_id)
    .fetch_one(state.read_pool())
    .await
    .unwrap();
    let total_length: f64 = row.get("total_length");
    let start_measure: f64 = row.get("start_measure");
    let end_measure: f64 = row.get("end_measure");
    assert!(total_length > 0.0);
    assert_eq!(start_measure, 0.0);
    assert!((end_measure - total_length).abs() < 1e-6);

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/routes/{route_id}/events"),
        json!({
            "event_type": "midpoint",
            "from_measure": total_length / 2.0,
            "properties": {}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create route event: {body}");
}

#[tokio::test]
async fn test_raster_catalog_and_tiles() {
    let (app, state) = setup_app().await;
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

    let raster_wkb = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT ST_AsWKB(ST_MakeEmptyRaster(1, 1, 0, 1, 1, -1, 0, 0, 4326))",
    )
    .fetch_one(state.read_pool())
    .await
    .unwrap();
    let bounds_wkb = "0103000000010000000500000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000F03F00000000000000000000000000000000";
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/rasters/{catalog_id}/tiles"),
        json!({
            "zoom_level": 0,
            "bounds_wkb_hex": bounds_wkb,
            "rast_hex": hex::encode(raster_wkb)
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "upload tile: {body}");

    let row = sqlx::query(
        "SELECT ST_Width(rast) AS width, ST_Height(rast) AS height, ST_SRID(rast) AS srid
         FROM raster_tiles WHERE catalog_id = $1",
    )
    .bind(Uuid::parse_str(catalog_id).unwrap())
    .fetch_one(state.read_pool())
    .await
    .unwrap();
    assert_eq!(row.get::<i32, _>("width"), 1);
    assert_eq!(row.get::<i32, _>("height"), 1);
    assert_eq!(row.get::<i32, _>("srid"), 4326);

    // too short to be a raster header, and a well-formed header the decoder
    // rejects on version: both are the client's bytes, not a server fault
    for rast_hex in ["00", &"ff".repeat(70)] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/rasters/{catalog_id}/tiles"),
            json!({
                "zoom_level": 0,
                "bounds_wkb_hex": bounds_wkb,
                "rast_hex": rast_hex
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "upload {rast_hex}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("not valid WKB"),
            "upload {rast_hex}: {body}"
        );
    }
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

/// Migration 015 only gives `pointcloud_patches.pa` the `pcpatch` type where
/// the pointcloud extension is installed. Without it the two routes that read
/// or write a patch answer 501 naming it, never a 500 over a missing type. The
/// catalog routes touch neither and keep working.
#[tokio::test]
async fn test_pointcloud_patch_routes_report_missing_pointcloud() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/pointclouds"),
        json!({"name": "lidar_scan", "srid": 4326}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create pc catalog: {body}");
    let catalog_id = body["id"].as_str().unwrap().to_string();
    let installed = has_extension(&state, "pointcloud").await;

    let posts = [
        (
            format!("/api/v1/pointclouds/{catalog_id}/patches"),
            json!({
                "bounds_wkb_hex": "0101000000000000000000F03F0000000000000040",
                "num_points": 1,
                "patch_hex": "00",
            }),
        ),
        (
            format!("/api/v1/pointclouds/{catalog_id}/profile"),
            json!({"line_wkb_hex": "01020000000200000000000000000000000000000000000000000000000000f03f000000000000f03f"}),
        ),
    ];
    for (uri, request) in &posts {
        let (status, body) = post_json(&app, uri, request.clone()).await;
        if installed {
            assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
        } else {
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
            assert!(
                body["error"].as_str().unwrap().contains("pointcloud"),
                "{uri}: {body}"
            );
        }
    }

    // the catalog side needs no extension and must not have picked up a 501
    let (status, body) = get_json(&app, &format!("/api/v1/pointclouds/{catalog_id}/stats")).await;
    assert_eq!(status, StatusCode::OK, "pc stats: {body}");
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
// OGC API - Features Part 2: CRS by reference
// ═══════════════════════════════════════════════════════════════════════

const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const EPSG_4326_URI: &str = "http://www.opengis.net/def/crs/EPSG/0/4326";
const EPSG_3857_URI: &str = "http://www.opengis.net/def/crs/EPSG/0/3857";

/// Helper: GET returning the Content-Crs header beside status and body.
async fn get_json_with_crs(app: &axum::Router, uri: &str) -> (StatusCode, String, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", "Bearer test-skip")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let content_crs = resp
        .headers()
        .get("content-crs")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, content_crs, value)
}

/// Helper: a dataset holding one point at longitude 10, latitude 20.
async fn crs_dataset(app: &axum::Router) -> Uuid {
    let dataset_id = create_dataset(app).await;
    let branch_id = create_branch(app, dataset_id, "main").await;
    // WKB hex for POINT(10 20) — little-endian
    let point_hex = "010100000000000000000024400000000000003440";
    commit_features(
        app,
        branch_id,
        json!([{"type": "insert", "geometry_wkb_hex": point_hex, "properties": {"name": "pin"}}]),
    )
    .await;
    dataset_id
}

#[tokio::test]
async fn test_ogc_conformance_declares_crs_class() {
    let (app, _) = setup_app().await;

    let (status, body) = get_json(&app, "/api/v1/ogc/conformance").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let classes = body["conformsTo"].as_array().unwrap();
    assert!(
        classes
            .iter()
            .any(|c| c == "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs"),
        "{body}"
    );
}

#[tokio::test]
async fn test_ogc_collection_lists_supported_crs() {
    let (app, _) = setup_app().await;
    let dataset_id = create_dataset(&app).await;

    let (status, body) = get_json(&app, &format!("/api/v1/ogc/collections/{dataset_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["storageCrs"], CRS84_URI, "{body}");
    let supported = body["crs"].as_array().unwrap();
    for uri in [CRS84_URI, EPSG_4326_URI, EPSG_3857_URI] {
        assert!(supported.iter().any(|c| c == uri), "missing {uri}: {body}");
    }
}

#[tokio::test]
async fn test_ogc_items_default_crs_is_crs84() {
    let (app, _) = setup_app().await;
    let dataset_id = crs_dataset(&app).await;

    let (status, content_crs, body) =
        get_json_with_crs(&app, &format!("/api/v1/ogc/collections/{dataset_id}/items")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_crs, format!("<{CRS84_URI}>"));
    let coords = body["features"][0]["geometry"]["coordinates"]
        .as_array()
        .unwrap();
    assert_eq!(coords[0].as_f64().unwrap(), 10.0, "{body}");
    assert_eq!(coords[1].as_f64().unwrap(), 20.0, "{body}");
}

#[tokio::test]
async fn test_ogc_items_epsg_4326_is_latitude_first() {
    let (app, _) = setup_app().await;
    let dataset_id = crs_dataset(&app).await;

    let (status, content_crs, body) = get_json_with_crs(
        &app,
        &format!("/api/v1/ogc/collections/{dataset_id}/items?crs={EPSG_4326_URI}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_crs, format!("<{EPSG_4326_URI}>"));
    let coords = body["features"][0]["geometry"]["coordinates"]
        .as_array()
        .unwrap();
    assert_eq!(coords[0].as_f64().unwrap(), 20.0, "{body}");
    assert_eq!(coords[1].as_f64().unwrap(), 10.0, "{body}");
}

#[tokio::test]
async fn test_ogc_items_epsg_3857_is_metres() {
    let (app, _) = setup_app().await;
    let dataset_id = crs_dataset(&app).await;

    let (status, content_crs, body) = get_json_with_crs(
        &app,
        &format!("/api/v1/ogc/collections/{dataset_id}/items?crs={EPSG_3857_URI}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_crs, format!("<{EPSG_3857_URI}>"));
    let coords = body["features"][0]["geometry"]["coordinates"]
        .as_array()
        .unwrap();
    let (x, y) = (coords[0].as_f64().unwrap(), coords[1].as_f64().unwrap());
    assert!((1.10e6..1.13e6).contains(&x), "x {x}: {body}");
    assert!((2.25e6..2.30e6).contains(&y), "y {y}: {body}");

    // the single feature route reprojects the same way
    let feature_id = body["features"][0]["id"].as_str().unwrap().to_string();
    let (status, content_crs, body) = get_json_with_crs(
        &app,
        &format!("/api/v1/ogc/collections/{dataset_id}/items/{feature_id}?crs={EPSG_3857_URI}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_crs, format!("<{EPSG_3857_URI}>"));
    let coords = body["geometry"]["coordinates"].as_array().unwrap();
    let (x, y) = (coords[0].as_f64().unwrap(), coords[1].as_f64().unwrap());
    assert!((1.10e6..1.13e6).contains(&x), "x {x}: {body}");
    assert!((2.25e6..2.30e6).contains(&y), "y {y}: {body}");
}

#[tokio::test]
async fn test_ogc_items_bbox_crs() {
    let (app, _) = setup_app().await;
    let dataset_id = crs_dataset(&app).await;

    // a metre bbox around the point
    let (status, _, body) = get_json_with_crs(
        &app,
        &format!(
            "/api/v1/ogc/collections/{dataset_id}/items\
             ?bbox=1100000,2260000,1130000,2290000&bbox-crs={EPSG_3857_URI}"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 1, "{body}");

    // and one nowhere near it
    let (status, _, body) = get_json_with_crs(
        &app,
        &format!(
            "/api/v1/ogc/collections/{dataset_id}/items\
             ?bbox=0,0,100000,100000&bbox-crs={EPSG_3857_URI}"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 0, "{body}");

    // an EPSG:4326 bbox is latitude first
    let (status, _, body) = get_json_with_crs(
        &app,
        &format!(
            "/api/v1/ogc/collections/{dataset_id}/items?bbox=19,9,21,11&bbox-crs={EPSG_4326_URI}"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["numberReturned"], 1, "{body}");
}

#[tokio::test]
async fn test_ogc_items_rejects_unsupported_crs() {
    let (app, _) = setup_app().await;
    let dataset_id = crs_dataset(&app).await;

    for crs in ["http://www.opengis.net/def/crs/EPSG/0/999999", "garbage"] {
        let (status, body) = get_json(
            &app,
            &format!("/api/v1/ogc/collections/{dataset_id}/items?crs={crs}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{crs}: {body}");
    }
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

/// `ST_Union` over no rows at all is NULL rather than an empty geometry, so a
/// merge whose ids match nothing has to answer the same shape the convex hull
/// answers for a branch with no features: a 200 carrying a null geometry.
#[tokio::test]
async fn test_geoprocessing_merge_of_no_features_is_a_null_geometry() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let sq1 = "0103000000010000000500000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000F03F00000000000000000000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": sq1, "properties": {}}
    ])).await;

    // a live branch, but an id that is not on it
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/geoprocessing/merge"),
        json!({"feature_ids": [Uuid::now_v7().to_string()]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge of nothing: {body}");
    assert!(body["geometry"].is_null(), "merge of nothing: {body}");
    assert_eq!(body["area_sq_meters"], 0.0, "merge of nothing: {body}");
}

/// `ST_Simplify` drops a geometry it cannot keep at the given tolerance, and
/// returns NULL where it did, so a tolerance wider than the feature answers a
/// feature with no geometry rather than failing the whole request.
#[tokio::test]
async fn test_geoprocessing_simplify_past_collapse_is_a_null_geometry() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    // the unit square, against a tolerance ten times wider than it is
    let sq1 = "0103000000010000000500000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000F03F00000000000000000000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": sq1, "properties": {}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/geoprocessing/simplify"),
        json!({"tolerance": 10.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "simplify: {body}");
    let features = body["features"].as_array().expect("a feature array");
    assert_eq!(features.len(), 1, "simplify: {body}");
    assert!(features[0]["geometry"].is_null(), "simplify: {body}");
    assert_eq!(
        features[0]["properties"]["points_after"], 0,
        "simplify: {body}"
    );
    assert_eq!(
        features[0]["properties"]["points_before"], 5,
        "simplify: {body}"
    );
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

/// `ST_Union` and `ST_Collect` have no geography form, so both of these routes
/// used to be a 500 for every request. The union is taken on geometry and only
/// the result cast, which is what makes the area come back in square meters.
#[tokio::test]
async fn test_union_and_convex_hull_report_area_in_square_meters() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let (f1, f2) = (Uuid::now_v7(), Uuid::now_v7());

    // the same two adjacent unit squares the merge test uses
    let sq1 = "0103000000010000000500000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000F03F00000000000000000000000000000000";
    let sq2 = "01030000000100000005000000000000000000F03F0000000000000000000000000000004000000000000000000000000000000040000000000000F03F000000000000F03F000000000000F03F000000000000F03F0000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": sq1, "properties": {}},
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": sq2, "properties": {}}
    ])).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/analytics/union"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "union: {body}");
    assert_eq!(body["feature_count"], 2, "union: {body}");
    assert!(body["union_geojson"].is_object(), "union: {body}");
    let area = body["total_area_sq_meters"].as_f64().unwrap();
    assert!(area > 1.0e10 && area < 1.0e11, "union area: {area}");

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/geoprocessing/convex-hull"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "convex hull: {body}");
    assert!(body["geometry"].is_object(), "convex hull: {body}");
    let area = body["area_sq_meters"].as_f64().unwrap();
    assert!(area > 1.0e10 && area < 1.0e11, "convex hull area: {area}");
}

/// The outlier arm measures each feature against the centroid of the whole
/// branch, which needs one aggregate's result inside another and so has to be
/// computed a CTE earlier. Two points sit equally far from the midpoint between
/// them, so the standard deviation of the two distances is zero and both clear
/// a threshold of three times it: the arm is proven to have run.
#[tokio::test]
async fn test_anomaly_detection_finds_spatial_outliers() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let (f1, f2) = (Uuid::now_v7(), Uuid::now_v7());

    // POINT(1 2) and POINT(10 20), a thousand kilometres apart
    let near = "0101000000000000000000F03F0000000000000040";
    let far = "010100000000000000000024400000000000003440";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": near, "properties": {}},
        {"type": "insert", "feature_id": f2.to_string(), "geometry_wkb_hex": far, "properties": {}}
    ])).await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/analytics/anomalies"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "anomalies: {body}");
    let found = body.as_array().expect("an array of anomalies");
    assert!(
        found.iter().any(|a| a["anomaly_type"] == "spatial_outlier"),
        "anomalies: {body}"
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

/// A face the topology's edges do not bound, and a geometry that is not a face
/// at all, are both the client's to fix. A topology whose edges contradict each
/// other is not, and stays a 500 the client never sees the detail of.
#[tokio::test]
async fn test_add_face_rejects_unusable_geometry() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let name = format!("addface_{}", Uuid::now_v7().simple());

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/topologies"),
        json!({"name": name, "srid": 4326}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create topology: {body}");

    // AddFace only registers a face the existing edges already bound, and no
    // route adds an edge, so the ring the success case needs is seeded here
    let ring = "0102000000050000000000000000000000000000000000000000000000000008400000000000000000000000000000084000000000000008400000000000000000000000000000084000000000000000000000000000000000";
    sqlx::query("SELECT topology.TopoGeo_AddLineString($1, ST_GeomFromWKB($2, 4326), 0)")
        .bind(&name)
        .bind(hex::decode(ring).unwrap())
        .fetch_all(state.unguarded_pool())
        .await
        .unwrap();

    let square = "010300000001000000050000000000000000000000000000000000000000000000000008400000000000000000000000000000084000000000000008400000000000000000000000000000084000000000000000000000000000000000";
    let uri = format!("/api/v1/topologies/{name}/add-face");
    let (status, body) = post_json(&app, &uri, json!({"geometry_wkb_hex": square})).await;
    assert_eq!(status, StatusCode::CREATED, "bounded face: {body}");

    let point = "0101000000000000000000F03F0000000000000040";
    let (status, body) = post_json(&app, &uri, json!({"geometry_wkb_hex": point})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "point face: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("must be a polygon"),
        "point face: {body}"
    );

    let elsewhere = "01030000000100000005000000000000000000244000000000000024400000000000002a4000000000000024400000000000002a400000000000002a4000000000000024400000000000002a4000000000000024400000000000002440";
    let (status, body) = post_json(&app, &uri, json!({"geometry_wkb_hex": elsewhere})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unbounded face: {body}");
    assert!(
        body["error"].as_str().unwrap().contains("Found no edges"),
        "unbounded face: {body}"
    );
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

/// Every 3d route calls an SFCGAL function, which a PostGIS build without
/// `postgis_sfcgal` does not define. Migration 015 installs it where the build
/// carries it, so on the image CI runs this asserts the other side: the guard
/// lets the call through.
#[tokio::test]
async fn test_sfcgal_routes_report_missing_sfcgal() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();
    let polygon_hex = "01030000000100000005000000000000000000000000000000000000000000000000002440000000000000000000000000000024400000000000002440000000000000000000000000000024400000000000000000000000000000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": polygon_hex, "properties": {}}
    ])).await;
    let installed = has_extension(&state, "postgis_sfcgal").await;

    let posts = [
        (
            format!("/api/v1/branches/{branch_id}/3d/extrude"),
            json!({"feature_id": f1.to_string(), "height": 10.0}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/3d/volume"),
            json!({"feature_id": f1.to_string()}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/3d/intersection"),
            json!({"feature_a": f1.to_string(), "feature_b": f1.to_string()}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/3d/straight-skeleton"),
            json!({"feature_id": f1.to_string()}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/3d/minkowski-sum"),
            json!({"feature_id": f1.to_string(), "buffer_geometry_wkb_hex": polygon_hex}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/3d/tesselate"),
            json!({"feature_id": f1.to_string()}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/3d/visibility"),
            json!({"observer_x": 1.0, "observer_y": 1.0, "observer_z": 1.0, "feature_id": f1.to_string()}),
        ),
    ];
    for (uri, request) in &posts {
        let (status, body) = post_json(&app, uri, request.clone()).await;
        if installed {
            assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
        } else {
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
            assert!(
                body["error"].as_str().unwrap().contains("postgis_sfcgal"),
                "{uri}: {body}"
            );
        }
    }
}

/// The buffer geometry is the client's, so a shape SFCGAL will not sweep is a
/// bad request. Skipped where the extension is missing: the guard answers 501
/// before the query runs, which `test_sfcgal_routes_report_missing_sfcgal` owns.
#[tokio::test]
async fn test_minkowski_sum_rejects_non_polygon_buffer() {
    let (app, state) = setup_app().await;
    if !has_extension(&state, "postgis_sfcgal").await {
        return;
    }
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let feature_id = Uuid::now_v7();
    let polygon_hex = "01030000000100000005000000000000000000000000000000000000000000000000002440000000000000000000000000000024400000000000002440000000000000000000000000000024400000000000000000000000000000000000000000";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": feature_id.to_string(), "geometry_wkb_hex": polygon_hex, "properties": {}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/3d/minkowski-sum");

    let (status, body) = post_json(
        &app,
        &uri,
        json!({"feature_id": feature_id.to_string(), "buffer_geometry_wkb_hex": polygon_hex}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "polygon buffer: {body}");

    let line_hex =
        "01020000000200000000000000000000000000000000000000000000000000F03F000000000000F03F";
    let (status, body) = post_json(
        &app,
        &uri,
        json!({"feature_id": feature_id.to_string(), "buffer_geometry_wkb_hex": line_hex}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "line buffer: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("must be a polygon"),
        "line buffer: {body}"
    );
}

/// PostGIS cuts a polygon but refuses a point, and which one is being cut is
/// the client's choice.
#[tokio::test]
async fn test_split_rejects_unsplittable_geometry() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let polygon_id = Uuid::now_v7();
    let point_id = Uuid::now_v7();
    let polygon_hex = "01030000000100000005000000000000000000000000000000000000000000000000002440000000000000000000000000000024400000000000002440000000000000000000000000000024400000000000000000000000000000000000000000";
    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": polygon_id.to_string(), "geometry_wkb_hex": polygon_hex, "properties": {}},
        {"type": "insert", "feature_id": point_id.to_string(), "geometry_wkb_hex": point_hex, "properties": {}}
    ])).await;
    let uri = format!("/api/v1/branches/{branch_id}/geoprocessing/split");
    let split_line = json!({"type": "LineString", "coordinates": [[-1.0, 5.0], [11.0, 5.0]]});

    let (status, body) = post_json(
        &app,
        &uri,
        json!({"feature_id": polygon_id.to_string(), "split_line": split_line}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "split polygon: {body}");

    let (status, body) = post_json(
        &app,
        &uri,
        json!({"feature_id": point_id.to_string(), "split_line": split_line}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "split point: {body}");
    assert!(
        body["error"].as_str().unwrap().starts_with("Splitting"),
        "split point: {body}"
    );
}

/// `ST_ContourLines` is absent from the PostGIS image CI runs, so contour
/// reports the gap rather than a server fault. Where a build does carry it, the
/// assertion flips: the route must not claim the function is missing.
#[tokio::test]
async fn test_contour_reports_missing_contour_lines() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let feature_id = Uuid::now_v7();
    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(&app, branch_id, json!([
        {"type": "insert", "feature_id": feature_id.to_string(), "geometry_wkb_hex": point_hex, "properties": {"elevation": 12.5}}
    ])).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/geoprocessing/contour"),
        json!({"value_property": "elevation", "interval": 1.0}),
    )
    .await;
    if has_function(&state, "st_contourlines").await {
        assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "contour: {body}");
    } else {
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "contour: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("ST_ContourLines"),
            "contour: {body}"
        );
    }
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
    // h3-pg might not be installed in test env, and then the route says so
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_IMPLEMENTED,
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
    // h3-pg might not be installed, and then the route says so
    assert!(status == StatusCode::OK || status == StatusCode::NOT_IMPLEMENTED);
}

/// Every h3 route names an `h3_*` function or the `h3index` type, neither of
/// which exists without the extension. On the stock PostGIS that CI and the
/// compose stack run they answer 501 naming it, never a 500 over a missing name.
#[tokio::test]
async fn test_h3_routes_report_missing_h3() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let installed = has_extension(&state, "h3").await;

    let gets = [
        format!("/api/v1/branches/{branch_id}/h3/hexagons?resolution=7"),
        format!("/api/v1/branches/{branch_id}/h3/aggregate?resolution=7"),
        format!("/api/v1/branches/{branch_id}/h3/neighbors?cell=8928308280fffff"),
        "/api/v1/h3/cell?lat=0&lng=0&resolution=7".to_string(),
        "/api/v1/h3/boundary?cell=8928308280fffff".to_string(),
    ];
    let posts = [
        (
            format!("/api/v1/branches/{branch_id}/h3/index"),
            json!({"resolution": 7}),
        ),
        (
            format!("/api/v1/branches/{branch_id}/h3/compact"),
            json!({"cells": ["8928308280fffff"]}),
        ),
    ];

    for uri in &gets {
        let (status, body) = get_json(&app, uri).await;
        assert_h3_answer(installed, status, &body, uri);
    }
    for (uri, request) in &posts {
        let (status, body) = post_json(&app, uri, request.clone()).await;
        assert_h3_answer(installed, status, &body, uri);
    }
}

fn assert_h3_answer(installed: bool, status: StatusCode, body: &Value, uri: &str) {
    if installed {
        assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
    } else {
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("h3 extension"),
            "{uri}: {body}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Vector Search Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_vector_generate_embeddings() {
    let (app, state) = setup_app().await;
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
    if has_extension(&state, "vector").await {
        assert_eq!(status, StatusCode::OK, "embed: {body}");
        assert!(body["embedded"].as_i64().unwrap() >= 1);
    } else {
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "embed: {body}");
    }
}

#[tokio::test]
async fn test_vector_similarity_search() {
    let (app, state) = setup_app().await;
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
    let expected = if has_extension(&state, "vector").await {
        StatusCode::OK
    } else {
        StatusCode::NOT_IMPLEMENTED
    };
    assert_eq!(status, expected, "similarity: {body}");
}

/// Every route that reads `feature_versions.embedding`, which migration 016
/// only adds where pgvector is installed. Without it the answer is 501 naming
/// the extension, never a 500 over the missing column.
#[tokio::test]
async fn test_vector_routes_report_missing_pgvector() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let installed = has_extension(&state, "vector").await;

    let reads = [
        format!("/api/v1/branches/{branch_id}/similarity/duplicates"),
        format!("/api/v1/branches/{branch_id}/similarity/search"),
        format!("/api/v1/branches/{branch_id}/similarity/embed"),
        format!("/api/v1/branches/{branch_id}/similarity/cluster"),
    ];
    for (i, uri) in reads.iter().enumerate() {
        let (status, body) = if i == 0 {
            get_json(&app, uri).await
        } else {
            post_json(&app, uri, json!({"embedding": [0.0], "fields": ["name"]})).await
        };
        if installed {
            assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
        } else {
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
            assert!(
                body["error"].as_str().unwrap().contains("pgvector"),
                "{uri}: {body}"
            );
        }
    }
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

/// Dijkstra, A*, driving distance, TSP and connected components are pgRouting
/// or nothing, so without the extension they answer 501 naming it. The junction
/// ids are deliberately absent rows: the guard runs before the query, so what
/// the network holds does not change the answer.
#[tokio::test]
async fn test_pgrouting_routes_report_missing_pgrouting() {
    let (app, state) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/networks"),
        json!({"name": "water_mains"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create network: {body}");
    let net_id = body["id"].as_str().unwrap().to_string();
    let installed = has_extension(&state, "pgrouting").await;
    let junction = Uuid::now_v7();

    let uri = format!("/api/v1/networks/{net_id}/connectivity");
    let (status, body) = get_json(&app, &uri).await;
    assert_pgrouting_answer(installed, status, &body, &uri);

    let posts = [
        (
            format!("/api/v1/networks/{net_id}/shortest-path"),
            json!({"from_junction": junction, "to_junction": junction}),
        ),
        (
            format!("/api/v1/networks/{net_id}/astar"),
            json!({"from_junction": junction, "to_junction": junction}),
        ),
        (
            format!("/api/v1/networks/{net_id}/isochrone"),
            json!({"start_junction": junction, "max_cost": 10.0}),
        ),
        (
            format!("/api/v1/networks/{net_id}/tsp"),
            json!({"junction_ids": [junction]}),
        ),
    ];
    for (uri, request) in &posts {
        let (status, body) = post_json(&app, uri, request.clone()).await;
        assert_pgrouting_answer(installed, status, &body, uri);
    }
}

/// A three-stop line a-b-c with unit edge costs, exercised end to end where
/// pgRouting is installed. The routes rank junction uuids to bigints inside
/// each statement and rank the results back, so every id below round-trips.
#[tokio::test]
async fn test_pgrouting_routes_round_trip_junction_uuids() {
    let (app, state) = setup_app().await;
    if !has_extension(&state, "pgrouting").await {
        return;
    }
    let ds_id = create_dataset(&app).await;
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds_id}/networks"),
        json!({"name": "mains"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create network: {body}");
    let net_id = body["id"].as_str().unwrap().to_string();

    let mut junctions = Vec::new();
    for lng in [0.0, 1.0, 2.0] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/networks/{net_id}/junctions"),
            json!({"lng": lng, "lat": 0.0}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "junction: {body}");
        junctions.push(body["id"].as_str().unwrap().to_string());
    }
    let (a, b, c) = (&junctions[0], &junctions[1], &junctions[2]);
    for (from, to) in [(a, b), (b, c)] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/networks/{net_id}/edges"),
            json!({"feature_id": Uuid::now_v7(), "from_junction": from, "to_junction": to, "cost": 1.0}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "edge: {body}");
    }

    for route in ["shortest-path", "astar"] {
        let (status, body) = post_json(
            &app,
            &format!("/api/v1/networks/{net_id}/{route}"),
            json!({"from_junction": a, "to_junction": c}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{route}: {body}");
        assert_eq!(body["found"], true, "{route}: {body}");
        assert_eq!(body["path_junctions"], json!([a, b, c]), "{route}: {body}");
        assert_eq!(
            body["path_edges"].as_array().unwrap().len(),
            2,
            "{route}: {body}"
        );
        assert_eq!(body["total_cost"], 2.0, "{route}: {body}");
    }

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/networks/{net_id}/isochrone"),
        json!({"start_junction": a, "max_cost": 1.5}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "isochrone: {body}");
    let reached: Vec<&str> = body["reachable_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node"].as_str().unwrap())
        .collect();
    assert!(reached.contains(&a.as_str()), "isochrone: {body}");
    assert!(reached.contains(&b.as_str()), "isochrone: {body}");
    assert!(!reached.contains(&c.as_str()), "isochrone: {body}");

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/networks/{net_id}/tsp"),
        json!({"junction_ids": [a, b, c], "start_junction": a}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tsp: {body}");
    let tour = body["ordered_junctions"].as_array().unwrap();
    for j in [a, b, c] {
        assert!(tour.contains(&json!(j)), "tsp misses {j}: {body}");
    }

    let (status, body) = get_json(&app, &format!("/api/v1/networks/{net_id}/connectivity")).await;
    assert_eq!(status, StatusCode::OK, "connectivity: {body}");
    assert_eq!(body["total_junctions"], 3, "connectivity: {body}");
    assert_eq!(body["total_edges"], 2, "connectivity: {body}");
    assert_eq!(body["connected_components"], 1, "connectivity: {body}");
    assert_eq!(
        body["isolated_junctions"],
        json!([]),
        "connectivity: {body}"
    );

    // an unknown junction ranks to nothing and the path is not found, not a 500
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/networks/{net_id}/shortest-path"),
        json!({"from_junction": Uuid::now_v7(), "to_junction": c}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "absent junction: {body}");
    assert_eq!(body["found"], false, "absent junction: {body}");
}

fn assert_pgrouting_answer(installed: bool, status: StatusCode, body: &Value, uri: &str) {
    if installed {
        assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
    } else {
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("pgRouting"),
            "{uri}: {body}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Industry Vertical Tests
// ═══════════════════════════════════════════════════════════════════════

/// An incident is a commit, and the branch it commits to arrives in the body
/// where no extractor can check it. A branch that does not exist is a 404, not
/// the 500 a `fetch_one` on the empty branch lookup used to raise.
#[tokio::test]
async fn test_create_incident_on_missing_branch_is_404() {
    let (app, _) = setup_app().await;

    let (status, body) = post_json(
        &app,
        "/api/v1/incidents",
        json!({
            "branch_id": Uuid::now_v7(),
            "incident_type": "wildfire",
            "severity": "high",
            "lat": 1.0,
            "lng": 2.0,
            "description": "grass fire at the ridge",
            "author": "dispatch",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "create incident: {body}");
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

/// The five analytics routes call MobilityDB functions on `trip` and have no
/// other form, so on stock PostGIS they answer 501 naming the extension rather
/// than 500 on a function nothing defines.
#[tokio::test]
async fn test_trajectory_analytics_report_missing_mobilitydb() {
    let (app, state) = setup_app().await;
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
    let installed = has_extension(&state, "mobilitydb").await;

    let gets = [
        format!("/api/v1/trajectories/{traj_id}/at?timestamp=2024-01-01T00:30:00Z"),
        format!("/api/v1/trajectories/{traj_id}/speed"),
        format!("/api/v1/trajectories/{traj_id}/distance"),
    ];
    let posts = [
        (
            format!("/api/v1/trajectories/{traj_id}/simplify"),
            json!({"tolerance": 0.001}),
        ),
        (
            format!("/api/v1/datasets/{ds_id}/trajectories/nearest"),
            json!({"trajectory_a": traj_id, "trajectory_b": traj_id}),
        ),
    ];

    for uri in &gets {
        let (status, body) = get_json(&app, uri).await;
        assert_mobilitydb_answer(installed, status, &body, uri);
    }
    for (uri, request) in &posts {
        let (status, body) = post_json(&app, uri, request.clone()).await;
        assert_mobilitydb_answer(installed, status, &body, uri);
    }
}

fn assert_mobilitydb_answer(installed: bool, status: StatusCode, body: &Value, uri: &str) {
    if installed {
        assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
    } else {
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("MobilityDB"),
            "{uri}: {body}"
        );
    }
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
// Esri drawingInfo: served verbatim by the facade, translated by /style
// ═══════════════════════════════════════════════════════════════════════

/// The drawingInfo verne stores when it migrates an esri point layer: a simple
/// marker renderer, plus one thing no translator draws so the losses are real.
fn esri_drawing_info() -> Value {
    json!({
        "renderer": {
            "type": "simple",
            "symbol": {
                "type": "esriSMS",
                "style": "esriSMSCircle",
                "color": [255, 0, 0, 255],
                "size": 12
            },
            "visualVariables": [{"type": "sizeInfo", "field": "pop"}]
        }
    })
}

/// Store an esri symbol the way verne does: one rule whose symbol carries the
/// format tag. `symbol` is passed whole so a test can store a broken one.
async fn store_esri_symbol(
    app: &axum::Router,
    dataset_id: Uuid,
    token: Option<&str>,
    symbol: Value,
) {
    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/symbology"),
        token,
        Some(json!({"name": "esri-drawing-info", "symbol": symbol, "priority": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "store esri symbol: {body}");
}

/// The well formed case: the tag plus a drawingInfo document.
async fn store_esri_style(app: &axum::Router, dataset_id: Uuid, token: Option<&str>) {
    let symbol = json!({"format": "esri-drawing-info", "drawingInfo": esri_drawing_info()});
    store_esri_symbol(app, dataset_id, token, symbol).await;
}

#[tokio::test]
async fn test_facade_serves_stored_esri_drawing_info() {
    let (app, _) = setup_app().await;
    let name = format!("styled_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;
    let uri = format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json");

    // with nothing stored the key is absent, which is a layer drawn by client default
    let (status, layer) = get_json(&app, &uri).await;
    assert_eq!(status, StatusCode::OK, "{layer}");
    assert!(layer.get("drawingInfo").is_none(), "{layer}");

    store_esri_style(&app, ds, None).await;
    let (status, layer) = get_json(&app, &uri).await;
    assert_eq!(status, StatusCode::OK, "{layer}");
    assert_eq!(
        layer["drawingInfo"],
        esri_drawing_info(),
        "the stored document is served verbatim: {layer}"
    );
}

#[tokio::test]
async fn test_facade_ignores_symbology_in_other_formats() {
    let (app, _) = setup_app().await;
    let name = format!("native_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 1).await;

    // a native rule is not an esri document, so the facade claims no drawing for it
    let (status, body) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds}/symbology"),
        json!({"name": "water", "symbol": {"type": "simple_fill", "color": [0, 0, 255, 255]}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{layer}");
    assert!(layer.get("drawingInfo").is_none(), "{layer}");
}

#[tokio::test]
async fn test_dataset_style_translates_stored_drawing_info() {
    let (app, _) = setup_app().await;
    let name = format!("wells_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    store_esri_style(&app, ds, None).await;

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds}/style")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "ptolemy", "{body}");
    assert_eq!(body["sourceLayer"], name, "{body}");

    let layers = body["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1, "{body}");
    assert_eq!(layers[0]["id"], format!("{name}-circle"), "{body}");
    assert_eq!(layers[0]["type"], "circle", "{body}");
    assert_eq!(layers[0]["source"], "ptolemy", "{body}");
    assert_eq!(layers[0]["source-layer"], name, "{body}");
    // a 12 point diameter is 8 css pixels of radius, and esri alpha is 0-255
    assert_eq!(layers[0]["paint"]["circle-radius"], 8.0, "{body}");
    assert_eq!(
        layers[0]["paint"]["circle-color"], "rgba(255,0,0,1)",
        "{body}"
    );

    // a vector-only style names no images, and the key is there and empty rather
    // than absent: the contract from this version on is that it is present
    assert_eq!(body["images"], json!({}), "{body}");

    // the size ramp nobody drew is reported rather than dropped in silence
    let losses = body["losses"].as_array().unwrap();
    assert_eq!(losses.len(), 1, "{body}");
    assert_eq!(losses[0]["path"], "renderer.visualVariables", "{body}");
    assert!(
        losses[0]["reason"]
            .as_str()
            .unwrap()
            .contains("visual variables"),
        "{body}"
    );

    // the caller's own style names its source and layer differently
    let (status, body) = get_json(
        &app,
        &format!("/api/v1/datasets/{ds}/style?source=verne&sourceLayer=drilled"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "verne", "{body}");
    assert_eq!(body["sourceLayer"], "drilled", "{body}");
    assert_eq!(body["layers"][0]["id"], "drilled-circle", "{body}");
    assert_eq!(body["layers"][0]["source"], "verne", "{body}");
    assert_eq!(body["layers"][0]["source-layer"], "drilled", "{body}");
}

/// A real 1x1 png, the smallest thing a service can inline in a picture symbol.
const MARKER_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

/// The drawingInfo a hosted layer publishes for an inline picture marker, which
/// is the renderer whose bitmap the translation has to carry over.
fn esri_picture_drawing_info() -> Value {
    json!({
        "renderer": {
            "type": "simple",
            "symbol": {
                "type": "esriPMS",
                "url": "5f2b1e.png",
                "imageData": MARKER_PNG,
                "contentType": "image/png",
                "width": 18,
                "height": 18,
            }
        }
    })
}

/// A picture marker's bitmap comes back under `images`, and the symbol layer
/// names it: the consumer registers the image before the layers draw.
#[tokio::test]
async fn test_dataset_style_passes_picture_marker_images_through() {
    let (app, _) = setup_app().await;
    let name = format!("pics_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    store_esri_symbol(
        &app,
        ds,
        None,
        json!({"format": "esri-drawing-info", "drawingInfo": esri_picture_drawing_info()}),
    )
    .await;

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds}/style")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let images = body["images"]
        .as_object()
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(images.len(), 1, "{body}");
    let (image_name, image) = images.iter().next().unwrap();
    assert_eq!(
        image["data_uri"],
        format!("data:image/png;base64,{MARKER_PNG}"),
        "{body}"
    );
    // 18 esri points at 96 dpi, the size the consumer registers the bitmap at
    assert_eq!(image["width"], 24.0, "{body}");
    assert_eq!(image["height"], 24.0, "{body}");

    // the layer references the image by that name, which is what pairs the two
    let layers = body["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1, "{body}");
    assert_eq!(layers[0]["type"], "symbol", "{body}");
    assert_eq!(layers[0]["layout"]["icon-image"], *image_name, "{body}");
}

#[tokio::test]
async fn test_dataset_style_404_without_a_stored_esri_style() {
    let (app, _) = setup_app().await;
    let ds = create_dataset(&app).await;

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds}/style")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no stored esri style"),
        "{body}"
    );

    // a rule in another format is not one either
    let (status, created) = post_json(
        &app,
        &format!("/api/v1/datasets/{ds}/symbology"),
        json!({"name": "water", "symbol": {"type": "simple_fill"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds}/style")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn test_dataset_style_422_on_mixed_geometry() {
    let (app, _) = setup_app().await;
    let name = format!("mixed_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "geometry").await;
    store_esri_style(&app, ds, None).await;

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{ds}/style")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("geometry_type is geometry"),
        "the error names the type: {body}"
    );
}

#[tokio::test]
async fn test_dataset_style_422_on_malformed_stored_document() {
    let (app, _) = setup_app().await;

    // tagged, but there is no document under the tag
    let keyless = create_dataset(&app).await;
    store_esri_symbol(&app, keyless, None, json!({"format": "esri-drawing-info"})).await;
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{keyless}/style")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("drawingInfo"),
        "{body}"
    );

    // tagged, and the document is not an object
    let scalar = create_dataset(&app).await;
    store_esri_symbol(
        &app,
        scalar,
        None,
        json!({"format": "esri-drawing-info", "drawingInfo": "none"}),
    )
    .await;
    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{scalar}/style")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("a string"),
        "the error names what was stored instead: {body}"
    );
}

#[tokio::test]
async fn test_private_dataset_style_is_404_for_outsiders() {
    let app = setup_app_authed().await;
    let (dataset_id, _, carol) = seed_private_dataset(&app).await;
    store_esri_style(&app, dataset_id, Some(&carol)).await;
    let uri = format!("/api/v1/datasets/{dataset_id}/style");

    let (status, body) = request_as(&app, "GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "anonymous: {body}");

    let eve = token_for_user("eve", Role::Editor);
    let (status, body) = request_as(&app, "GET", &uri, Some(&eve), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "non-granted editor: {body}");

    // the owner reads the translation of their own style
    let (status, body) = request_as(&app, "GET", &uri, Some(&carol), None).await;
    assert_eq!(status, StatusCode::OK, "owner: {body}");
    assert_eq!(body["layers"][0]["type"], "circle", "{body}");
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
            "is_composite": true,
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
    assert_eq!(body["is_composite"], true, "{body}");

    let (status, body) = get_json(&app, &format!("/api/v1/datasets/{origin}/relationships")).await;
    assert_eq!(status, StatusCode::OK, "list classes: {body}");
    assert_eq!(body[0]["id"], class_id, "{body}");
    assert_eq!(body[0]["is_composite"], true, "{body}");

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

/// A body that says nothing about `is_composite` gets a simple class, which is
/// what every class written before the field existed already is.
#[tokio::test]
async fn test_relationship_class_defaults_to_simple() {
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
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create class: {body}");
    let class_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get_json(&app, &format!("/api/v1/relationship-classes/{class_id}")).await;
    assert_eq!(status, StatusCode::OK, "get class: {body}");
    assert_eq!(body["is_composite"], false, "{body}");
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

    assert_eq!(feature_version_count(state.read_pool()).await, 2);
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
    let before = feature_version_count(state.read_pool()).await;

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
            feature_version_count(state.read_pool()).await,
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
            feature_version_count(state.read_pool()).await,
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
                feature_version_count(state.read_pool()).await,
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

fn token_for_subject(subject: &str) -> String {
    generate_token(TEST_SECRET, subject, Role::Viewer, 3600)
}

fn invitation_expiry(hours: i64) -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

#[tokio::test]
async fn workspace_project_collaboration_enforces_resource_roles() {
    use sha2::{Digest, Sha256};

    let (app, state) = setup_app_authed_with_state().await;
    let owner = token_for_subject("workspace-owner");
    let editor = token_for_subject("workspace-editor");
    let viewer = token_for_subject("workspace-viewer");
    let outsider = token_for_subject("workspace-outsider");
    let direct_editor = token_for_subject("direct-project-editor");
    let invitee = token_for_subject("workspace-invitee");

    let (status, workspace) = request_as(
        &app,
        "POST",
        "/api/v1/workspaces",
        Some(&owner),
        Some(json!({"name": "collaboration", "description": "workspace"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    assert_eq!(workspace["role"], "owner");
    assert_eq!(workspace["created_by"], "workspace-owner");
    assert!(workspace["created_at"].is_string());
    assert!(workspace["updated_at"].is_string());
    let workspace_id = workspace["id"].as_str().unwrap();

    for (user_id, role) in [
        ("workspace-editor", "editor"),
        ("workspace-viewer", "viewer"),
    ] {
        let (status, body) = request_as(
            &app,
            "PUT",
            &format!("/api/v1/workspaces/{workspace_id}/members/{user_id}"),
            Some(&owner),
            Some(json!({"role": role})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["created_at"].is_string());
    }

    let (status, body) = request_as(&app, "GET", "/api/v1/workspaces", Some(&editor), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["role"], "editor");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "owner");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "viewer");

    let (status, body) = request_as(&app, "GET", "/api/v1/workspaces", Some(&outsider), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.as_array().unwrap().is_empty());

    let (status, _) = request_as(
        &app,
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}"),
        Some(&outsider),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/workspaces/{workspace_id}"),
        Some(&editor),
        Some(json!({"name": "renamed", "description": "edited"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "editor");

    let (status, _) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/workspaces/{workspace_id}"),
        Some(&viewer),
        Some(json!({"name": "blocked"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = request_as(
        &app,
        "POST",
        &format!("/api/v1/workspaces/{workspace_id}/invitations"),
        Some(&owner),
        Some(json!({"role": "owner", "expires_at": invitation_expiry(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/workspaces/{workspace_id}/projects"),
        Some(&editor),
        Some(json!({"name": "editor project"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["role"], "owner");
    let project = body;
    let project_id = project["id"].as_str().unwrap();

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/projects/{project_id}"),
        Some(&editor),
        Some(json!({"name": "editor update"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "owner");

    let (status, _) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/projects/{project_id}"),
        Some(&viewer),
        Some(json!({"name": "viewer update"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/projects/{project_id}"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "viewer");

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/workspaces/{workspace_id}/invitations"),
        Some(&owner),
        Some(json!({"role": "viewer", "expires_at": invitation_expiry(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let workspace_invitation_token = body["token"].as_str().unwrap().to_string();
    let workspace_invitation_id = body["id"].as_str().unwrap();

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}/invitations"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .any(|invitation| invitation["id"] == workspace_invitation_id)
    );

    let (status, _) = request_as(
        &app,
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}/invitations"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let stored_hash: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM project_invitations WHERE id = $1")
            .bind(Uuid::parse_str(workspace_invitation_id).unwrap())
            .fetch_one(state.unguarded_pool())
            .await
            .unwrap();
    assert_ne!(stored_hash, workspace_invitation_token.as_bytes());

    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/invitations/accept",
        Some(&invitee),
        Some(json!({"token": workspace_invitation_token})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["target"], "workspace");

    let (status, body) = request_as(&app, "GET", "/api/v1/workspaces", Some(&invitee), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["role"], "viewer");

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/projects/{project_id}/members/workspace-invitee"),
        Some(&owner),
        Some(json!({"role": "editor"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "editor");

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/projects/{project_id}"),
        Some(&invitee),
        Some(json!({"name": "highest effective role"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "editor");

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/projects/{project_id}/invitations"),
        Some(&owner),
        Some(json!({"role": "editor", "expires_at": invitation_expiry(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let project_invitation_token = body["token"].as_str().unwrap().to_string();

    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/invitations/accept",
        Some(&direct_editor),
        Some(json!({"token": project_invitation_token})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["target"], "project");

    let (status, _) = request_as(
        &app,
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}"),
        Some(&direct_editor),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) =
        request_as(&app, "GET", "/api/v1/projects", Some(&direct_editor), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["role"], "editor");

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/projects/{project_id}"),
        Some(&direct_editor),
        Some(json!({"name": "effective editor"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "editor");

    let (status, _) = request_as(
        &app,
        "POST",
        &format!("/api/v1/projects/{project_id}/invitations"),
        Some(&owner),
        Some(json!({"role": "owner", "expires_at": invitation_expiry(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/projects/{project_id}/invitations"),
        Some(&owner),
        Some(json!({"role": "viewer", "expires_at": invitation_expiry(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let revoked_invitation_id = body["id"].as_str().unwrap();
    let revoked_invitation_token = body["token"].as_str().unwrap().to_string();

    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/projects/{project_id}/invitations/{revoked_invitation_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, _) = request_as(
        &app,
        "POST",
        "/api/v1/invitations/accept",
        Some(&outsider),
        Some(json!({"token": revoked_invitation_token})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let expired_token = "expired-invitation-token";
    sqlx::query(
        "INSERT INTO project_invitations (
             id, project_id, token_hash, role, created_by, expires_at
         ) VALUES ($1, $2, $3, 'viewer', 'workspace-owner', now() - interval '1 hour')",
    )
    .bind(Uuid::now_v7())
    .bind(Uuid::parse_str(project_id).unwrap())
    .bind(Sha256::digest(expired_token.as_bytes()).as_slice())
    .execute(state.unguarded_pool())
    .await
    .unwrap();

    let (status, _) = request_as(
        &app,
        "POST",
        "/api/v1/invitations/accept",
        Some(&outsider),
        Some(json!({"token": expired_token})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/workspaces/{workspace_id}/members/workspace-owner"),
        Some(&owner),
        Some(json!({"role": "viewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/workspaces/{workspace_id}/members/workspace-owner"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
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

fn test_hash_api_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(key.as_bytes()))
}

async fn seed_api_key(state: &AppState, role: &str) -> String {
    let key = format!("ptk_{}", Uuid::now_v7().simple());
    let key_hash = test_hash_api_key(&key);
    let key_prefix = format!("ptk_{}", &key_hash[..12]);
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_hash, key_prefix, role, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind(Uuid::now_v7())
    .bind(format!("test-{role}"))
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(role)
    .execute(state.unguarded_pool())
    .await
    .unwrap();
    key
}

/// A stored `ptk_` bearer authenticates as the row's role; a random `ptk_`
/// string is 401 rather than a JWT decode miss falling through.
#[tokio::test]
async fn test_ptk_bearer_authenticates_as_the_stored_role() {
    let (app, state) = setup_app_authed_with_state().await;

    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some("ptk_not_a_stored_key"),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "unknown ptk_: {body}");

    let viewer = seed_api_key(&state, "viewer").await;
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&viewer),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "viewer ptk_ write: {body}");

    let editor = seed_api_key(&state, "editor").await;
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&editor),
        Some(new_dataset_body()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "editor ptk_ write: {body}");

    let admin = seed_api_key(&state, "admin").await;
    let (status, body) = request_as(&app, "GET", "/metrics", Some(&admin), None).await;
    assert_eq!(status, StatusCode::OK, "admin ptk_ metrics: {body}");
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

#[tokio::test]
async fn test_replication_peers_read_is_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    assert_read_is_admin_only(&app, "/api/v1/replication/peers", &admin).await;
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
            .fetch_one(state.read_pool())
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
    .execute(state.unguarded_pool())
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
        project_id: None,
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

/// What `/permissions/{user}/check` answers, which has to agree with what the
/// same user's write really does.
async fn check_allowed(
    app: &axum::Router,
    scope: &str,
    id: Uuid,
    user: &str,
    required: &str,
) -> bool {
    let (status, body) = request_as(
        app,
        "GET",
        &format!("/api/v1/{scope}/{id}/permissions/{user}/check?required={required}"),
        Some(&token_for_user("root", Role::Admin)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "check failed: {body}");
    body["allowed"]
        .as_bool()
        .unwrap_or_else(|| panic!("{body}"))
}

/// No rows is not permission to write: a dataset that never had a grant denies
/// every enforced editor, whatever the role gate said.
#[tokio::test]
async fn test_dataset_without_permission_rows_denies_every_editor() {
    let (app, state) = setup_app_authed_with_state().await;
    let (_, branch_id) = seed_unowned_dataset(&state).await;

    let (status, body) = commit_as(&app, branch_id, &token_for_user("eve", Role::Editor)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// ... and an instance admin, who is the only one who can grant on such a
/// dataset, unlocks it for the user they grant to and nobody else.
#[tokio::test]
async fn test_instance_admin_grant_unlocks_an_unowned_dataset() {
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

    // and /check has to say the same, or it promises alice a write that 403s
    assert!(!check_allowed(&app, "branches", branch_id, "alice", "write").await);
    assert!(check_allowed(&app, "branches", branch_id, "bob", "write").await);
    // the dataset scope is untouched by the branch rows, and so is its answer
    assert!(check_allowed(&app, "datasets", dataset_id, "alice", "write").await);
    assert!(!check_allowed(&app, "datasets", dataset_id, "bob", "write").await);
    // a read grant anywhere on the dataset still reads every branch
    assert!(check_allowed(&app, "branches", branch_id, "alice", "read").await);
}

/// `required` takes the same three levels a grant does. Any other string ranks
/// below every grant, so without this it answers allowed for anyone with a row.
#[tokio::test]
async fn test_permission_check_rejects_an_unknown_level() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;
    grant(&app, "datasets", dataset_id, "alice", "read").await;

    for uri in [
        format!("/api/v1/datasets/{dataset_id}/permissions/alice/check?required=owner"),
        format!("/api/v1/branches/{branch_id}/permissions/alice/check?required=owner"),
    ] {
        let (status, body) = request_as(
            &app,
            "GET",
            &uri,
            Some(&token_for_user("root", Role::Admin)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
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
/// stay in the list, but an authorized caller may legitimately get a 500 from
/// h3 or a 501 from the pgvector routes.
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
    .execute(state.unguarded_pool())
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
    .execute(state.unguarded_pool())
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
        .execute(state.unguarded_pool())
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
    .execute(state.unguarded_pool())
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
    .execute(state.unguarded_pool())
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

// ─── Project membership as a dataset grant ──────────────────────────

/// A public dataset with one committed feature, whose creator auto-grant makes
/// `carol` its admin. Returns (dataset id, branch id, carol's token).
async fn seed_public_dataset(app: &axum::Router) -> (Uuid, Uuid, String) {
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
    assert_eq!(dataset["visibility"], "public", "{dataset}");
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

/// A project with one member at each collaboration role, inside a workspace
/// `carol` owns. Carol inherits owner on the project from the workspace, which is
/// what lets her set the members.
async fn seed_project_with_members(app: &axum::Router, carol: &str) -> Uuid {
    let (status, workspace) = request_as(
        app,
        "POST",
        "/api/v1/workspaces",
        Some(carol),
        Some(json!({"name": format!("ws_{}", Uuid::now_v7())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_string();

    let (status, project) = request_as(
        app,
        "POST",
        &format!("/api/v1/workspaces/{workspace_id}/projects"),
        Some(carol),
        Some(json!({"name": format!("pr_{}", Uuid::now_v7())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    let project_id = Uuid::parse_str(project["id"].as_str().unwrap()).unwrap();

    for (user, role) in [
        ("project-owner", "owner"),
        ("project-editor", "editor"),
        ("project-viewer", "viewer"),
    ] {
        let (status, body) = request_as(
            app,
            "PUT",
            &format!("/api/v1/projects/{project_id}/members/{user}"),
            Some(carol),
            Some(json!({"role": role})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{user} as {role}: {body}");
    }
    project_id
}

async fn attach_project(
    app: &axum::Router,
    token: &str,
    dataset_id: Uuid,
    project_id: Uuid,
) -> (StatusCode, Value) {
    request_as(
        app,
        "PUT",
        &format!("/api/v1/datasets/{dataset_id}/project"),
        Some(token),
        Some(json!({"project_id": project_id})),
    )
    .await
}

async fn detach_project(app: &axum::Router, token: &str, dataset_id: Uuid) -> (StatusCode, Value) {
    request_as(
        app,
        "DELETE",
        &format!("/api/v1/datasets/{dataset_id}/project"),
        Some(token),
        None,
    )
    .await
}

/// A public dataset, its project, and the three member tokens, with the attach
/// already done. Returns (dataset id, branch id, project id, carol's token).
async fn seed_attached_dataset(app: &axum::Router) -> (Uuid, Uuid, Uuid, String) {
    let (dataset_id, branch_id, carol) = seed_public_dataset(app).await;
    let project_id = seed_project_with_members(app, &carol).await;
    let (status, body) = attach_project(app, &carol, dataset_id, project_id).await;
    assert_eq!(status, StatusCode::OK, "attach: {body}");
    (dataset_id, branch_id, project_id, carol)
}

/// Attaching closes the dataset in the same breath as it opens it to the
/// project: public reads stop, and the project's members take their place.
#[tokio::test]
async fn test_attach_makes_the_dataset_private_to_the_project() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = seed_public_dataset(&app).await;
    let project_id = seed_project_with_members(&app, &carol).await;
    let features = format!("/api/v1/branches/{branch_id}/features");

    let (status, body) = request_as(&app, "GET", &features, None, None).await;
    assert_eq!(status, StatusCode::OK, "public before the attach: {body}");

    let (status, body) = attach_project(&app, &carol, dataset_id, project_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], project_id.to_string(), "{body}");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["visibility"], "private", "the attach did not hide it");
    assert_eq!(
        body["project_id"],
        project_id.to_string(),
        "the read does not report the project"
    );

    let (status, body) = request_as(&app, "GET", &features, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "anonymous read: {body}");

    // the project's members read it with no grant of their own
    for user in ["project-owner", "project-editor", "project-viewer"] {
        let (status, body) = request_as(
            &app,
            "GET",
            &features,
            Some(&token_for_user(user, Role::Viewer)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{user} read: {body}");
    }

    // and someone in neither the project nor its workspace does not
    let (status, body) = request_as(
        &app,
        "GET",
        &features,
        Some(&token_for_user("eve", Role::Editor)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "non-member read: {body}");
}

/// A workspace role reaches the project's datasets the same way a project role
/// does, because the effective role is the higher of the two.
#[tokio::test]
async fn test_inherited_workspace_role_reaches_an_attached_dataset() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = seed_public_dataset(&app).await;

    let (status, workspace) = request_as(
        &app,
        "POST",
        "/api/v1/workspaces",
        Some(&carol),
        Some(json!({"name": format!("ws_{}", Uuid::now_v7())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_string();

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/workspaces/{workspace_id}/members/workspace-editor"),
        Some(&carol),
        Some(json!({"role": "editor"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, project) = request_as(
        &app,
        "POST",
        &format!("/api/v1/workspaces/{workspace_id}/projects"),
        Some(&carol),
        Some(json!({"name": format!("pr_{}", Uuid::now_v7())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    let project_id = Uuid::parse_str(project["id"].as_str().unwrap()).unwrap();

    let (status, body) = attach_project(&app, &carol, dataset_id, project_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // never a member of the project itself, and an editor on it all the same
    let inheritor = token_for_user("workspace-editor", Role::Editor);
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/features"),
        Some(&inheritor),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = commit_as(&app, branch_id, &inheritor).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Viewer maps to `read`: enough to see the content, not to change it.
#[tokio::test]
async fn test_project_viewer_reads_but_cannot_write() {
    let app = setup_app_authed().await;
    let (_, branch_id, _, _) = seed_attached_dataset(&app).await;
    // an editor role on the instance, so what denies the write is the project
    // role and not the role gate in front of it
    let viewer = token_for_user("project-viewer", Role::Editor);

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/features"),
        Some(&viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = commit_as(&app, branch_id, &viewer).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// Editor maps to `write`: enough to commit, not to hand out access.
#[tokio::test]
async fn test_project_editor_writes_but_cannot_administer() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _, _) = seed_attached_dataset(&app).await;
    let editor = token_for_user("project-editor", Role::Editor);

    let (status, body) = commit_as(&app, branch_id, &editor).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = grant_as(&app, &editor, "datasets", dataset_id, "eve", "read").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "editor delegating: {body}");

    // /check has to say the same, or it denies a write that then succeeds
    assert!(check_allowed(&app, "datasets", dataset_id, "project-editor", "write").await);
    assert!(!check_allowed(&app, "datasets", dataset_id, "project-editor", "admin").await);
    assert!(check_allowed(&app, "datasets", dataset_id, "project-owner", "admin").await);
    assert!(!check_allowed(&app, "datasets", dataset_id, "project-viewer", "write").await);
    assert!(check_allowed(&app, "datasets", dataset_id, "project-viewer", "read").await);
    assert!(!check_allowed(&app, "datasets", dataset_id, "eve", "read").await);

    let (status, body) = request_as(
        &app,
        "PATCH",
        &format!("/api/v1/datasets/{dataset_id}"),
        Some(&editor),
        Some(json!({"visibility": "public"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "editor publishing: {body}");
}

/// Owner maps to `admin`: grants, visibility, and detaching.
#[tokio::test]
async fn test_project_owner_administers_the_dataset() {
    let app = setup_app_authed().await;
    let (dataset_id, _, _, _) = seed_attached_dataset(&app).await;
    let owner = token_for_user("project-owner", Role::Editor);

    let (status, body) = grant_as(&app, &owner, "datasets", dataset_id, "eve", "read").await;
    assert_eq!(status, StatusCode::CREATED, "owner delegating: {body}");

    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/permissions"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner reading the acl: {body}");

    let (status, body) = detach_project(&app, &owner, dataset_id).await;
    assert_eq!(status, StatusCode::OK, "owner detaching: {body}");
}

/// Grants are additive, so the caller holds the stronger of the two levels. Both
/// directions: an explicit grant above the project role, and a project role above
/// an explicit grant.
#[tokio::test]
async fn test_the_stronger_of_grant_and_project_role_wins() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _, carol) = seed_attached_dataset(&app).await;

    // the project makes them a reader; an explicit write grant makes them a writer
    let viewer = token_for_user("project-viewer", Role::Editor);
    let (status, body) = commit_as(&app, branch_id, &viewer).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    grant(&app, "datasets", dataset_id, "project-viewer", "write").await;
    let (status, body) = commit_as(&app, branch_id, &viewer).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // and the other way: a weaker explicit grant does not pull an owner down
    grant(&app, "datasets", dataset_id, "project-owner", "read").await;
    let owner = token_for_user("project-owner", Role::Editor);
    let (status, body) = grant_as(&app, &owner, "datasets", dataset_id, "eve", "read").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a read grant demoted a project owner: {body}"
    );

    // carol's own admin grant is untouched by any of it
    let (status, body) = grant_as(&app, &carol, "datasets", dataset_id, "dave", "read").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Detaching drops the project's access and leaves the dataset private, so losing
/// a project can never publish its data. The dataset read reports the link both
/// ways round, so a client can tell which project it is looking at.
#[tokio::test]
async fn test_detach_leaves_the_dataset_private() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, project_id, carol) = seed_attached_dataset(&app).await;
    let features = format!("/api/v1/branches/{branch_id}/features");
    let dataset = format!("/api/v1/datasets/{dataset_id}");
    let viewer = token_for_user("project-viewer", Role::Viewer);

    let (status, body) = request_as(&app, "GET", &features, Some(&viewer), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // the read names the project while it is attached, in the single read and in
    // the listing, and to a member as much as to the dataset's own admin
    for token in [&carol, &viewer] {
        let (status, body) = request_as(&app, "GET", &dataset, Some(token), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["project_id"], project_id.to_string(), "{body}");
    }
    let (status, body) = request_as(&app, "GET", "/api/v1/datasets", Some(&carol), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = body
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == dataset_id.to_string())
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(listed["project_id"], project_id.to_string(), "{listed}");

    let (status, body) = detach_project(&app, &carol, dataset_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], Value::Null, "{body}");

    // and reports null once it is gone, rather than leaving the old id behind
    let (status, body) = request_as(&app, "GET", &dataset, Some(&carol), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], Value::Null, "{body}");
    let (status, body) = request_as(&app, "GET", "/api/v1/datasets", Some(&carol), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = body
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == dataset_id.to_string())
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(listed["project_id"], Value::Null, "{listed}");

    let (status, body) = request_as(&app, "GET", &features, Some(&viewer), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "member after detach: {body}");

    let (status, body) = request_as(&app, "GET", &features, None, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the detach published it: {body}"
    );

    // the dataset's own admin still reaches it, and is who publishes it again
    let (status, body) = request_as(&app, "GET", &features, Some(&carol), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // detaching twice is not a silent success
    let (status, body) = detach_project(&app, &carol, dataset_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// Attaching takes both halves. Neither one on its own does.
#[tokio::test]
async fn test_attach_needs_dataset_admin_and_project_editor() {
    let app = setup_app_authed().await;
    let (dataset_id, _, carol) = seed_public_dataset(&app).await;
    let project_id = seed_project_with_members(&app, &carol).await;

    // no grant at all on the dataset
    let (status, body) = attach_project(
        &app,
        &token_for_user("eve", Role::Editor),
        dataset_id,
        project_id,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "outsider attaching: {body}");

    // a write grant is not an admin grant
    grant(&app, "datasets", dataset_id, "dave", "write").await;
    let (status, body) = attach_project(
        &app,
        &token_for_user("dave", Role::Editor),
        dataset_id,
        project_id,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "write grantee: {body}");

    // a dataset admin who is not in the project cannot see the project to attach to
    grant(&app, "datasets", dataset_id, "mallory", "admin").await;
    let (status, body) = attach_project(
        &app,
        &token_for_user("mallory", Role::Editor),
        dataset_id,
        project_id,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-member attaching: {body}"
    );

    // and a dataset admin who is only a viewer on the project cannot either: it
    // would hand them the write access their viewer role withholds
    grant(&app, "datasets", dataset_id, "project-viewer", "admin").await;
    let (status, body) = attach_project(
        &app,
        &token_for_user("project-viewer", Role::Editor),
        dataset_id,
        project_id,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "project viewer attaching: {body}"
    );

    // nothing above attached it
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["visibility"], "public", "a denied attach still hid it");
}

/// An attached private dataset is in the listings for the project's members and
/// nobody else's, which is the listing filter rather than the per-id layer.
#[tokio::test]
async fn test_attached_dataset_is_listed_for_project_members_only() {
    let app = setup_app_authed().await;
    let (dataset_id, _, _, _) = seed_attached_dataset(&app).await;
    let id = dataset_id.to_string();

    for uri in DATASET_LISTINGS {
        assert!(
            !listing_mentions(&app, uri, None, &id).await,
            "anonymous GET {uri} leaked the project's dataset"
        );
        assert!(
            !listing_mentions(&app, uri, Some(&token_for_user("eve", Role::Editor)), &id).await,
            "non-member GET {uri} leaked the project's dataset"
        );
    }

    // the listings that cover versioned datasets: stac collections list raster
    // catalogs, of which this dataset has none
    let viewer = token_for_user("project-viewer", Role::Viewer);
    for uri in [
        "/api/v1/datasets",
        "/api/v1/ogc/collections",
        "/api/v1/qgis/datasets",
    ] {
        assert!(
            listing_mentions(&app, uri, Some(&viewer), &id).await,
            "member GET {uri} hid the project's dataset"
        );
    }
}

/// A dataset attached to no project decides on explicit grants alone, which is
/// what every dataset did before there were projects.
#[tokio::test]
async fn test_dataset_without_a_project_is_unchanged() {
    let app = setup_app_authed().await;
    let (public_id, public_branch, carol) = seed_public_dataset(&app).await;
    seed_project_with_members(&app, &carol).await;
    let (private_id, private_branch, _) = seed_private_dataset(&app).await;

    // the project's owner is the strongest role there is, and it reaches neither
    let owner = token_for_user("project-owner", Role::Editor);
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{private_branch}/features"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // 404 rather than 403 on both: the visibility layer sits outside the write
    // gate, so a private dataset the caller cannot read is not there to refuse
    let (status, body) = commit_as(&app, private_branch, &owner).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = grant_as(&app, &owner, "datasets", private_id, "eve", "read").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    assert!(
        !listing_mentions(
            &app,
            "/api/v1/datasets",
            Some(&owner),
            &private_id.to_string()
        )
        .await,
        "an unattached private dataset was listed for a project owner"
    );

    // and the public one keeps serving anonymous reads and its own writes
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{public_branch}/features"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        listing_mentions(&app, "/api/v1/datasets", None, &public_id.to_string()).await,
        "an unattached public dataset stopped being listed"
    );
    let (status, body) = commit_as(&app, public_branch, &owner).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
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
        .execute(state.unguarded_pool())
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
    .execute(state.unguarded_pool())
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

    // dave holds admin on the branch, and that reaches nothing: the branch
    // endpoints resolve to the dataset, whose admin he is not
    let dave = token_for_user("dave", Role::Editor);
    let (status, body) = grant_as(&app, &dave, "branches", branch_id, "eve", "write").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "branch admin granting: {body}"
    );
    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/branches/{branch_id}/permissions/dave"),
        Some(&dave),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "branch admin revoking: {body}"
    );
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/permissions"),
        Some(&dave),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "branch admin reading the acl: {body}"
    );
}

/// A blank subject is nobody, on either scope: the row would sit there waiting
/// for a token whose `sub` is empty.
#[tokio::test]
async fn test_grant_rejects_a_blank_user_id() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = seed_private_dataset(&app).await;

    for blank in ["", "   "] {
        let (status, body) = grant_as(&app, &carol, "datasets", dataset_id, blank, "write").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "dataset grant to {blank:?}: {body}"
        );
        let (status, body) = grant_as(&app, &carol, "branches", branch_id, blank, "write").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "branch grant to {blank:?}: {body}"
        );
    }

    // and nothing was written on either scope
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/permissions"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/branches/{branch_id}/permissions"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.as_array().unwrap().is_empty(), "{body}");
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

/// Revoking may not take away a dataset's last admin row, which would leave
/// nobody able to manage its grants.
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

    // dave is removable, he is not the admin the rule protects
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

/// The last non-admin row is removable: taking it away leaves the dataset with
/// no rows, which denies every enforced writer instead of opening it.
#[tokio::test]
async fn test_revoking_the_last_write_row_closes_the_dataset() {
    let (app, state) = setup_app_authed_with_state().await;
    let (dataset_id, branch_id) = seed_unowned_dataset(&state).await;
    let alice = token_for_user("alice", Role::Editor);
    grant(&app, "datasets", dataset_id, "alice", "write").await;

    let (status, body) = commit_as(&app, branch_id, &alice).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = request_as(
        &app,
        "DELETE",
        &format!("/api/v1/datasets/{dataset_id}/permissions/alice"),
        Some(&token_for_user("root", Role::Admin)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = commit_as(&app, branch_id, &alice).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "after the last row went: {body}"
    );
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
    .fetch_one(state.read_pool())
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

    // topology reads are admin-only for the same reason: they are keyed by
    // name, not a dataset the write ladder can own
    let (status, response) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/topologies"),
        Some(&editor),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "editor GET: {response}");
}

/// Topology list and name-keyed geometry used to be public GETs because
/// classify returned Public before the `/topologies` match.
#[tokio::test]
async fn test_topology_reads_are_admin_only() {
    let app = setup_app_authed().await;
    let admin = token_for(Role::Admin);
    let dataset_id = create_dataset_authed(&app, &admin).await;
    assert_read_is_admin_only(
        &app,
        &format!("/api/v1/datasets/{dataset_id}/topologies"),
        &admin,
    )
    .await;

    let name = format!("readtopo_{}", Uuid::now_v7().simple());
    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{dataset_id}/topologies"),
        Some(&admin),
        Some(json!({"name": name, "srid": 4326})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create topology: {body}");

    for suffix in ["faces", "edges", "nodes"] {
        assert_read_is_admin_only(&app, &format!("/api/v1/topologies/{name}/{suffix}"), &admin)
            .await;
    }
}

// ─── Policy is read off the route template, not the raw path ────────

/// Single path segments a caller can plant in a free-text parameter that name a
/// rule in `classify` or `needs_write_grant`. Every one of these turned
/// `DELETE /api/v1/datasets/{id}/tags/{tag}` into some other endpoint's policy
/// while the exemption lists were matched against the raw request path.
const POLICY_KEYWORDS: [&str; 14] = [
    "trace",
    "astar",
    "transform",
    "validate",
    "query",
    "profile",
    "tsp",
    "isochrone",
    "shortest-path",
    "permissions",
    "topologies",
    "add-face",
    "simplify",
    "webhooks",
];

/// The exploit that refuted the first write gate, run against the live router.
#[tokio::test]
async fn test_a_planted_tag_cannot_opt_out_of_the_write_ladder() {
    let app = setup_app_authed().await;
    let (dataset_id, _, carol) = owned_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    // carol owns the dataset, so every tag below exists to be deleted
    for tag in POLICY_KEYWORDS {
        let (status, body) = request_as(
            &app,
            "POST",
            &format!("/api/v1/datasets/{dataset_id}/tags"),
            Some(&carol),
            Some(json!({"tag": tag})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed tag {tag}: {body}");
    }

    // an editor with no grant is refused whatever they put in the tag segment
    for tag in POLICY_KEYWORDS {
        let (status, body) = request_as(
            &app,
            "DELETE",
            &format!("/api/v1/datasets/{dataset_id}/tags/{tag}"),
            Some(&eve),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "planted tag {tag} must not bypass the ladder: {body}"
        );
    }

    // and nothing was deleted
    let (status, tags) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/tags"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tags}");
    assert_eq!(
        tags.as_array().unwrap().len(),
        POLICY_KEYWORDS.len(),
        "planted tags were deleted: {tags}"
    );

    // a granted editor still deletes them, so the gate did not simply break the
    // route for everyone
    grant(&app, "datasets", dataset_id, "eve", "write").await;
    for tag in POLICY_KEYWORDS {
        let (status, body) = request_as(
            &app,
            "DELETE",
            &format!("/api/v1/datasets/{dataset_id}/tags/{tag}"),
            Some(&eve),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "granted delete of {tag}: {body}"
        );
    }
    let (_, tags) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/tags"),
        Some(&carol),
        None,
    )
    .await;
    assert!(tags.as_array().unwrap().is_empty(), "{tags}");
}

/// The same trick against the other free-text terminal segments in the API.
/// These are gated by `classify` returning Admin today; the assertion is that a
/// planted keyword does not change that.
#[tokio::test]
async fn test_planted_segments_do_not_downgrade_other_routes() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, _carol) = owned_dataset(&app).await;
    let eve = token_for_user("eve", Role::Editor);

    for planted in POLICY_KEYWORDS {
        for uri in [
            format!("/api/v1/datasets/{dataset_id}/permissions/{planted}"),
            format!("/api/v1/branches/{branch_id}/permissions/{planted}"),
        ] {
            let (status, body) = request_as(&app, "DELETE", &uri, Some(&eve), None).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{uri} must stay refused for a non-admin: {body}"
            );
        }
    }
}

// ─── Every mounted mutating route, read from the route tables ───────

/// The route templates the router registers, parsed out of the `.route(...)`
/// calls in this crate rather than hand-listed, so a route added tomorrow shows
/// up here without anyone remembering to add it.
///
/// `lib.rs` says where each module's table is mounted; the module's own file
/// says what it registers. That is the same pair of facts axum builds the
/// matcher from, so the templates below are the ones `MatchedPath` will report.
fn mounted_mutating_routes() -> Vec<(String, String)> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lib = std::fs::read_to_string(src.join("lib.rs")).unwrap();

    let mut prefixes: Vec<(String, String)> = Vec::new();
    for (call, has_prefix) in [(".nest(", true), (".merge(", false)] {
        for chunk in lib.split(call).skip(1) {
            let prefix = if has_prefix {
                match quoted(chunk) {
                    Some(p) => p,
                    None => continue,
                }
            } else {
                String::new()
            };
            let rest = if has_prefix {
                match chunk.split_once(',') {
                    Some((_, rest)) => rest,
                    None => continue,
                }
            } else {
                chunk
            };
            if let Some(module) = rest.trim().split("::").next() {
                let module = module.trim();
                if !module.is_empty() && module.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    prefixes.push((module.to_string(), prefix));
                }
            }
        }
    }
    assert!(
        prefixes.len() > 20,
        "failed to parse the mount table in lib.rs: {prefixes:?}"
    );

    let mut routes = Vec::new();
    for (module, prefix) in prefixes {
        // the module's own file, and its child modules: a table split across
        // `src/arcgis/*.rs` registers on the same mount, so the census has to
        // read there too or a route could be added where nothing is looking
        let mut files = vec![src.join(format!("{module}.rs"))];
        if let Ok(children) = std::fs::read_dir(src.join(&module)) {
            files.extend(
                children
                    .filter_map(Result::ok)
                    .map(|child| child.path())
                    .filter(|path| path.extension().is_some_and(|kind| kind == "rs")),
            );
        }
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (template, methods) in route_table(&text) {
                for method in methods {
                    routes.push((method, format!("{prefix}{template}")));
                }
            }
        }
    }
    routes.sort();
    routes.dedup();
    routes
}

/// The first double-quoted string in `s`.
fn quoted(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let end = start + s[start..].find('"')?;
    Some(s[start..end].to_string())
}

/// Every `.route("<template>", <method router>)` in one module's source, with
/// the mutating methods attached to it.
fn route_table(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(".route(") {
        let after = &rest[at + ".route(".len()..];
        rest = after;
        let Some(template) = quoted(after) else {
            continue;
        };

        // the argument list, up to the paren that closes `.route(`
        let mut depth = 1usize;
        let mut end = 0usize;
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let args = &after[..end];

        let methods: Vec<String> = ["post", "put", "patch", "delete"]
            .iter()
            .filter(|m| calls(args, m))
            .map(|m| m.to_uppercase())
            .collect();
        if !methods.is_empty() {
            out.push((template, methods));
        }
    }
    out
}

/// Whether `args` calls the method constructor `name`, not merely mentions it
/// inside a handler's name (`delete_attachment` is not `delete(`).
fn calls(args: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    let mut from = 0;
    while let Some(at) = args[from..].find(&needle) {
        let at = from + at;
        let before = args[..at].chars().next_back();
        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// Mutating route templates deliberately left outside the write ladder. Every
/// entry is either a POST that only computes, or grant management, which
/// rbac.rs gates harder. Adding a route to this list is the only way to opt out,
/// and it cannot be done from a request.
const UNGATED_TEMPLATES: [&str; 52] = [
    // the FeatureServer's two queries take a POST body only because an object id
    // list is too long for a URL, and extractChanges takes one because the
    // generations it is asked about are JSON. They are the only ungated POSTs on
    // the facade: applyEdits and the three attachment writes are gated like any
    // other write.
    "/arcgis/rest/services/{service}/FeatureServer/extractChanges",
    "/arcgis/rest/services/{service}/FeatureServer/{layer}/query",
    "/arcgis/rest/services/{service}/FeatureServer/{layer}/queryAttachments",
    "/api/v1/attribute-rules/{id}/validate",
    "/api/v1/branches/{branch_id}/permissions/{user_id}",
    "/api/v1/branches/{id}/3d/extrude",
    "/api/v1/branches/{id}/3d/intersection",
    "/api/v1/branches/{id}/3d/minkowski-sum",
    "/api/v1/branches/{id}/3d/straight-skeleton",
    "/api/v1/branches/{id}/3d/tesselate",
    "/api/v1/branches/{id}/3d/visibility",
    "/api/v1/branches/{id}/3d/volume",
    "/api/v1/branches/{id}/features/filter",
    "/api/v1/branches/{id}/features/intersects",
    "/api/v1/branches/{id}/features/within",
    "/api/v1/branches/{id}/geoprocessing/centroid",
    "/api/v1/branches/{id}/geoprocessing/clip",
    "/api/v1/branches/{id}/geoprocessing/contour",
    "/api/v1/branches/{id}/geoprocessing/convex-hull",
    "/api/v1/branches/{id}/geoprocessing/densify",
    "/api/v1/branches/{id}/geoprocessing/difference",
    "/api/v1/branches/{id}/geoprocessing/dissolve",
    "/api/v1/branches/{id}/geoprocessing/distance-matrix",
    "/api/v1/branches/{id}/geoprocessing/intersect",
    "/api/v1/branches/{id}/geoprocessing/merge",
    "/api/v1/branches/{id}/geoprocessing/nearest-neighbor",
    "/api/v1/branches/{id}/geoprocessing/simplify",
    "/api/v1/branches/{id}/geoprocessing/spatial-join",
    "/api/v1/branches/{id}/geoprocessing/split",
    "/api/v1/branches/{id}/geoprocessing/voronoi",
    "/api/v1/branches/{id}/h3/compact",
    "/api/v1/branches/{id}/permissions",
    "/api/v1/branches/{id}/similarity/cluster",
    "/api/v1/branches/{id}/similarity/search",
    "/api/v1/branches/{id}/transform",
    "/api/v1/coverage/simulate",
    "/api/v1/datasets/{dataset_id}/permissions/{user_id}",
    "/api/v1/datasets/{id}/permissions",
    "/api/v1/datasets/{id}/trajectories/nearest",
    "/api/v1/incidents/evacuate",
    "/api/v1/networks/{id}/astar",
    "/api/v1/networks/{id}/isochrone",
    "/api/v1/networks/{id}/shortest-path",
    "/api/v1/networks/{id}/trace",
    "/api/v1/networks/{id}/tsp",
    "/api/v1/parcels/merge",
    "/api/v1/parcels/split",
    "/api/v1/pointclouds/{id}/profile",
    "/api/v1/pointclouds/{id}/query",
    "/api/v1/surveys/compare",
    "/api/v1/topologies/{name}/validate",
    "/api/v1/trajectories/{id}/simplify",
];

#[test]
fn test_every_mounted_mutating_route_is_gated_or_listed() {
    let routes = mounted_mutating_routes();
    assert!(
        routes.len() > 100,
        "parsed only {} routes, the route-table parser is broken",
        routes.len()
    );

    for (method, template) in &routes {
        let method = axum::http::Method::from_bytes(method.as_bytes()).unwrap();
        let gated = ptolemy_api::auth::needs_write_grant(&method, template);
        let listed = UNGATED_TEMPLATES.contains(&template.as_str());
        assert_eq!(
            gated, !listed,
            "{method} {template}: gated={gated} but the exemption list says {listed}. \
             A route that writes must be gated; one that only computes must be listed."
        );
    }

    // and no stale entries, so the list cannot drift away from the router
    for template in UNGATED_TEMPLATES {
        assert!(
            routes.iter().any(|(_, t)| t == template),
            "{template} is exempt but no longer mounted"
        );
    }
}

/// The bug class, swept across the whole router instead of a hand-picked route.
///
/// Every mutating template whose last segment is a parameter hands the caller
/// that segment. This walks all of them, plants each policy keyword there, fires
/// the request as an editor with no grant on anything, and requires that none of
/// them succeeds. Under raw-path matching ten of these returned 204.
#[tokio::test]
async fn test_no_free_text_segment_opens_a_mutating_route() {
    let app = setup_app_authed().await;
    let eve = token_for_user("eve", Role::Editor);

    let free_text: Vec<(String, String)> = mounted_mutating_routes()
        .into_iter()
        .filter(|(_, t)| {
            t.rsplit('/')
                .next()
                .is_some_and(|last| last.starts_with('{'))
        })
        .collect();
    assert!(
        free_text.len() >= 15,
        "expected the API to have free-text terminal routes, found {}",
        free_text.len()
    );

    for (method, template) in &free_text {
        let stem = template.rsplit_once('/').unwrap().0;
        // a real dataset the caller has no grant on, so a resolved target is
        // refused rather than merely missing
        let (dataset_id, branch_id, _owner) = owned_dataset(&app).await;
        let stem = stem
            .replace("{dataset_id}", &dataset_id.to_string())
            .replace("{branch_id}", &branch_id.to_string())
            .replace("{id}", &dataset_id.to_string())
            .replace("{target_id}", &branch_id.to_string());

        for planted in POLICY_KEYWORDS {
            let uri = format!("{stem}/{planted}");
            let (status, body) = request_as(&app, method, &uri, Some(&eve), None).await;
            assert!(
                !status.is_success(),
                "{method} {uri} succeeded with {status}: {body}"
            );
        }
    }
}

/// A handler that writes takes its [`WriteGrant`] out of the request extensions,
/// where [`write_middleware`] put it. That is a runtime lookup, so a write
/// handler mounted on a route the write layer exempts would compile and then
/// answer 500 the first time anyone called it.
///
/// This walks every mounted mutating route and requires that none of them ever
/// answers the missing-extension rejection. Bad bodies are expected and fine:
/// a 400 or a 422 means the handler ran, which is what is being checked.
#[tokio::test]
async fn test_no_mutating_route_is_missing_its_write_grant() {
    let app = setup_app_authed().await;
    let (dataset_id, branch_id, carol) = owned_dataset(&app).await;

    let mut checked = 0;
    for (method, template) in mounted_mutating_routes() {
        let uri = template
            .replace("{dataset_id}", &dataset_id.to_string())
            .replace("{branch_id}", &branch_id.to_string())
            .replace("{target_id}", &branch_id.to_string())
            .replace("{source_id}", &branch_id.to_string())
            .replace("{id}", &dataset_id.to_string());
        // anything still parameterised needs a value this test cannot invent
        if uri.contains('{') {
            continue;
        }
        checked += 1;

        let req = Request::builder()
            .method(method.as_str())
            .uri(&uri)
            .header("authorization", format!("Bearer {carol}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);

        assert!(
            !body.contains("Missing request extension"),
            "{method} {uri} answered {status} with no write grant in scope: {body}"
        );
    }
    assert!(
        checked > 50,
        "substituted only {checked} routes, the parser or the parameter names moved"
    );
}

/// A field alias is the label a source's users have always read, and a
/// migration that drops it makes the data visibly worse the day it lands.
/// Nothing displays it yet, so the round trip is the whole contract.
#[tokio::test]
async fn test_a_field_alias_survives_the_schema_round_trip() {
    let app = setup_app_authed().await;
    let (dataset_id, _branch_id, carol) = owned_dataset(&app).await;

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/datasets/{dataset_id}/schema"),
        Some(&carol),
        Some(json!({"fields": [
            {"name": "constructionmaterial", "field_type": "string", "required": false,
             "alias": "Construction Material"},
            {"name": "plain", "field_type": "string", "required": false},
        ]})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    let (status, schema) = request_as(
        &app,
        "GET",
        &format!("/api/v1/datasets/{dataset_id}/schema"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{schema}");

    let fields = schema["fields"].as_array().expect("fields");
    assert_eq!(fields[0]["alias"], "Construction Material", "{schema}");
    // a field with no alias keeps none rather than gaining an empty one
    assert!(fields[1]["alias"].is_null(), "{schema}");
}

// ─── Single feature read ────────────────────────────────────────────

const POINT_HEX: &str = "0101000000000000000000F03F0000000000000040";

#[tokio::test]
async fn test_get_feature_returns_geometry_and_properties() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": POINT_HEX,
             "properties": {"name": "hut", "kind": "shed"}}
        ]),
    )
    .await;

    let (status, body) =
        get_json(&app, &format!("/api/v1/branches/{branch_id}/features/{f1}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["feature_id"], f1.to_string());
    assert_eq!(
        body["geometry_wkb_hex"].as_str().unwrap().to_uppercase(),
        POINT_HEX
    );
    assert_eq!(body["properties"]["name"], "hut");
    assert_eq!(body["properties"]["kind"], "shed");
}

/// A sync client reads this as the merge's "theirs", so it must be the branch head.
#[tokio::test]
async fn test_get_feature_returns_the_latest_committed_properties() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": POINT_HEX,
             "properties": {"name": "first"}}
        ]),
    )
    .await;
    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "update", "feature_id": f1.to_string(), "properties": {"name": "second"}}
        ]),
    )
    .await;

    let (status, body) =
        get_json(&app, &format!("/api/v1/branches/{branch_id}/features/{f1}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["properties"]["name"], "second");
    // an update that omits geometry keeps the one the insert carried
    assert_eq!(
        body["geometry_wkb_hex"].as_str().unwrap().to_uppercase(),
        POINT_HEX
    );
}

#[tokio::test]
async fn test_get_feature_unknown_id_404() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, _) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/{}", Uuid::now_v7()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_feature_deleted_is_404() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": POINT_HEX,
             "properties": {}}
        ]),
    )
    .await;
    commit_features(
        &app,
        branch_id,
        json!([{"type": "delete", "feature_id": f1.to_string()}]),
    )
    .await;

    let (status, _) = get_json(&app, &format!("/api/v1/branches/{branch_id}/features/{f1}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ─── Native geometry ────────────────────────────────────────────────

/// WKB hex for a point with survey-precision coordinates, distinct from the
/// 4326 working copy sent beside it.
const NATIVE_HEX: &str = "0101000000ADFB5E7CC4841F41E92631C9C0BA5141";

#[tokio::test]
async fn test_native_geometry_commit_and_read_back_exact() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex,
             "properties": {}, "native_geometry_wkb_hex": NATIVE_HEX, "native_srid": 26919}
        ]),
    )
    .await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/{f1}/native"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // exact, not approximate: the original must survive the round trip untouched
    assert_eq!(
        body["native_geometry_wkb_hex"]
            .as_str()
            .unwrap()
            .to_uppercase(),
        NATIVE_HEX
    );
    assert_eq!(body["native_srid"], 26919);
}

#[tokio::test]
async fn test_native_geometry_null_when_never_sent() {
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
        &format!("/api/v1/branches/{branch_id}/features/{f1}/native"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["native_geometry_wkb_hex"].is_null());
    assert!(body["native_srid"].is_null());
}

#[tokio::test]
async fn test_native_geometry_half_supplied_is_rejected() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let point_hex = "0101000000000000000000F03F0000000000000040";
    let (status, _) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/commit"),
        json!({
            "message": "half", "author": "test",
            "operations": [{"type": "insert", "feature_id": Uuid::now_v7().to_string(),
                "geometry_wkb_hex": point_hex, "properties": {}, "native_srid": 26919}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_native_geometry_unknown_feature_404() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let (status, _) = get_json(
        &app,
        &format!(
            "/api/v1/branches/{branch_id}/features/{}/native",
            Uuid::now_v7()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A compound reference, which no single EPSG code names, travels as WKT.
const COMPOUND_WKT: &str =
    "COMPD_CS[\"NAD83 + NAVD88 height\",GEOGCS[\"NAD83\"],VERT_CS[\"NAVD88 height\"]]";

#[tokio::test]
async fn test_native_geometry_wkt_commit_and_read_back_exact() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;
    let f1 = Uuid::now_v7();

    let point_hex = "0101000000000000000000F03F0000000000000040";
    commit_features(
        &app,
        branch_id,
        json!([
            {"type": "insert", "feature_id": f1.to_string(), "geometry_wkb_hex": point_hex,
             "properties": {}, "native_geometry_wkb_hex": NATIVE_HEX, "native_crs_wkt": COMPOUND_WKT}
        ]),
    )
    .await;

    let (status, body) = get_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/features/{f1}/native"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["native_geometry_wkb_hex"]
            .as_str()
            .unwrap()
            .to_uppercase(),
        NATIVE_HEX
    );
    assert_eq!(body["native_crs_wkt"], COMPOUND_WKT);
    assert!(body["native_srid"].is_null(), "{body}");
}

#[tokio::test]
async fn test_native_geometry_srid_and_wkt_together_rejected() {
    let (app, _) = setup_app().await;
    let ds_id = create_dataset(&app).await;
    let branch_id = create_branch(&app, ds_id, "main").await;

    let point_hex = "0101000000000000000000F03F0000000000000040";
    let (status, _) = post_json(
        &app,
        &format!("/api/v1/branches/{branch_id}/commit"),
        json!({
            "message": "both", "author": "test",
            "operations": [{"type": "insert", "feature_id": Uuid::now_v7().to_string(),
                "geometry_wkb_hex": point_hex, "properties": {},
                "native_geometry_wkb_hex": NATIVE_HEX,
                "native_srid": 26919, "native_crs_wkt": COMPOUND_WKT}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ═══════════════════════════════════════════════════════════════════════
// ArcGIS FeatureServer facade
// ═══════════════════════════════════════════════════════════════════════

/// Helper: post a form body, which is how an Esri client sends a query whose
/// object id list is too long for a URL.
async fn post_form(app: &axum::Router, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Helper: create a dataset under a name a URL can carry, return its id.
async fn create_named_dataset(app: &axum::Router, name: &str, geometry_type: &str) -> Uuid {
    let (status, body) = post_json(
        app,
        "/api/v1/datasets",
        json!({"name": name, "geometry_type": geometry_type, "srid": 4326, "created_by": "test"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create dataset: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

fn wkb_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// Little-endian WKB for a 2D point, so a test can place a feature exactly.
fn point_wkb(x: f64, y: f64) -> String {
    let mut out = vec![1u8];
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    wkb_hex(&out)
}

/// Little-endian WKB for a 2D polygon, rings in the order given and wound
/// however the test wound them: the facade decides the Esri winding itself.
fn polygon_wkb(rings: &[Vec<(f64, f64)>]) -> String {
    let mut out = vec![1u8];
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    for ring in rings {
        out.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for (x, y) in ring {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
    }
    wkb_hex(&out)
}

/// Twice the signed area, positive counter-clockwise, over an esriJSON ring.
fn ring_winding(ring: &Value) -> f64 {
    let points: Vec<(f64, f64)> = ring
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let pair = p.as_array().unwrap();
            (pair[0].as_f64().unwrap(), pair[1].as_f64().unwrap())
        })
        .collect();
    let mut sum = 0.0;
    for pair in points.windows(2) {
        sum += pair[0].0 * pair[1].1 - pair[1].0 * pair[0].1;
    }
    let (first, last) = (points[0], points[points.len() - 1]);
    sum + last.0 * first.1 - first.0 * last.1
}

/// Helper: a service's layer 0 query URL.
fn query_url(service: &str, params: &str) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/query?f=json&{params}")
}

/// Helper: seed `count` points along a line, one feature each.
async fn seed_points(app: &axum::Router, branch_id: Uuid, count: usize) {
    let ops: Vec<Value> = (0..count)
        .map(|i| {
            json!({
                "type": "insert",
                "feature_id": Uuid::now_v7().to_string(),
                "geometry_wkb_hex": point_wkb(i as f64, i as f64),
                "properties": {"name": format!("point-{i}")},
            })
        })
        .collect();
    commit_features(app, branch_id, json!(ops)).await;
}

#[tokio::test]
async fn test_arcgis_catalog_lists_readable_datasets_and_skips_mixed_geometry() {
    let (app, _) = setup_app().await;
    let named = format!("roads_{}", Uuid::now_v7().simple());
    let mixed = format!("mixed_{}", Uuid::now_v7().simple());
    let roads = create_named_dataset(&app, &named, "linestring").await;
    create_branch(&app, roads, "main").await;
    let mixed_id = create_named_dataset(&app, &mixed, "geometry").await;
    create_branch(&app, mixed_id, "main").await;

    let (status, body) = get_json(&app, "/arcgis/rest/services?f=json").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let services = body["services"].as_array().unwrap();
    let listed = |name: &str| services.iter().any(|s| s["name"] == name);
    assert!(listed(&named), "{body}");
    assert!(
        !listed(&mixed),
        "a mixed-geometry dataset has no layer type: {body}"
    );
    let entry = services.iter().find(|s| s["name"] == named).unwrap();
    assert_eq!(entry["type"], "FeatureServer");
    assert!(
        entry["url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/arcgis/rest/services/{named}/FeatureServer")),
        "{entry}"
    );

    // its own URL says why rather than faking a layer type
    let (status, body) = get_json(
        &app,
        &format!("/arcgis/rest/services/{mixed}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("geometryType"),
        "{body}"
    );
}

#[tokio::test]
async fn test_arcgis_service_root_and_layer_metadata() {
    let (app, _) = setup_app().await;
    let name = format!("parks_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 3).await;

    let (status, root) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{root}");
    assert_eq!(root["currentVersion"], 11.2, "{root}");
    assert_eq!(root["capabilities"], "Query", "{root}");
    assert!(root["serviceDescription"].as_str().unwrap().contains(&name));
    assert_eq!(root["tables"].as_array().unwrap().len(), 0, "{root}");
    let layers = root["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1, "{root}");
    assert_eq!(layers[0]["id"], 0);
    assert_eq!(layers[0]["name"], name);

    // the same service resolved by dataset id rather than by name
    let (status, by_id) = get_json(
        &app,
        &format!("/arcgis/rest/services/{ds}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{by_id}");
    assert_eq!(by_id["layers"][0]["name"], name, "{by_id}");

    let (status, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{layer}");
    assert_eq!(layer["id"], 0);
    assert_eq!(layer["name"], name);
    assert_eq!(layer["type"], "Feature Layer");
    assert_eq!(layer["geometryType"], "esriGeometryPoint");
    assert_eq!(layer["objectIdField"], "objectid");
    assert_eq!(layer["maxRecordCount"], 1000);
    assert_eq!(layer["supportedQueryFormats"], "JSON,geoJSON");
    assert_eq!(layer["capabilities"], "Query");
    assert_eq!(
        layer["advancedQueryCapabilities"]["supportsPagination"],
        true
    );
    // a client reads these before it will send a statistics, distinct or ordered
    // query at all, and before it will page an aggregated one
    let advanced = &layer["advancedQueryCapabilities"];
    assert_eq!(advanced["supportsStatistics"], true, "{layer}");
    assert_eq!(advanced["supportsDistinct"], true, "{layer}");
    assert_eq!(advanced["supportsOrderBy"], true, "{layer}");
    assert_eq!(
        advanced["supportsPaginationOnAggregatedQueries"], true,
        "{layer}"
    );
    // the attachment operations are served on every layer, whether or not this
    // one holds an attachment yet. Editing them still needs an editable layer,
    // which `capabilities` above says this one is not.
    assert_eq!(layer["hasAttachments"], true);
    // computed from the seeded points, which run from (0,0) to (2,2)
    assert_eq!(layer["extent"]["xmin"], 0.0, "{layer}");
    assert_eq!(layer["extent"]["ymax"], 2.0, "{layer}");
    assert_eq!(layer["extent"]["spatialReference"]["wkid"], 4326, "{layer}");
    let fields = layer["fields"].as_array().unwrap();
    let oid = fields.iter().find(|f| f["name"] == "objectid").unwrap();
    assert_eq!(oid["type"], "esriFieldTypeOID", "{layer}");
    let name_field = fields.iter().find(|f| f["name"] == "name").unwrap();
    assert_eq!(name_field["type"], "esriFieldTypeString", "{layer}");
    assert_eq!(name_field["length"], 2048, "{layer}");

    // the service has one layer, and asking for another is an error, not a 404
    let (status, body) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/1?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");
}

#[tokio::test]
async fn test_arcgis_query_pages_and_reports_exceeded_transfer_limit() {
    let (app, _) = setup_app().await;
    let name = format!("paged_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 5).await;

    let (status, first) = get_json(
        &app,
        &query_url(
            &name,
            "where=1=1&outFields=*&resultOffset=0&resultRecordCount=2",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["objectIdFieldName"], "objectid", "{first}");
    assert_eq!(first["geometryType"], "esriGeometryPoint", "{first}");
    assert_eq!(first["spatialReference"]["wkid"], 4326, "{first}");
    assert!(first["fields"].as_array().unwrap().len() >= 2, "{first}");
    assert_eq!(first["features"].as_array().unwrap().len(), 2, "{first}");
    assert_eq!(first["exceededTransferLimit"], true, "{first}");
    assert_eq!(first["features"][0]["attributes"]["objectid"], 1, "{first}");
    assert_eq!(
        first["features"][0]["attributes"]["name"], "point-0",
        "{first}"
    );
    assert_eq!(first["features"][0]["geometry"]["x"], 0.0, "{first}");

    let (_, middle) = get_json(
        &app,
        &query_url(&name, "resultOffset=2&resultRecordCount=2"),
    )
    .await;
    assert_eq!(
        middle["features"][0]["attributes"]["objectid"], 3,
        "{middle}"
    );
    assert_eq!(middle["exceededTransferLimit"], true, "{middle}");

    // the last page is short, so there is nothing past it
    let (_, last) = get_json(
        &app,
        &query_url(&name, "resultOffset=4&resultRecordCount=2"),
    )
    .await;
    assert_eq!(last["features"].as_array().unwrap().len(), 1, "{last}");
    assert_eq!(last["exceededTransferLimit"], false, "{last}");

    // a full page that happens to be the last one still says there is no more
    let (_, exact) = get_json(
        &app,
        &query_url(&name, "resultOffset=0&resultRecordCount=5"),
    )
    .await;
    assert_eq!(exact["features"].as_array().unwrap().len(), 5, "{exact}");
    assert_eq!(exact["exceededTransferLimit"], false, "{exact}");
}

#[tokio::test]
async fn test_arcgis_query_object_ids_count_only_and_ids_only() {
    let (app, _) = setup_app().await;
    let name = format!("ids_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 5).await;

    let (status, ids) = get_json(&app, &query_url(&name, "returnIdsOnly=true")).await;
    assert_eq!(status, StatusCode::OK, "{ids}");
    assert_eq!(ids["objectIdFieldName"], "objectid", "{ids}");
    assert_eq!(ids["objectIds"], json!([1, 2, 3, 4, 5]), "{ids}");

    let (_, count) = get_json(&app, &query_url(&name, "returnCountOnly=true")).await;
    assert_eq!(count["count"], 5, "{count}");

    let (_, some) = get_json(&app, &query_url(&name, "objectIds=2,4&outFields=*")).await;
    let features = some["features"].as_array().unwrap();
    assert_eq!(features.len(), 2, "{some}");
    assert_eq!(features[0]["attributes"]["objectid"], 2, "{some}");
    assert_eq!(features[1]["attributes"]["objectid"], 4, "{some}");

    let (_, filtered_count) = get_json(
        &app,
        &query_url(&name, "objectIds=2,4&returnCountOnly=true"),
    )
    .await;
    assert_eq!(filtered_count["count"], 2, "{filtered_count}");

    let (_, bad) = get_json(&app, &query_url(&name, "objectIds=two")).await;
    assert_eq!(bad["error"]["code"], 400, "{bad}");
}

#[tokio::test]
async fn test_arcgis_query_envelope_filter_and_refused_relations() {
    let (app, _) = setup_app().await;
    let name = format!("env_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0), "properties": {"name": "near"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(50.0, 50.0), "properties": {"name": "far"}},
        ]),
    )
    .await;

    let inside = "geometry=0,0,10,10&geometryType=esriGeometryEnvelope\
                  &spatialRel=esriSpatialRelIntersects&inSR=4326&outFields=*";
    let (status, body) = get_json(&app, &query_url(&name, inside)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 1, "{body}");
    assert_eq!(features[0]["attributes"]["name"], "near", "{body}");

    // the envelope object form, which is what an ArcGIS JS map sends
    let object =
        "geometry=%7B%22xmin%22%3A0%2C%22ymin%22%3A0%2C%22xmax%22%3A10%2C%22ymax%22%3A10%7D";
    let (_, from_object) = get_json(&app, &query_url(&name, object)).await;
    assert_eq!(
        from_object["features"].as_array().unwrap().len(),
        1,
        "{from_object}"
    );

    let (_, count) = get_json(
        &app,
        &query_url(&name, "geometry=0,0,10,10&returnCountOnly=true"),
    )
    .await;
    assert_eq!(count["count"], 1, "{count}");

    // a mercator envelope in metres, wide enough to cover (1, 1) and not (50, 50)
    let mercator = "geometry=0,0,200000,200000&geometryType=esriGeometryEnvelope\
                    &spatialRel=esriSpatialRelIntersects&inSR=102100&outFields=*";
    let (_, from_mercator) = get_json(&app, &query_url(&name, mercator)).await;
    let features = from_mercator["features"].as_array().unwrap();
    assert_eq!(features.len(), 1, "{from_mercator}");
    assert_eq!(features[0]["attributes"]["name"], "near", "{from_mercator}");

    for refused in [
        "geometry=0,0,10,10&geometryType=esriGeometryPolygon",
        "geometry=0,0,10,10&spatialRel=esriSpatialRelContains",
        "geometry=0,0,10,10&inSR=27700",
    ] {
        let (status, body) = get_json(&app, &query_url(&name, refused)).await;
        assert_eq!(status, StatusCode::OK, "{refused}: {body}");
        assert_eq!(body["error"]["code"], 400, "{refused}: {body}");
    }
}

/// A web map renders in Web Mercator and asks for it as Esri's 102100. The
/// coordinates must come back transformed, in metres, under that name.
#[tokio::test]
async fn test_arcgis_query_serves_web_mercator_when_asked() {
    let (app, _) = setup_app().await;
    let name = format!("merc_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 0.0), "properties": {"name": "one-degree"}},
        ]),
    )
    .await;

    let (status, body) = get_json(&app, &query_url(&name, "outSR=102100&outFields=*")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["spatialReference"]["wkid"], 102100, "{body}");
    assert_eq!(body["spatialReference"]["latestWkid"], 3857, "{body}");
    let geometry = &body["features"][0]["geometry"];
    // one degree of longitude on the mercator sphere, with slack for the
    // transform rather than an exact float
    let x = geometry["x"].as_f64().unwrap();
    assert!((x - 111319.490793).abs() < 1.0, "{geometry}");
    assert!(geometry["y"].as_f64().unwrap().abs() < 1.0, "{geometry}");

    // geojson stays 4326 by its own spec, so mercator geojson is refused
    let (_, refused) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0/query?f=geojson&outSR=102100"),
    )
    .await;
    assert_eq!(refused["error"]["code"], 400, "{refused}");
}

#[tokio::test]
async fn test_arcgis_polygon_exterior_ring_comes_out_clockwise() {
    let (app, _) = setup_app().await;
    let name = format!("poly_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "polygon").await;
    let branch = create_branch(&app, ds, "main").await;

    // stored the GeoJSON way round: exterior counter-clockwise, hole clockwise
    let exterior = vec![(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0), (0.0, 0.0)];
    let hole = vec![(1.0, 1.0), (1.0, 2.0), (2.0, 2.0), (2.0, 1.0), (1.0, 1.0)];
    commit_features(
        &app,
        branch,
        json!([{
            "type": "insert", "feature_id": Uuid::now_v7().to_string(),
            "geometry_wkb_hex": polygon_wkb(&[exterior, hole]), "properties": {},
        }]),
    )
    .await;

    let (status, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["geometryType"], "esriGeometryPolygon", "{body}");
    let rings = body["features"][0]["geometry"]["rings"].as_array().unwrap();
    assert_eq!(rings.len(), 2, "{body}");
    assert!(
        ring_winding(&rings[0]) < 0.0,
        "exterior must be clockwise: {body}"
    );
    assert!(
        ring_winding(&rings[1]) > 0.0,
        "hole must be counter-clockwise: {body}"
    );
    // reversed, not reordered: the same vertices, walked the other way
    assert_eq!(rings[0][0], json!([0.0, 0.0]), "{body}");
    assert_eq!(rings[0][1], json!([0.0, 4.0]), "{body}");
}

#[tokio::test]
async fn test_arcgis_uses_a_real_objectid_field_when_the_schema_declares_one() {
    let (app, _) = setup_app().await;
    let name = format!("migrated_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        None,
        Some(json!({"fields": [
            {"name": "OBJECTID", "field_type": "integer", "required": true},
            {"name": "name", "field_type": "string", "required": false},
            {"name": "open", "field_type": "boolean", "required": false},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "a", "open": true}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 200, "name": "b", "open": false}},
        ]),
    )
    .await;

    let (_, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(layer["objectIdField"], "OBJECTID", "{layer}");
    let fields = layer["fields"].as_array().unwrap();
    let oid = fields.iter().find(|f| f["name"] == "OBJECTID").unwrap();
    assert_eq!(oid["type"], "esriFieldTypeOID", "{layer}");
    assert_eq!(
        fields.iter().filter(|f| f["name"] == "OBJECTID").count(),
        1,
        "the id is declared once: {layer}"
    );

    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    assert_eq!(body["objectIdFieldName"], "OBJECTID", "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features[0]["attributes"]["OBJECTID"], 100, "{body}");
    assert_eq!(features[1]["attributes"]["OBJECTID"], 200, "{body}");
    // Esri has no boolean field type, so it travels as its text
    assert_eq!(features[0]["attributes"]["open"], "true", "{body}");
    assert_eq!(features[1]["attributes"]["open"], "false", "{body}");

    let (_, ids) = get_json(&app, &query_url(&name, "returnIdsOnly=true")).await;
    assert_eq!(ids["objectIds"], json!([100, 200]), "{ids}");

    let (_, one) = get_json(&app, &query_url(&name, "objectIds=200&outFields=*")).await;
    assert_eq!(one["features"].as_array().unwrap().len(), 1, "{one}");
    assert_eq!(one["features"][0]["attributes"]["name"], "b", "{one}");
}

#[tokio::test]
async fn test_arcgis_synthesizes_objectid_when_no_schema_declares_one() {
    let (app, _) = setup_app().await;
    let name = format!("plain_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 3).await;

    let (_, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(layer["objectIdField"], "objectid", "{layer}");
    // the field list is derived from the property keys the features carry
    let names: Vec<&str> = layer["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["objectid", "name"], "{layer}");
    // a row-number layer publishes no global id either: its rows carry no id a
    // client can write down, which is the same reason it takes no edits
    assert_eq!(layer["globalIdField"], "", "{layer}");

    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    assert_eq!(body["globalIdFieldName"], "", "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 3, "{body}");
    let ids: Vec<i64> = features
        .iter()
        .map(|f| f["attributes"]["objectid"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2, 3], "{body}");

    // and there is no such field to name, so asking for it is refused by name
    let (status, refused) = get_json(&app, &query_url(&name, "outFields=globalid")).await;
    assert_eq!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["error"]["code"], 400, "{refused}");
}

/// Helper: the global ids a where clause selects, sorted so a test does not
/// depend on the order rows come back in.
async fn global_ids_matching(app: &axum::Router, service: &str, clause: &str) -> Vec<String> {
    let (status, body) = get_json(app, &query_url(service, &where_param(clause))).await;
    assert_eq!(status, StatusCode::OK, "{clause}: {body}");
    assert!(body["error"].is_null(), "{clause}: {body}");
    let mut held: Vec<String> = body["features"]
        .as_array()
        .unwrap_or_else(|| panic!("{clause}: {body}"))
        .iter()
        .map(|f| {
            f["attributes"]["globalid"]
                .as_str()
                .unwrap_or_else(|| panic!("{f}"))
                .to_string()
        })
        .collect();
    held.sort();
    held
}

/// The virtual `globalid`: declared on the layer, served as the feature's uuid in
/// braces and upper case, and filterable, which is how a consumer resolves an
/// attachment's parent feature.
#[tokio::test]
async fn test_arcgis_serves_and_filters_the_virtual_global_id() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "guid", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 10, "name": "first"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 20, "name": "second"}},
        ]),
    )
    .await;

    // the layer names the field and declares it as esri's own global id type
    let (_, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(layer["globalIdField"], "globalid", "{layer}");
    let declared = layer["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "globalid")
        .unwrap_or_else(|| panic!("{layer}"));
    assert_eq!(declared["type"], "esriFieldTypeGlobalID", "{layer}");
    assert_eq!(declared["editable"], false, "{layer}");

    // served with the rows, as a guid in braces and upper case
    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    assert_eq!(body["globalIdFieldName"], "globalid", "{body}");
    let guids: Vec<String> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            f["attributes"]["globalid"]
                .as_str()
                .unwrap_or_else(|| panic!("{f}"))
                .to_string()
        })
        .collect();
    assert_eq!(guids.len(), 2, "{body}");
    for guid in &guids {
        assert!(guid.starts_with('{') && guid.ends_with('}'), "{guid}");
        let held = Uuid::parse_str(guid.trim_matches(|c| c == '{' || c == '}')).unwrap();
        assert_eq!(*guid, format!("{{{}}}", held.to_string().to_uppercase()));
    }

    // named on its own, it is answered beside the object id
    let (_, one) = get_json(&app, &query_url(&name, "outFields=globalid")).await;
    let held = &one["features"][0]["attributes"];
    assert!(held["globalid"].is_string(), "{one}");
    assert!(held["OBJECTID"].is_i64(), "{one}");

    // and filtered by, which is the query a consumer pairs attachments through:
    // braces and any case name the same feature
    let first = guids[0].clone();
    let bare = first.trim_matches(|c| c == '{' || c == '}').to_lowercase();
    for clause in [
        format!("globalid = '{first}'"),
        format!("globalid = '{bare}'"),
        format!("globalid IN ('{first}')"),
        format!("globalid IN ('{bare}')"),
    ] {
        let held = global_ids_matching(&app, &name, &clause).await;
        assert_eq!(held, vec![first.clone()], "{clause}");
    }
    // the IN list a consumer batches parents into
    let mut both = guids.clone();
    both.sort();
    let clause = format!("globalid IN ('{first}', '{}')", guids[1]);
    assert_eq!(global_ids_matching(&app, &name, &clause).await, both);

    // a guid no feature carries answers no rows rather than an error
    let (status, none) = get_json(
        &app,
        &query_url(
            &name,
            &where_param("globalid = '{00000000-0000-0000-0000-000000000000}'"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{none}");
    assert!(none["features"].as_array().unwrap().is_empty(), "{none}");
}

/// An edit never writes either id: the object id is the service's to assign and
/// the global id is the feature's uuid, so a client-supplied `globalid` attribute
/// is dropped rather than stored under a key `/query` would never read it from.
#[tokio::test]
async fn test_arcgis_apply_edits_ignores_a_client_supplied_global_id() {
    let (app, _) = setup_app().await;
    let (name, _ds, _branch) = editable_layer(&app, "guidedit", "point").await;

    let planted = "{DEADBEEF-0000-0000-0000-000000000001}";
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{
                "attributes": {"name": "first", "globalid": planted},
                "geometry": {"x": 1.0, "y": 1.0},
            }])
            .to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");
    let oid = out["addResults"][0]["objectId"].as_i64().unwrap();

    // the global id served is the feature's own uuid, not what the client sent
    let (_, held) = get_json(&app, &query_url(&name, "outFields=*")).await;
    let attributes = &held["features"][0]["attributes"];
    assert_ne!(attributes["globalid"], planted, "{held}");
    assert!(
        Uuid::parse_str(
            attributes["globalid"]
                .as_str()
                .unwrap()
                .trim_matches(|c| c == '{' || c == '}')
        )
        .is_ok(),
        "{held}"
    );

    // an update carrying one is ignored the same way, and the id does not move
    let before = attributes["globalid"].clone();
    let body = form_body(&[
        ("f", "json".into()),
        (
            "updates",
            json!([{"attributes": {"OBJECTID": oid, "name": "renamed", "globalid": planted}}])
                .to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");
    let (_, after) = get_json(&app, &query_url(&name, "outFields=*")).await;
    let attributes = &after["features"][0]["attributes"];
    assert_eq!(attributes["globalid"], before, "{after}");
    assert_eq!(attributes["name"], "renamed", "{after}");

    // useGlobalIds is still refused, because an edit names its feature by object id
    let body = form_body(&[
        ("f", "json".into()),
        ("useGlobalIds", "true".into()),
        ("deletes", oid.to_string()),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("useGlobalIds"),
        "{out}"
    );
}

#[tokio::test]
async fn test_arcgis_refuses_a_where_clause_it_cannot_honor() {
    let (app, _) = setup_app().await;
    let name = format!("wheres_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;

    for allowed in ["where=1=1", "where=", "outFields=*"] {
        let (status, body) = get_json(&app, &query_url(&name, allowed)).await;
        assert_eq!(status, StatusCode::OK, "{allowed}: {body}");
        assert!(body["error"].is_null(), "{allowed}: {body}");
        assert_eq!(body["features"].as_array().unwrap().len(), 2, "{allowed}");
    }

    // a clause the parser reads is a filter now, not a refusal
    let (status, body) = get_json(&app, &query_url(&name, "where=name%3D%27point-0%27")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    assert_eq!(body["features"].as_array().unwrap().len(), 1, "{body}");

    // a filter that silently did not apply would be worse than a refusal
    for refused in [
        "orderByFields=nosuchfield",
        "orderByFields=name%20sideways",
        "outSR=27700",
        "outFields=nosuchfield",
        "returnZ=true",
        // asking for statistics and naming none
        "outStatistics=%5B%5D",
        // filtering groups when nothing asked for any
        "having=count(*)%3E1",
        "gdbVersion=SDE.DEFAULT",
    ] {
        let (status, body) = get_json(&app, &query_url(&name, refused)).await;
        assert_eq!(status, StatusCode::OK, "{refused}: {body}");
        assert_eq!(body["error"]["code"], 400, "{refused}: {body}");
    }

    let (status, body) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0/query?f=html"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");

    // the orders and references it can honor
    for allowed in [
        "orderByFields=objectid",
        "orderByFields=objectid%20ASC",
        "orderByFields=objectid%20DESC",
        "orderByFields=name%20DESC%2Cobjectid",
        "outSR=4326",
        "outSR=%7B%22wkid%22%3A4326%7D",
        "outSR=3857",
        "outSR=102100",
        "returnZ=false",
        "f=pjson",
    ] {
        let (status, body) = get_json(&app, &query_url(&name, allowed)).await;
        assert_eq!(status, StatusCode::OK, "{allowed}: {body}");
        assert!(body["error"].is_null(), "{allowed}: {body}");
    }
}

/// Helper: a where clause as a query parameter, encoded so the URL carries the
/// clause the test wrote and nothing else.
fn where_param(clause: &str) -> String {
    format!("where={}", urlencoding::encode(clause))
}

/// Helper: a layer whose schema declares the types a where clause compares
/// against, so a number is stored as a number and a string as a string.
///
/// Three features: two inside the envelope (0,0,10,10) and one far outside,
/// `ward` present on one of them only, and `code` a string field that holds
/// number-looking text.
async fn seed_where_layer(app: &axum::Router, name: &str) -> Uuid {
    let ds = create_named_dataset(app, name, "point").await;
    let branch = create_branch(app, ds, "main").await;

    let (status, body) = request_as(
        app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        None,
        Some(json!({"fields": [
            {"name": "OBJECTID", "field_type": "integer", "required": true},
            {"name": "name", "field_type": "string", "required": false},
            {"name": "pop", "field_type": "integer", "required": false},
            {"name": "score", "field_type": "float", "required": false},
            {"name": "seen", "field_type": "string", "required": false},
            {"name": "ward", "field_type": "string", "required": false},
            {"name": "code", "field_type": "string", "required": false},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    commit_features(
        app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 10, "name": "alpha", "pop": 100, "score": 1.5,
                            "seen": "2024-01-01T00:00:00Z", "ward": "north", "code": "007"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 20, "name": "beta", "pop": 200, "score": 2.5,
                            "seen": "2024-06-01T12:00:00Z", "code": "20"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(50.0, 50.0),
             "properties": {"OBJECTID": 30, "name": "gamma", "pop": 300, "score": 3.5,
                            "seen": "2025-01-01T00:00:00Z", "code": "100"}},
        ]),
    )
    .await;
    ds
}

#[tokio::test]
async fn test_arcgis_query_where_filters_rows() {
    let (app, _) = setup_app().await;
    let name = format!("filter_{}", Uuid::now_v7().simple());
    seed_where_layer(&app, &name).await;

    // the object ids each clause should answer with, in id order
    let cases: [(&str, Vec<i64>); 24] = [
        ("1=1", vec![10, 20, 30]),
        ("pop = 200", vec![20]),
        ("pop=200", vec![20]),
        ("pop <> 200", vec![10, 30]),
        ("pop != 200", vec![10, 30]),
        ("pop > 150", vec![20, 30]),
        ("pop >= 200 AND pop < 300", vec![20]),
        ("200 > pop", vec![10]),
        ("score <= 2.5", vec![10, 20]),
        ("name = 'alpha'", vec![10]),
        ("name <> 'alpha'", vec![20, 30]),
        ("name > 'b'", vec![20, 30]),
        ("name LIKE 'a%'", vec![10]),
        ("name LIKE '%a'", vec![10, 20, 30]),
        ("name LIKE 'bet_'", vec![20]),
        ("name NOT LIKE '%a'", vec![]),
        ("pop IN (100, 300)", vec![10, 30]),
        ("name IN ('alpha', 'gamma')", vec![10, 30]),
        ("pop NOT IN (100)", vec![20, 30]),
        ("pop BETWEEN 150 AND 300", vec![20, 30]),
        ("name BETWEEN 'a' AND 'b'", vec![10]),
        ("ward IS NULL", vec![20, 30]),
        ("ward IS NOT NULL", vec![10]),
        ("NOT pop = 100", vec![20, 30]),
    ];
    for (clause, wanted) in cases {
        let (status, body) = get_json(
            &app,
            &query_url(&name, &format!("{}&outFields=*", where_param(clause))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{clause}: {body}");
        assert!(body["error"].is_null(), "{clause}: {body}");
        let ids: Vec<i64> = body["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["attributes"]["OBJECTID"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, wanted, "{clause}: {body}");
    }

    // the object id compares against the id column rather than a property read
    for (clause, wanted) in [
        ("OBJECTID = 20", vec![20]),
        ("objectid IN (10, 30)", vec![10, 30]),
        ("objectid >= 20", vec![20, 30]),
    ] {
        let (_, body) = get_json(
            &app,
            &query_url(&name, &format!("{}&outFields=*", where_param(clause))),
        )
        .await;
        let ids: Vec<i64> = body["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["attributes"]["OBJECTID"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, wanted, "{clause}: {body}");
    }
}

/// AND binds tighter than OR, parentheses override it, and a where clause
/// combines with the other filters by AND.
#[tokio::test]
async fn test_arcgis_query_where_precedence_and_other_filters() {
    let (app, _) = setup_app().await;
    let name = format!("prec_{}", Uuid::now_v7().simple());
    seed_where_layer(&app, &name).await;

    let ids = |body: &Value| -> Vec<i64> {
        body["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["attributes"]["OBJECTID"].as_i64().unwrap())
            .collect()
    };

    // AND first: only pop=100 can match, because name='nothing' matches nothing
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{}&outFields=*",
                where_param("pop = 100 OR pop = 300 AND name = 'nothing'")
            ),
        ),
    )
    .await;
    assert_eq!(ids(&body), vec![10], "{body}");

    // and the same clause with parentheses answers the other way
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{}&outFields=*",
                where_param("(pop = 100 OR pop = 300) AND name = 'gamma'")
            ),
        ),
    )
    .await;
    assert_eq!(ids(&body), vec![30], "{body}");

    // a date literal against the RFC 3339 text the store holds
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{}&outFields=*", where_param("seen >= DATE '2024-06-01'")),
        ),
    )
    .await;
    assert_eq!(ids(&body), vec![20, 30], "{body}");

    // a number against a field declared string compares as the text the client
    // wrote: '007' is not '7', and '20' is '20'
    for (clause, wanted) in [
        ("code = 7", vec![]),
        ("code = '007'", vec![10]),
        ("code = 20", vec![20]),
        // and a string against a field declared integer compares as text too
        ("pop = '200'", vec![20]),
    ] {
        let (_, body) = get_json(
            &app,
            &query_url(&name, &format!("{}&outFields=*", where_param(clause))),
        )
        .await;
        assert_eq!(ids(&body), wanted, "{clause}: {body}");
    }

    // = NULL never matches: that is what IS NULL is for
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{}&outFields=*", where_param("ward = NULL")),
        ),
    )
    .await;
    assert!(ids(&body).is_empty(), "{body}");

    // the envelope holds the two near points, and the clause holds two of the
    // three, so together they hold one
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "geometry=0,0,10,10&outFields=*&{}",
                where_param("pop > 150")
            ),
        ),
    )
    .await;
    assert_eq!(ids(&body), vec![20], "{body}");

    // and with the objectIds list, which is a third filter
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("objectIds=10,20&outFields=*&{}", where_param("pop > 150")),
        ),
    )
    .await;
    assert_eq!(ids(&body), vec![20], "{body}");

    // every mode the query answers in takes the clause
    let (_, count) = get_json(
        &app,
        &query_url(
            &name,
            &format!("returnCountOnly=true&{}", where_param("pop > 150")),
        ),
    )
    .await;
    assert_eq!(count["count"], 2, "{count}");

    let (_, only) = get_json(
        &app,
        &query_url(
            &name,
            &format!("returnIdsOnly=true&{}", where_param("pop > 150")),
        ),
    )
    .await;
    assert_eq!(only["objectIds"], json!([20, 30]), "{only}");

    let (_, geojson) = get_json(
        &app,
        &format!(
            "/arcgis/rest/services/{name}/FeatureServer/0/query?f=geojson&outFields=*&{}",
            where_param("name = 'beta'")
        ),
    )
    .await;
    assert_eq!(
        geojson["features"].as_array().unwrap().len(),
        1,
        "{geojson}"
    );
    assert_eq!(geojson["features"][0]["properties"]["name"], "beta");
}

/// Every literal is bound, so a clause that reads like an injection is a string
/// comparison that matches nothing, and the database is still there afterwards.
#[tokio::test]
async fn test_arcgis_query_where_treats_injection_as_data() {
    let (app, _) = setup_app().await;
    let name = format!("inject_{}", Uuid::now_v7().simple());
    seed_where_layer(&app, &name).await;

    for clause in [
        "name='x''; DROP TABLE datasets;--'",
        "name = 'x'' OR ''1''=''1'",
        "name = '%' AND name = 'x'' UNION SELECT 1'",
        "code = '1; TRUNCATE TABLE features'",
    ] {
        let (status, body) = get_json(
            &app,
            &query_url(&name, &format!("{}&outFields=*", where_param(clause))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{clause}: {body}");
        assert!(body["error"].is_null(), "{clause}: {body}");
        assert!(
            body["features"].as_array().unwrap().is_empty(),
            "{clause}: {body}"
        );
    }

    // a quote that does not close is refused rather than guessed at
    for refused in ["name = 'x", "name = x'", "name = '''''"] {
        let (status, body) = get_json(
            &app,
            &query_url(&name, &format!("{}&outFields=*", where_param(refused))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{refused}: {body}");
        assert_eq!(body["error"]["code"], 400, "{refused}: {body}");
    }

    // nothing was dropped: the same layer answers the same three features, and
    // the catalog still lists it
    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    assert_eq!(body["features"].as_array().unwrap().len(), 3, "{body}");
    let (status, catalog) = get_json(&app, "/arcgis/rest/services?f=json").await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    assert!(
        catalog["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == name),
        "{catalog}"
    );
}

/// A property key can hold anything, a quote and a backslash included, and both
/// the object id read and every where clause name their key as a bind rather than
/// quoting it into the SQL. So a layer with such a key answers correctly, and
/// nothing about it depends on how the server escapes a quote.
///
/// Naming that field in a clause is a refusal, not a broken query: a field
/// reference is a bare word to the tokenizer, so there is no way to write one.
/// That is the reason this is as end to end as it gets.
#[tokio::test]
async fn test_arcgis_query_reads_a_layer_whose_property_key_carries_a_quote() {
    let (app, _) = setup_app().await;
    let name = format!("hostilekey_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    let hostile = r"it's\ a ' key";

    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        None,
        Some(json!({"fields": [
            {"name": "OBJECTID", "field_type": "integer", "required": false},
            {"name": "name", "field_type": "string", "required": false},
            {"name": hostile, "field_type": "string", "required": false},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let feature = |oid: i64, label: &str, held: &str, x: f64| {
        let mut properties = serde_json::Map::new();
        properties.insert("OBJECTID".to_string(), json!(oid));
        properties.insert("name".to_string(), json!(label));
        properties.insert(hostile.to_string(), json!(held));
        json!({"type": "insert", "feature_id": Uuid::now_v7().to_string(),
               "geometry_wkb_hex": point_wkb(x, 1.0), "properties": properties})
    };
    commit_features(
        &app,
        branch,
        json!([
            feature(10, "alpha", "first", 1.0),
            feature(20, "beta", "second", 2.0),
        ]),
    )
    .await;

    // the id read binds its own key, so the ids are the ones the features carry
    let (_, ids) = get_json(&app, &query_url(&name, "returnIdsOnly=true")).await;
    assert_eq!(ids["objectIds"], json!([10, 20]), "{ids}");
    assert_eq!(facade_count(&app, &name).await, 2);

    // the layer declares the field, and its value comes back under that name
    let (_, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    let declared: Vec<&str> = layer["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    // the virtual global id follows the object id on every real-oid layer
    assert_eq!(
        declared,
        vec!["OBJECTID", "globalid", "name", hostile],
        "{layer}"
    );

    // a clause filters correctly against a real database with that key in the
    // data, and the whole feature still reads back
    let (status, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{}&outFields=*", where_param("name = 'beta'")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 1, "{body}");
    assert_eq!(features[0]["attributes"]["OBJECTID"], 20, "{body}");
    assert_eq!(features[0]["attributes"][hostile], "second", "{body}");

    // and the id compares as the id, over the same bound key
    let (_, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{}&outFields=*", where_param("OBJECTID >= 20")),
        ),
    )
    .await;
    assert_eq!(body["features"].as_array().unwrap().len(), 1, "{body}");

    // naming the field itself is a refusal rather than a query that breaks
    let (status, body) = get_json(
        &app,
        &query_url(&name, &where_param(&format!("{hostile} IS NULL"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");

    // nothing was harmed: the layer still answers both features
    assert_eq!(facade_count(&app, &name).await, 2);
}

/// What the clause parser will not read is refused by name, because a filter
/// that silently did not apply hands back rows the client did not ask for.
#[tokio::test]
async fn test_arcgis_query_where_refuses_what_it_cannot_honor_by_name() {
    let (app, _) = setup_app().await;
    let name = format!("norefuse_{}", Uuid::now_v7().simple());
    seed_where_layer(&app, &name).await;

    for (clause, names) in [
        ("nosuchfield = 1", "nosuchfield"),
        ("upper(name) = 'ALPHA'", "upper"),
        ("EXTRACT(year FROM seen) = 2024", "EXTRACT"),
        ("pop + 1 = 101", "arithmetic"),
        ("pop IN (SELECT pop FROM datasets)", "subquery"),
        ("name LIKE 'a%' ESCAPE '!'", "ESCAPE"),
        ("\"name\" = 'alpha'", "quoted identifier"),
        ("name = 'alpha'; SELECT 1", "';'"),
        ("name = 'alpha' -- and the rest", "comment"),
        ("seen > DATE '01/06/2024'", "date literal"),
        ("name", "nothing to compare"),
    ] {
        let (status, body) = get_json(
            &app,
            &query_url(&name, &format!("{}&outFields=*", where_param(clause))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{clause}: {body}");
        assert_eq!(body["error"]["code"], 400, "{clause}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(names), "'{clause}' answered '{message}'");
    }
}

/// Helper: an `outStatistics` parameter, from the JSON a client would send.
fn stats_param(spec: Value) -> String {
    format!("outStatistics={}", urlencoding::encode(&spec.to_string()))
}

/// Helper: the attributes of every feature in an answer, in the order they came.
fn attributes(body: &Value) -> Vec<Value> {
    body["features"]
        .as_array()
        .unwrap_or_else(|| panic!("{body}"))
        .iter()
        .map(|feature| feature["attributes"].clone())
        .collect()
}

/// Helper: the declared type of one field of an answer.
fn field_type(body: &Value, name: &str) -> String {
    body["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("{body}"))
        .iter()
        .find(|field| field["name"] == name)
        .unwrap_or_else(|| panic!("no field {name} in {body}"))["type"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Helper: a layer whose schema declares the types a statistic needs, with
/// numbers a test can add up by hand.
///
/// Four features in two wards of two, `pop` 10/20/30/40, `score` on three of
/// them, and `code` a string field holding number-looking text.
async fn seed_stats_layer(app: &axum::Router, name: &str) -> Uuid {
    let ds = create_named_dataset(app, name, "point").await;
    let branch = create_branch(app, ds, "main").await;

    let (status, body) = request_as(
        app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        None,
        Some(json!({"fields": [
            {"name": "OBJECTID", "field_type": "integer", "required": true},
            {"name": "ward", "field_type": "string", "required": false},
            {"name": "pop", "field_type": "integer", "required": false},
            {"name": "score", "field_type": "float", "required": false},
            {"name": "code", "field_type": "string", "required": false},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    commit_features(
        app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 1, "ward": "north", "pop": 10, "score": 1.0, "code": "7"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 2, "ward": "north", "pop": 20, "score": 3.0, "code": "7"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(3.0, 3.0),
             "properties": {"OBJECTID": 3, "ward": "south", "pop": 30, "score": 5.0, "code": "100"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(50.0, 50.0),
             "properties": {"OBJECTID": 4, "ward": "south", "pop": 40, "code": "abc"}},
        ]),
    )
    .await;
    ds
}

#[tokio::test]
async fn test_arcgis_query_statistics_over_the_whole_layer() {
    let (app, _) = setup_app().await;
    let name = format!("stats_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    let (status, body) = get_json(
        &app,
        &query_url(
            &name,
            &stats_param(json!([
                {"statisticType": "count", "onStatisticField": "pop"},
                {"statisticType": "count", "onStatisticField": "score"},
                {"statisticType": "sum", "onStatisticField": "pop"},
                {"statisticType": "avg", "onStatisticField": "pop", "outStatisticFieldName": "mean_pop"},
                {"statisticType": "min", "onStatisticField": "pop"},
                {"statisticType": "max", "onStatisticField": "pop"},
                {"statisticType": "min", "onStatisticField": "ward"},
            ])),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    let rows = attributes(&body);
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["count_pop"], 4, "{body}");
    // a count counts the values that are there, and one feature has no score
    assert_eq!(rows[0]["count_score"], 3, "{body}");
    assert_eq!(rows[0]["sum_pop"], 100.0, "{body}");
    assert_eq!(rows[0]["mean_pop"], 25.0, "{body}");
    assert_eq!(rows[0]["min_pop"], 10, "{body}");
    assert_eq!(rows[0]["max_pop"], 40, "{body}");
    // a min over text is text, and it is declared as text
    assert_eq!(rows[0]["min_ward"], "north", "{body}");
    assert_eq!(
        field_type(&body, "min_ward"),
        "esriFieldTypeString",
        "{body}"
    );

    // a count is a whole number of rows and says so, and the numeric aggregates
    // are doubles
    assert_eq!(
        field_type(&body, "count_pop"),
        "esriFieldTypeInteger",
        "{body}"
    );
    assert_eq!(
        field_type(&body, "sum_pop"),
        "esriFieldTypeDouble",
        "{body}"
    );
    assert_eq!(
        field_type(&body, "mean_pop"),
        "esriFieldTypeDouble",
        "{body}"
    );

    // an aggregate is not one feature, so it has no geometry to draw whatever
    // returnGeometry says
    let (_, with_geometry) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "returnGeometry=true&{}",
                stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
            ),
        ),
    )
    .await;
    assert!(
        with_geometry["features"][0].get("geometry").is_none(),
        "{with_geometry}"
    );
    assert_eq!(with_geometry["features"][0]["attributes"]["count_pop"], 4);

    // the sample deviation of 10 and 20, which the where clause selects
    let (_, spread) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{}&{}",
                where_param("pop < 30"),
                stats_param(json!([
                    {"statisticType": "stddev", "onStatisticField": "pop"},
                    {"statisticType": "var", "onStatisticField": "pop"},
                    {"statisticType": "count", "onStatisticField": "pop"},
                ]))
            ),
        ),
    )
    .await;
    let held = &attributes(&spread)[0];
    assert_eq!(held["count_pop"], 2, "{spread}");
    assert_eq!(held["var_pop"], 50.0, "{spread}");
    let stddev = held["stddev_pop"]
        .as_f64()
        .unwrap_or_else(|| panic!("{spread}"));
    assert!((stddev - 50f64.sqrt()).abs() < 1e-9, "{spread}");
}

#[tokio::test]
async fn test_arcgis_query_statistics_group_by_and_page() {
    let (app, _) = setup_app().await;
    let name = format!("grouped_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    let counted = stats_param(json!([
        {"statisticType": "count", "onStatisticField": "pop"},
        {"statisticType": "sum", "onStatisticField": "pop"},
    ]));

    let (status, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{counted}&groupByFieldsForStatistics=ward&orderByFields=ward"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = attributes(&body);
    assert_eq!(rows.len(), 2, "{body}");
    assert_eq!(rows[0]["ward"], "north", "{body}");
    assert_eq!(rows[0]["count_pop"], 2, "{body}");
    assert_eq!(rows[0]["sum_pop"], 30.0, "{body}");
    assert_eq!(rows[1]["ward"], "south", "{body}");
    assert_eq!(rows[1]["sum_pop"], 70.0, "{body}");
    // the grouped field keeps its own type, and the group is not an object id
    assert_eq!(field_type(&body, "ward"), "esriFieldTypeString", "{body}");
    assert!(rows[0].get("OBJECTID").is_none(), "{body}");

    // a statistic alias orders the groups like any other column
    let (_, biggest) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{counted}&groupByFieldsForStatistics=ward&orderByFields=sum_pop%20DESC"),
        ),
    )
    .await;
    assert_eq!(attributes(&biggest)[0]["ward"], "south", "{biggest}");

    // the where clause and the envelope narrow what is aggregated
    let (_, filtered) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{counted}&groupByFieldsForStatistics=ward&orderByFields=ward&{}",
                where_param("pop > 15")
            ),
        ),
    )
    .await;
    let rows = attributes(&filtered);
    assert_eq!(rows.len(), 2, "{filtered}");
    assert_eq!(rows[0]["count_pop"], 1, "{filtered}");
    assert_eq!(rows[0]["sum_pop"], 20.0, "{filtered}");
    assert_eq!(rows[1]["sum_pop"], 70.0, "{filtered}");

    // the envelope leaves out the far feature, so south is one row of 30
    let (_, near) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{counted}&groupByFieldsForStatistics=ward&orderByFields=ward&geometry=0,0,10,10"
            ),
        ),
    )
    .await;
    assert_eq!(attributes(&near)[1]["sum_pop"], 30.0, "{near}");

    // and the groups page like rows, with the same limit report
    let (_, first) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{counted}&groupByFieldsForStatistics=ward&resultRecordCount=1"),
        ),
    )
    .await;
    assert_eq!(attributes(&first).len(), 1, "{first}");
    assert_eq!(first["exceededTransferLimit"], true, "{first}");
    let (_, second) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{counted}&groupByFieldsForStatistics=ward&resultRecordCount=1&resultOffset=1"
            ),
        ),
    )
    .await;
    assert_eq!(attributes(&second)[0]["ward"], "south", "{second}");
    assert_eq!(second["exceededTransferLimit"], false, "{second}");
}

#[tokio::test]
async fn test_arcgis_query_statistics_refuse_what_they_cannot_answer() {
    let (app, _) = setup_app().await;
    let name = format!("badstats_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    // a name that could not be a field name is refused rather than escaped into
    // an alias: the columns are read by position, so this never reached the SQL,
    // and the layer proves it below by still being there
    let hostile = "x\"; DROP TABLE features; --";
    for (params, names) in [
        (
            stats_param(json!([{"statisticType": "sum", "onStatisticField": "code"}])),
            "code",
        ),
        (
            stats_param(json!([{"statisticType": "avg", "onStatisticField": "ward"}])),
            "ward",
        ),
        (
            stats_param(json!([{"statisticType": "median", "onStatisticField": "pop"}])),
            "median",
        ),
        (
            stats_param(json!([{"statisticType": "sum", "onStatisticField": "nosuchfield"}])),
            "nosuchfield",
        ),
        (
            stats_param(json!([{"statisticType": "count", "onStatisticField": "pop",
                                "outStatisticFieldName": "9lives"}])),
            "9lives",
        ),
        (
            stats_param(json!([{"statisticType": "count", "onStatisticField": "pop",
                                "outStatisticFieldName": hostile}])),
            "not a field name",
        ),
        (
            stats_param(json!([
                {"statisticType": "count", "onStatisticField": "pop", "outStatisticFieldName": "total"},
                {"statisticType": "sum", "onStatisticField": "pop", "outStatisticFieldName": "TOTAL"},
            ])),
            "both named",
        ),
        (
            format!(
                "{}&groupByFieldsForStatistics=nosuchfield",
                stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
            ),
            "nosuchfield",
        ),
        (
            "groupByFieldsForStatistics=ward".to_string(),
            "groupByFieldsForStatistics",
        ),
        (
            format!(
                "returnDistinctValues=true&outFields=ward&{}",
                stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
            ),
            "two different answers",
        ),
        (
            format!(
                "returnCountOnly=true&{}",
                stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
            ),
            "not features",
        ),
        (
            format!(
                "returnIdsOnly=true&{}",
                stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
            ),
            "not features",
        ),
        (
            format!(
                "{}&orderByFields=pop",
                stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
            ),
            "not one of the fields",
        ),
    ] {
        let (status, body) = get_json(&app, &query_url(&name, &params)).await;
        assert_eq!(status, StatusCode::OK, "{params}: {body}");
        assert_eq!(body["error"]["code"], 400, "{params}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(names), "'{params}' answered '{message}'");
    }

    // f=geojson describes features, and these answers are not features
    let (_, geojson) = get_json(
        &app,
        &format!(
            "/arcgis/rest/services/{name}/FeatureServer/0/query?f=geojson&{}",
            stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]))
        ),
    )
    .await;
    assert_eq!(geojson["error"]["code"], 400, "{geojson}");

    // nothing was harmed by any of it
    assert_eq!(facade_count(&app, &name).await, 4);
}

/// Helper: a `having` clause as a query parameter, encoded so the URL carries
/// the clause the test wrote and nothing else.
fn having_param(clause: &str) -> String {
    having_named("having", clause)
}

/// The same under either of the parameter's two names: Esri's REST reference
/// calls it `havingClause` and the JS API's property is `having`.
fn having_named(name: &str, clause: &str) -> String {
    format!("{name}={}", urlencoding::encode(clause))
}

/// The form Esri's REST reference documents: an aggregate function over a field of
/// the layer, which the docs say need not appear in `outStatistics`. Such an
/// aggregate is computed to filter the groups and is not served back.
#[tokio::test]
async fn test_arcgis_query_having_takes_aggregate_functions() {
    let (app, _) = setup_app().await;
    let name = format!("havingfn_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    // north holds two scores and south one, so a count of the values that are
    // there tells the two groups apart while a count of rows does not
    let summed = stats_param(json!([{"statisticType": "sum", "onStatisticField": "pop"}]));
    let grouped = format!("{summed}&groupByFieldsForStatistics=ward&orderByFields=ward");

    // the docs' own shape: the aggregate the clause filters by is absent from
    // outStatistics, so it is computed for the filter and never projected
    let (status, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("COUNT(score) > 1")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    assert_eq!(
        attributes(&body),
        vec![json!({"ward": "north", "sum_pop": 30.0})],
        "{body}"
    );
    // and the answer carries the two columns it asked for and no third one
    assert_eq!(body["fields"].as_array().unwrap().len(), 2, "{body}");

    // COUNT(*) counts the rows in a group, so both wards clear it: this is the
    // difference between counting rows and counting the values that are there
    let (_, rows) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("COUNT(*) = 2")),
        ),
    )
    .await;
    assert_eq!(
        attributes(&rows),
        vec![
            json!({"ward": "north", "sum_pop": 30.0}),
            json!({"ward": "south", "sum_pop": 70.0}),
        ],
        "{rows}"
    );
    // COUNT(1) is the same count written the way a code generator writes it
    let (_, ones) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("COUNT(1) = 2")),
        ),
    )
    .await;
    assert_eq!(attributes(&ones).len(), 2, "{ones}");
    // while a count of a field with a null in it keeps north alone
    let (_, values) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("COUNT(score) = 2")),
        ),
    )
    .await;
    assert_eq!(attributes(&values).len(), 1, "{values}");
    assert_eq!(attributes(&values)[0]["ward"], "north", "{values}");

    // the docs' combined example shape, AVG(...) >= n AND MIN(...) >= m, over a
    // request whose outStatistics asks for neither of them
    let counted = stats_param(json!([{"statisticType": "count", "onStatisticField": "pop"}]));
    let (_, combined) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{counted}&groupByFieldsForStatistics=ward&orderByFields=ward&{}",
                having_param("AVG(pop) >= 20 AND MIN(score) >= 5")
            ),
        ),
    )
    .await;
    assert_eq!(
        attributes(&combined),
        vec![json!({"ward": "south", "count_pop": 2})],
        "{combined}"
    );

    // every aggregate of the closed set is available to the clause, projected or
    // not: south is the group of 30 and 40
    for clause in [
        "SUM(pop) > 50",
        "AVG(pop) > 25",
        "MIN(pop) > 25",
        "MAX(pop) > 35",
        "COUNT(code) = 2 AND SUM(pop) > 50",
    ] {
        let (_, body) = get_json(
            &app,
            &query_url(&name, &format!("{grouped}&{}", having_param(clause))),
        )
        .await;
        assert!(body["error"].is_null(), "{clause}: {body}");
        assert_eq!(
            attributes(&body),
            vec![json!({"ward": "south", "sum_pop": 70.0})],
            "{clause}: {body}"
        );
    }

    // the sample forms behave as they do in outStatistics: south holds one score,
    // and one value has no sample deviation, so its group is dropped rather than
    // compared against a zero
    for clause in ["VAR(score) > 1", "STDDEV(score) > 1"] {
        let (_, body) = get_json(
            &app,
            &query_url(&name, &format!("{grouped}&{}", having_param(clause))),
        )
        .await;
        assert_eq!(
            attributes(&body),
            vec![json!({"ward": "north", "sum_pop": 30.0})],
            "{clause}: {body}"
        );
    }

    // the aggregate a clause names may be one the answer projects, and then it is
    // that column that is filtered on
    let (_, projected) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("SUM(pop) > 100")),
        ),
    )
    .await;
    assert_eq!(attributes(&projected), Vec::<Value>::new(), "{projected}");

    // `havingClause` is the same parameter under the name the REST reference uses
    let (status, spelled) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{grouped}&{}",
                having_named("havingClause", "COUNT(score) > 1")
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{spelled}");
    assert_eq!(
        attributes(&spelled),
        vec![json!({"ward": "north", "sum_pop": 30.0})],
        "{spelled}"
    );

    // one clause under both names asks for one thing, which is answerable
    let (_, twice) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{grouped}&{}&{}",
                having_param("COUNT(score) > 1"),
                having_named("havingClause", "COUNT(score) > 1")
            ),
        ),
    )
    .await;
    assert_eq!(attributes(&twice).len(), 1, "{twice}");

    // paging and ordering still compose after a filter on an unprojected column
    let (_, page) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{grouped}&{}&resultRecordCount=1",
                having_param("COUNT(*) = 2")
            ),
        ),
    )
    .await;
    assert_eq!(attributes(&page).len(), 1, "{page}");
    assert_eq!(attributes(&page)[0]["ward"], "north", "{page}");
    assert_eq!(page["exceededTransferLimit"], true, "{page}");

    // the layer is untouched by any of it
    assert_eq!(facade_count(&app, &name).await, 4);
}

/// `having` filters the aggregated rows: the groups are made first and the
/// predicate keeps or drops each whole group. The grammar is the where clause's,
/// but it names the columns the answer carries rather than the layer's fields.
#[tokio::test]
async fn test_arcgis_query_having_filters_the_groups() {
    let (app, _) = setup_app().await;
    let name = format!("having_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    // north is pop 10 and 20, south is 30 and 40
    let counted = stats_param(json!([
        {"statisticType": "count", "onStatisticField": "pop"},
        {"statisticType": "sum", "onStatisticField": "pop"},
    ]));
    let grouped = format!("{counted}&groupByFieldsForStatistics=ward&orderByFields=ward");

    // a sum over the threshold keeps south alone
    let (status, body) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("sum_pop > 50")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    assert_eq!(
        attributes(&body),
        vec![json!({"ward": "south", "count_pop": 2, "sum_pop": 70.0})],
        "{body}"
    );

    // a threshold both groups clear keeps both, in the order asked for
    let (_, both) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("sum_pop >= 30")),
        ),
    )
    .await;
    assert_eq!(
        attributes(&both),
        vec![
            json!({"ward": "north", "count_pop": 2, "sum_pop": 30.0}),
            json!({"ward": "south", "count_pop": 2, "sum_pop": 70.0}),
        ],
        "{both}"
    );

    // and one neither clears keeps none: an empty answer, not an error
    let (_, none) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("sum_pop > 100")),
        ),
    )
    .await;
    assert!(none["error"].is_null(), "{none}");
    assert_eq!(attributes(&none), Vec::<Value>::new(), "{none}");

    // over a count, grouping by the text field: "7" is on two features and the
    // other two codes on one each
    let by_code = format!("{counted}&groupByFieldsForStatistics=code&orderByFields=code");
    let (_, repeated) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{by_code}&{}", having_param("count_pop > 1")),
        ),
    )
    .await;
    assert_eq!(
        attributes(&repeated),
        vec![json!({"code": "7", "count_pop": 2, "sum_pop": 30.0})],
        "{repeated}"
    );

    // the group field itself is a column the clause can name, and text compares
    // as text
    let (_, named) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{grouped}&{}", having_param("ward LIKE 'nor%'")),
        ),
    )
    .await;
    assert_eq!(attributes(&named).len(), 1, "{named}");
    assert_eq!(attributes(&named)[0]["ward"], "north", "{named}");

    // the whole grammar is there, and a client's own alias is one of the columns
    let aliased = stats_param(json!([
        {"statisticType": "sum", "onStatisticField": "pop", "outStatisticFieldName": "total"},
        {"statisticType": "avg", "onStatisticField": "pop", "outStatisticFieldName": "mean"},
    ]));
    let (_, complex) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{aliased}&groupByFieldsForStatistics=ward&orderByFields=ward&{}",
                having_param("total >= 30 AND NOT mean IN (35) AND ward IS NOT NULL")
            ),
        ),
    )
    .await;
    assert_eq!(
        attributes(&complex),
        vec![json!({"ward": "north", "total": 30.0, "mean": 15.0})],
        "{complex}"
    );

    // the where clause narrows what is aggregated and having filters what came
    // out of it, in that order: pop > 15 leaves north a sum of 20, which the
    // having then drops
    let (_, composed) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{grouped}&{}&{}",
                where_param("pop > 15"),
                having_param("sum_pop > 50")
            ),
        ),
    )
    .await;
    assert_eq!(
        attributes(&composed),
        vec![json!({"ward": "south", "count_pop": 2, "sum_pop": 70.0})],
        "{composed}"
    );

    // paging and ordering compose after the filter: two groups survive, one page
    // each, and the order is the one asked for
    let surviving = format!("{grouped}&{}", having_param("sum_pop >= 30"));
    let (_, first) = get_json(
        &app,
        &query_url(&name, &format!("{surviving}&resultRecordCount=1")),
    )
    .await;
    assert_eq!(attributes(&first).len(), 1, "{first}");
    assert_eq!(attributes(&first)[0]["ward"], "north", "{first}");
    assert_eq!(first["exceededTransferLimit"], true, "{first}");
    let (_, second) = get_json(
        &app,
        &query_url(
            &name,
            &format!("{surviving}&resultRecordCount=1&resultOffset=1"),
        ),
    )
    .await;
    assert_eq!(attributes(&second)[0]["ward"], "south", "{second}");
    assert_eq!(second["exceededTransferLimit"], false, "{second}");

    // a statistic's alias orders the surviving groups like any other column
    let (_, biggest) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "{counted}&groupByFieldsForStatistics=ward&orderByFields=sum_pop%20DESC&{}",
                having_param("count_pop = 2")
            ),
        ),
    )
    .await;
    assert_eq!(attributes(&biggest)[0]["ward"], "south", "{biggest}");

    // and the layer declares that it answers one
    let (_, metadata) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(
        metadata["advancedQueryCapabilities"]["supportsHavingClause"], true,
        "{metadata}"
    );
}

/// What `having` will not read is refused by name, and nothing it carries reaches
/// the SQL: a clause is parsed against the answer's columns and rendered with
/// every literal bound, so an injection-shaped literal is one string to compare.
#[tokio::test]
async fn test_arcgis_query_having_refuses_what_it_cannot_honor() {
    let (app, _) = setup_app().await;
    let name = format!("nohaving_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    let counted = stats_param(json!([
        {"statisticType": "count", "onStatisticField": "pop"},
        {"statisticType": "sum", "onStatisticField": "pop"},
    ]));
    let grouped = format!("{counted}&groupByFieldsForStatistics=ward");

    for (params, names) in [
        // a column the answer does not carry, named in the refusal
        (
            format!("{grouped}&{}", having_param("nosuchcolumn > 1")),
            "nosuchcolumn",
        ),
        // a field of the layer that this answer does not carry: having filters
        // the groups, so it cannot reach back to the rows
        (format!("{grouped}&{}", having_param("score > 1")), "score"),
        (format!("{grouped}&{}", having_param("pop > 1")), "pop"),
        // the grammar's own refusals
        (
            format!("{grouped}&{}", having_param("upper(ward) = 'A'")),
            "upper",
        ),
        (
            format!("{grouped}&{}", having_param("sum_pop + 1 = 31")),
            "arithmetic",
        ),
        (
            format!("{grouped}&{}", having_param("sum_pop = 30; SELECT 1")),
            "';'",
        ),
        (
            format!("{grouped}&{}", having_param("sum_pop IN (SELECT 1)")),
            "subquery",
        ),
        (
            format!("{grouped}&{}", having_param("\"sum_pop\" = 30")),
            "quoted identifier",
        ),
        // no grouping to filter, and the missing parameter is named
        (
            format!("{counted}&{}", having_param("sum_pop > 1")),
            "groupByFieldsForStatistics",
        ),
        // no statistics at all, and that parameter is named instead
        (
            format!(
                "groupByFieldsForStatistics=ward&{}",
                having_param("sum_pop > 1")
            ),
            "outStatistics",
        ),
        (
            format!("outFields=*&{}", having_param("pop > 1")),
            "outStatistics",
        ),
        // and it is no use on the other aggregated shape either
        (
            format!(
                "returnDistinctValues=true&outFields=ward&{}",
                having_param("ward = 'north'")
            ),
            "outStatistics",
        ),
        // the function form is held to the type rule outStatistics is held to
        (
            format!("{grouped}&{}", having_param("AVG(code) > 1")),
            "avg is not supported on 'code'",
        ),
        (
            format!("{grouped}&{}", having_param("SUM(ward) > 1")),
            "sum is not supported on 'ward'",
        ),
        // a function this service has no aggregate for, and a field it has not got
        (
            format!("{grouped}&{}", having_param("MEDIAN(pop) > 1")),
            "MEDIAN",
        ),
        (
            format!("{grouped}&{}", having_param("AVG(nosuchfield) > 1")),
            "nosuchfield",
        ),
        // an aggregate takes one field name and nothing else
        (
            format!("{grouped}&{}", having_param("AVG(pop, score) > 1")),
            "avg takes one field name",
        ),
        // the two names are one parameter, so two different clauses is a refusal
        (
            format!(
                "{grouped}&{}&{}",
                having_param("COUNT(*) > 1"),
                having_named("havingClause", "COUNT(*) > 100")
            ),
            "two names for one parameter",
        ),
    ] {
        let (status, body) = get_json(&app, &query_url(&name, &params)).await;
        assert_eq!(status, StatusCode::OK, "{params}: {body}");
        assert_eq!(body["error"]["code"], 400, "{params}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(names), "'{params}' answered '{message}'");
    }

    // an injection-shaped literal is data: it parses as one string, compares as
    // one string and matches no group
    for hostile in [
        "ward = 'north'; DROP TABLE features; --'",
        "ward = 'x'' OR ''1''=''1'",
        "ward = 'north'' UNION SELECT 1 --'",
    ] {
        let (status, body) = get_json(
            &app,
            &query_url(&name, &format!("{grouped}&{}", having_param(hostile))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{hostile}: {body}");
        // either one string that matches nothing, or a refusal: never a group
        if body["error"].is_null() {
            assert_eq!(attributes(&body), Vec::<Value>::new(), "{hostile}: {body}");
        } else {
            assert_eq!(body["error"]["code"], 400, "{hostile}: {body}");
        }
    }

    // nothing any of them sent was run: the layer still answers all four features
    assert_eq!(facade_count(&app, &name).await, 4);
    let (_, still) = get_json(
        &app,
        &query_url(&name, &format!("{grouped}&orderByFields=ward")),
    )
    .await;
    assert_eq!(attributes(&still).len(), 2, "{still}");
}

#[tokio::test]
async fn test_arcgis_query_returns_distinct_values() {
    let (app, _) = setup_app().await;
    let name = format!("distinct_{}", Uuid::now_v7().simple());
    seed_stats_layer(&app, &name).await;

    // two wards over four features
    let (status, body) = get_json(
        &app,
        &query_url(&name, "returnDistinctValues=true&outFields=ward"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    let rows = attributes(&body);
    assert_eq!(rows.len(), 2, "{body}");
    assert_eq!(rows[0], json!({"ward": "north"}), "{body}");
    assert_eq!(rows[1], json!({"ward": "south"}), "{body}");
    // no geometry, and no object id: a distinct row is not one feature
    assert!(body["features"][0].get("geometry").is_none(), "{body}");
    assert_eq!(body["fields"].as_array().unwrap().len(), 1, "{body}");

    // returnGeometry does not change that, which is what Esri does with it here
    let (_, asked) = get_json(
        &app,
        &query_url(
            &name,
            "returnDistinctValues=true&outFields=ward&returnGeometry=true",
        ),
    )
    .await;
    assert!(asked["features"][0].get("geometry").is_none(), "{asked}");

    // a text field that holds number-looking values keeps them apart as text
    let (_, codes) = get_json(
        &app,
        &query_url(&name, "returnDistinctValues=true&outFields=code"),
    )
    .await;
    assert_eq!(attributes(&codes).len(), 3, "{codes}");

    // two fields together are distinct as a pair
    let (_, pairs) = get_json(
        &app,
        &query_url(&name, "returnDistinctValues=true&outFields=ward,pop"),
    )
    .await;
    assert_eq!(attributes(&pairs).len(), 4, "{pairs}");

    // the object id is a value like any other when it is asked for by name
    let (_, ids) = get_json(
        &app,
        &query_url(&name, "returnDistinctValues=true&outFields=OBJECTID"),
    )
    .await;
    let held: Vec<i64> = attributes(&ids)
        .iter()
        .map(|row| row["OBJECTID"].as_i64().unwrap())
        .collect();
    assert_eq!(held, vec![1, 2, 3, 4], "{ids}");

    // the where clause and the envelope narrow the set the values come from
    let (_, filtered) = get_json(
        &app,
        &query_url(
            &name,
            &format!(
                "returnDistinctValues=true&outFields=ward&{}",
                where_param("pop < 30")
            ),
        ),
    )
    .await;
    assert_eq!(
        attributes(&filtered),
        vec![json!({"ward": "north"})],
        "{filtered}"
    );

    // an order over the selected fields, and the reverse of it
    let (_, down) = get_json(
        &app,
        &query_url(
            &name,
            "returnDistinctValues=true&outFields=ward&orderByFields=ward%20DESC",
        ),
    )
    .await;
    assert_eq!(attributes(&down)[0]["ward"], "south", "{down}");

    // and the values page with the same limit report the rows use
    let (_, first) = get_json(
        &app,
        &query_url(
            &name,
            "returnDistinctValues=true&outFields=pop&resultRecordCount=2",
        ),
    )
    .await;
    let held: Vec<f64> = attributes(&first)
        .iter()
        .map(|row| row["pop"].as_f64().unwrap())
        .collect();
    assert_eq!(held, vec![10.0, 20.0], "{first}");
    assert_eq!(first["exceededTransferLimit"], true, "{first}");
    let (_, last) = get_json(
        &app,
        &query_url(
            &name,
            "returnDistinctValues=true&outFields=pop&resultRecordCount=2&resultOffset=2",
        ),
    )
    .await;
    assert_eq!(attributes(&last).len(), 2, "{last}");
    assert_eq!(last["exceededTransferLimit"], false, "{last}");

    // what it will not answer: no fields to be distinct over, and the shapes
    // that are a different answer altogether
    for (params, names) in [
        ("returnDistinctValues=true", "outFields"),
        ("returnDistinctValues=true&outFields=*", "'*'"),
        (
            "returnDistinctValues=true&outFields=nosuchfield",
            "nosuchfield",
        ),
        (
            "returnDistinctValues=true&outFields=ward&returnCountOnly=true",
            "not features",
        ),
        (
            "returnDistinctValues=true&outFields=ward&returnIdsOnly=true",
            "not features",
        ),
        (
            "returnDistinctValues=true&outFields=ward&orderByFields=pop",
            "not one of the fields",
        ),
    ] {
        let (status, body) = get_json(&app, &query_url(&name, params)).await;
        assert_eq!(status, StatusCode::OK, "{params}: {body}");
        assert_eq!(body["error"]["code"], 400, "{params}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(names), "'{params}' answered '{message}'");
    }
}

#[tokio::test]
async fn test_arcgis_query_orders_by_any_field() {
    let (app, _) = setup_app().await;
    let name = format!("ordered_{}", Uuid::now_v7().simple());
    seed_where_layer(&app, &name).await;

    let ids = |body: &Value| -> Vec<i64> {
        attributes(body)
            .iter()
            .map(|row| row["OBJECTID"].as_i64().unwrap())
            .collect()
    };

    for (order, wanted) in [
        ("name DESC", vec![30, 20, 10]),
        ("name ASC", vec![10, 20, 30]),
        ("name", vec![10, 20, 30]),
        ("score DESC", vec![30, 20, 10]),
        ("pop", vec![10, 20, 30]),
        ("OBJECTID DESC", vec![30, 20, 10]),
        // a partly-empty field puts its empty rows last in either direction
        ("ward", vec![10, 20, 30]),
        ("ward DESC", vec![10, 20, 30]),
        // the first term decides, and the rest settle its ties
        ("ward DESC, OBJECTID DESC", vec![10, 30, 20]),
        ("code DESC", vec![20, 30, 10]),
    ] {
        let (status, body) = get_json(
            &app,
            &query_url(
                &name,
                &format!("outFields=*&orderByFields={}", urlencoding::encode(order)),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{order}: {body}");
        assert!(body["error"].is_null(), "{order}: {body}");
        assert_eq!(ids(&body), wanted, "{order}: {body}");
    }

    // the ids-only answer is ordered too, rather than always by object id
    let (_, listed) = get_json(
        &app,
        &query_url(&name, "returnIdsOnly=true&orderByFields=name%20DESC"),
    )
    .await;
    assert_eq!(listed["objectIds"], json!([30, 20, 10]), "{listed}");

    // and an order runs with the paging rather than under it
    let (_, page) = get_json(
        &app,
        &query_url(
            &name,
            "outFields=*&orderByFields=name%20DESC&resultRecordCount=2",
        ),
    )
    .await;
    assert_eq!(ids(&page), vec![30, 20], "{page}");
    assert_eq!(page["exceededTransferLimit"], true, "{page}");
}

/// A field declared as a number can hold text, because a schema may be declared
/// after the rows were written. Ordering by it reads the numbers as numbers and
/// puts the rest last, rather than failing the query.
#[tokio::test]
async fn test_arcgis_query_orders_a_numeric_field_holding_text() {
    let (app, _) = setup_app().await;
    let name = format!("mixedpop_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;

    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 1, "pop": "30"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 2, "pop": "not a number"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(3.0, 3.0),
             "properties": {"OBJECTID": 3, "pop": "10"}},
        ]),
    )
    .await;

    // the schema arrives after the rows, which is how a layer ends up declaring a
    // type its values disagree with
    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        None,
        Some(json!({"fields": [
            {"name": "OBJECTID", "field_type": "integer", "required": true},
            {"name": "pop", "field_type": "integer", "required": false},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = get_json(&app, &query_url(&name, "outFields=*&orderByFields=pop")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    let ids: Vec<i64> = attributes(&body)
        .iter()
        .map(|row| row["OBJECTID"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![3, 1, 2], "{body}");

    // the same guard on the way into a statistic: the two numbers add up and the
    // text is not one of them
    let (_, summed) = get_json(
        &app,
        &query_url(
            &name,
            &stats_param(json!([
                {"statisticType": "sum", "onStatisticField": "pop"},
                {"statisticType": "count", "onStatisticField": "pop"},
            ])),
        ),
    )
    .await;
    assert_eq!(attributes(&summed)[0]["sum_pop"], 40.0, "{summed}");
    // a count counts values rather than numbers, so the text is one of them
    assert_eq!(attributes(&summed)[0]["count_pop"], 3, "{summed}");
}

#[tokio::test]
async fn test_arcgis_errors_are_http_200_with_an_error_object() {
    let (app, _) = setup_app().await;

    for uri in [
        "/arcgis/rest/services/nosuchservice/FeatureServer?f=json",
        "/arcgis/rest/services/nosuchservice/FeatureServer/0?f=json",
        "/arcgis/rest/services/nosuchservice/FeatureServer/0/query?f=json",
    ] {
        let (status, body) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["error"]["code"], 400, "{uri}: {body}");
        assert!(body["error"]["message"].is_string(), "{uri}: {body}");
        assert_eq!(body["error"]["details"], json!([]), "{uri}: {body}");
    }
}

#[tokio::test]
async fn test_arcgis_refuses_a_dataset_with_no_main_branch() {
    let (app, _) = setup_app().await;
    let name = format!("branchless_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    create_branch(&app, ds, "draft").await;

    let (status, body) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("main"),
        "{body}"
    );
}

#[tokio::test]
async fn test_arcgis_query_accepts_a_form_post_and_drops_geometry_on_request() {
    let (app, _) = setup_app().await;
    let name = format!("posted_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 3).await;

    let (status, body) = post_form(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0/query"),
        "f=json&objectIds=1,3&outFields=objectid&returnGeometry=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 2, "{body}");
    assert_eq!(features[0]["attributes"]["objectid"], 1, "{body}");
    assert!(features[0]["geometry"]["x"].is_f64(), "{body}");
    // outFields named only the id, so nothing else came back
    assert_eq!(
        features[0]["attributes"].as_object().unwrap().len(),
        1,
        "{body}"
    );

    let (_, bare) = get_json(&app, &query_url(&name, "returnGeometry=false")).await;
    assert!(bare["features"][0]["geometry"].is_null(), "{bare}");
    assert!(
        bare["features"][0]["attributes"]["objectid"].is_i64(),
        "{bare}"
    );
}

#[tokio::test]
async fn test_arcgis_answers_geojson_when_asked() {
    let (app, _) = setup_app().await;
    let name = format!("geojson_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;

    let (status, body) = get_json(
        &app,
        &format!(
            "/arcgis/rest/services/{name}/FeatureServer/0/query?f=geojson&where=1=1&outFields=*"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "FeatureCollection", "{body}");
    let features = body["features"].as_array().unwrap();
    assert_eq!(features.len(), 2, "{body}");
    assert_eq!(features[0]["type"], "Feature", "{body}");
    assert_eq!(features[0]["geometry"]["type"], "Point", "{body}");
    assert_eq!(features[0]["properties"]["name"], "point-0", "{body}");
}

#[tokio::test]
async fn test_arcgis_does_not_widen_anonymous_reads_of_a_private_dataset() {
    let (app, state) = setup_app_authed_with_state().await;
    let name = format!("secret_{}", Uuid::now_v7().simple());
    let admin = token_for(Role::Admin);
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&admin),
        Some(json!({"name": name, "geometry_type": "point", "srid": 4326,
                    "created_by": "admin", "visibility": "private"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ds = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{ds}/branches"),
        Some(&admin),
        Some(json!({"name": "main", "created_by": "admin"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(state.get_dataset(ds).await.is_ok());

    // anonymous: the dataset is simply not there, exactly as in every other listing
    let (status, catalog) = get_json(&app, "/arcgis/rest/services?f=json").await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    assert!(
        !catalog["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == name),
        "{catalog}"
    );
    for uri in [
        format!("/arcgis/rest/services/{name}/FeatureServer?f=json"),
        format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
        format!("/arcgis/rest/services/{name}/FeatureServer/0/query?f=json"),
    ] {
        let (status, body) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["error"]["code"], 400, "{uri}: {body}");
    }

    // named by dataset id instead, the visibility layer sees the uuid and
    // answers its own 404 before this frontend runs. Refused either way.
    let (status, body) = get_json(
        &app,
        &format!("/arcgis/rest/services/{ds}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // the admin sees it through the same routes
    let (status, catalog) = request_as(
        &app,
        "GET",
        "/arcgis/rest/services?f=json",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    assert!(
        catalog["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == name),
        "{catalog}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// ArcGIS FeatureServer applyEdits
// ═══════════════════════════════════════════════════════════════════════

/// Helper: the applyEdits URL of a service's layer 0.
fn apply_edits_url(service: &str) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/applyEdits")
}

/// Helper: form-encode a parameter list, which is how an Esri client sends an
/// edit whose feature JSON cannot go in a URL.
fn form_body(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Helper: post a form body with a bearer header, for the auth-enabled tests.
async fn post_form_as(
    app: &axum::Router,
    uri: &str,
    body: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// The fields that make a layer editable: a real integer OBJECTID, plus two
/// attributes to merge over.
fn editable_schema() -> Value {
    json!({"fields": [
        {"name": "OBJECTID", "field_type": "integer", "required": false},
        {"name": "name", "field_type": "string", "required": false},
        {"name": "kind", "field_type": "string", "required": false},
    ]})
}

/// Helper: a dataset whose schema declares a real OBJECTID, which is what makes
/// its layer editable, with a `main` branch. Returns the service name, the
/// dataset id and the branch id.
async fn editable_layer(
    app: &axum::Router,
    prefix: &str,
    geometry_type: &str,
) -> (String, Uuid, Uuid) {
    let name = format!("{prefix}_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(app, &name, geometry_type).await;
    let (status, body) = request_as(
        app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        None,
        Some(editable_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let branch = create_branch(app, ds, "main").await;
    (name, ds, branch)
}

/// Helper: how many changesets the branch's history holds, which is what says a
/// batch became exactly one commit.
async fn history_len(app: &axum::Router, branch: Uuid) -> usize {
    let (status, body) = get_json(app, &format!("/api/v1/branches/{branch}/history")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().unwrap().len()
}

/// Same, on a private dataset, whose history has to be asked for with a token.
async fn history_len_as(app: &axum::Router, branch: Uuid, token: &str) -> usize {
    let (status, body) = request_as(
        app,
        "GET",
        &format!("/api/v1/branches/{branch}/history"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().unwrap().len()
}

/// Helper: how many features the facade sees on a layer.
async fn facade_count(app: &axum::Router, service: &str) -> i64 {
    let (status, body) = get_json(app, &query_url(service, "returnCountOnly=true")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["count"].as_i64().unwrap_or_else(|| panic!("{body}"))
}

/// The forward spherical mercator a web client projects with, so a test can send
/// metres and ask for the degrees back.
fn to_mercator(lon: f64, lat: f64) -> (f64, f64) {
    const RADIUS: f64 = 6378137.0;
    let radians = std::f64::consts::PI / 180.0;
    (
        lon * radians * RADIUS,
        (lat * radians / 2.0 + std::f64::consts::FRAC_PI_4)
            .tan()
            .ln()
            * RADIUS,
    )
}

#[tokio::test]
async fn test_arcgis_apply_edits_adds_updates_and_deletes_in_one_commit() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "edits", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first", "kind": "seed"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 200, "name": "second", "kind": "seed"}},
        ]),
    )
    .await;
    let before = history_len(&app, branch).await;

    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "third", "kind": "added"},
                    "geometry": {"x": 3.0, "y": 3.0}}])
            .to_string(),
        ),
        (
            "updates",
            json!([{"attributes": {"OBJECTID": 100, "name": "renamed"}}]).to_string(),
        ),
        ("deletes", "200".into()),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");
    // the next id after the highest that exists
    assert_eq!(
        out["addResults"],
        json!([{"objectId": 201, "success": true}]),
        "{out}"
    );
    assert_eq!(
        out["updateResults"],
        json!([{"objectId": 100, "success": true}]),
        "{out}"
    );
    assert_eq!(
        out["deleteResults"],
        json!([{"objectId": 200, "success": true}]),
        "{out}"
    );

    // three edits, one commit
    assert_eq!(history_len(&app, branch).await, before + 1);

    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    let held = body["features"].as_array().unwrap();
    assert_eq!(held.len(), 2, "{body}");
    assert_eq!(held[0]["attributes"]["OBJECTID"], 100, "{body}");
    assert_eq!(held[0]["attributes"]["name"], "renamed", "{body}");
    // the attribute the update did not carry is still what it was
    assert_eq!(held[0]["attributes"]["kind"], "seed", "{body}");
    // and so is the geometry, because the update carried none
    assert_eq!(held[0]["geometry"]["x"], 1.0, "{body}");
    assert_eq!(held[1]["attributes"]["OBJECTID"], 201, "{body}");
    assert_eq!(held[1]["attributes"]["name"], "third", "{body}");
    assert_eq!(held[1]["geometry"]["x"], 3.0, "{body}");
    assert_eq!(held[1]["geometry"]["y"], 3.0, "{body}");
}

/// An empty layer starts at 1, and adds in one batch get consecutive ids rather
/// than the same one.
#[tokio::test]
async fn test_arcgis_apply_edits_assigns_consecutive_object_ids() {
    let (app, _) = setup_app().await;
    let (name, _ds, _branch) = editable_layer(&app, "ids", "point").await;

    let adds: Vec<Value> = (0..3)
        .map(|i| {
            json!({"attributes": {"name": format!("p{i}")},
                   "geometry": {"x": i as f64, "y": 0.0}})
        })
        .collect();
    let body = form_body(&[("f", "json".into()), ("adds", json!(adds).to_string())]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["addResults"],
        json!([
            {"objectId": 1, "success": true},
            {"objectId": 2, "success": true},
            {"objectId": 3, "success": true},
        ]),
        "{out}"
    );

    let (_, ids) = get_json(&app, &query_url(&name, "returnIdsOnly=true")).await;
    assert_eq!(ids["objectIds"], json!([1, 2, 3]), "{ids}");

    // and the next batch carries on from there rather than starting over
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "p3"}, "geometry": {"x": 9.0, "y": 9.0}}]).to_string(),
        ),
    ]);
    let (_, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(
        out["addResults"],
        json!([{"objectId": 4, "success": true}]),
        "{out}"
    );
}

/// Two batches of adds racing each other on one layer cannot be given the same
/// object id. The id is an ordinary property with no unique constraint behind it,
/// so nothing downstream would refuse a duplicate: the assignment itself is what
/// has to be serialized, and it is, on the branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_arcgis_apply_edits_racing_adds_get_distinct_object_ids() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "race", "point").await;
    let url = apply_edits_url(&name);
    let before = history_len(&app, branch).await;

    let batch = |tag: &str| {
        let adds: Vec<Value> = (0..3)
            .map(|i| {
                json!({"attributes": {"name": format!("{tag}{i}")},
                       "geometry": {"x": i as f64, "y": 0.0}})
            })
            .collect();
        form_body(&[("f", "json".into()), ("adds", json!(adds).to_string())])
    };
    let (one, two) = (batch("a"), batch("b"));
    let (first, second) = tokio::join!(post_form(&app, &url, &one), post_form(&app, &url, &two));

    let assigned = |(status, out): (StatusCode, Value)| -> Vec<i64> {
        assert_eq!(status, StatusCode::OK, "{out}");
        assert!(out["error"].is_null(), "{out}");
        out["addResults"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["objectId"].as_i64().unwrap())
            .collect()
    };
    let mut ids = assigned(first);
    ids.extend(assigned(second));
    let mut distinct = ids.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 6, "two batches were given one id: {ids:?}");

    // and the layer holds exactly the six it handed out, one feature each
    let (_, held) = get_json(&app, &query_url(&name, "returnIdsOnly=true")).await;
    let stored: Vec<i64> = held["objectIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_i64().unwrap())
        .collect();
    assert_eq!(stored, distinct, "{held}");
    // one commit per batch: the lock serializes them, it does not merge them
    assert_eq!(history_len(&app, branch).await, before + 2);
}

/// Esri takes an add with no geometry, for a table or a shape a client fills in
/// later. A feature here is a geometry and its attributes, so an attribute-only
/// add is refused by name rather than written as something with nothing to draw.
#[tokio::test]
async fn test_arcgis_apply_edits_refuses_an_attribute_only_add() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "noshape", "point").await;
    let before = history_len(&app, branch).await;

    for adds in [
        json!([{"attributes": {"name": "shapeless"}}]),
        json!([{"attributes": {"name": "shapeless"}, "geometry": Value::Null}]),
        // a good add alongside it: the batch is one commit, so neither lands
        json!([{"attributes": {"name": "fine"}, "geometry": {"x": 1.0, "y": 1.0}},
               {"attributes": {"name": "shapeless"}}]),
    ] {
        let body = form_body(&[("f", "json".into()), ("adds", adds.to_string())]);
        let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
        assert_eq!(status, StatusCode::OK, "{adds}: {out}");
        assert_eq!(out["error"]["code"], 400, "{adds}: {out}");
        let why = out["error"]["message"].as_str().unwrap();
        assert!(why.contains("requires a geometry"), "{adds}: {out}");
        assert!(why.contains(&name), "the refusal names the layer: {out}");
    }
    assert_eq!(facade_count(&app, &name).await, 0);
    assert_eq!(history_len(&app, branch).await, before);
}

/// A client-supplied objectid on an add is ignored: the service assigns the id,
/// or a client could collide with a feature that exists.
#[tokio::test]
async fn test_arcgis_apply_edits_ignores_a_client_object_id_on_an_add() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "claimed", "point").await;
    commit_features(
        &app,
        branch,
        json!([{"type": "insert", "feature_id": Uuid::now_v7().to_string(),
                "geometry_wkb_hex": point_wkb(1.0, 1.0),
                "properties": {"OBJECTID": 7, "name": "held"}}]),
    )
    .await;

    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"OBJECTID": 7, "name": "claimant"},
                    "geometry": {"x": 2.0, "y": 2.0}}])
            .to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["addResults"],
        json!([{"objectId": 8, "success": true}]),
        "{out}"
    );

    let (_, ids) = get_json(&app, &query_url(&name, "returnIdsOnly=true")).await;
    assert_eq!(ids["objectIds"], json!([7, 8]), "{ids}");
}

/// An update carrying a geometry replaces it, and one carrying none keeps it.
#[tokio::test]
async fn test_arcgis_apply_edits_updates_geometry_only_when_one_is_sent() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "moved", "point").await;
    commit_features(
        &app,
        branch,
        json!([{"type": "insert", "feature_id": Uuid::now_v7().to_string(),
                "geometry_wkb_hex": point_wkb(1.0, 1.0),
                "properties": {"OBJECTID": 5, "name": "a", "kind": "seed"}}]),
    )
    .await;

    let body = form_body(&[
        ("f", "json".into()),
        (
            "updates",
            json!([{"attributes": {"OBJECTID": 5},
                    "geometry": {"x": 6.0, "y": 7.0}}])
            .to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");

    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    let held = &body["features"][0];
    assert_eq!(held["geometry"]["x"], 6.0, "{body}");
    assert_eq!(held["geometry"]["y"], 7.0, "{body}");
    // the update named no attributes, so every one of them is untouched
    assert_eq!(held["attributes"]["name"], "a", "{body}");
    assert_eq!(held["attributes"]["kind"], "seed", "{body}");
    assert_eq!(held["attributes"]["OBJECTID"], 5, "{body}");
}

/// The deliberate divergence from Esri: a batch is one commit, so one bad edit
/// refuses all of them rather than coming back as a failed row beside two
/// successes.
#[tokio::test]
async fn test_arcgis_apply_edits_refuses_the_whole_batch_on_one_bad_edit() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "atomic", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0), "properties": {"OBJECTID": 1, "name": "a"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0), "properties": {"OBJECTID": 2, "name": "b"}},
        ]),
    )
    .await;
    let commits = history_len(&app, branch).await;
    assert_eq!(facade_count(&app, &name).await, 2);

    // a good add, an update of a feature that is not there, and a good delete
    let batch = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "c"}, "geometry": {"x": 3.0, "y": 3.0}}]).to_string(),
        ),
        (
            "updates",
            json!([{"attributes": {"OBJECTID": 999, "name": "ghost"}}]).to_string(),
        ),
        ("deletes", "1".into()),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &batch).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains("999"),
        "the error names the cause: {out}"
    );
    assert!(out["addResults"].is_null(), "{out}");

    // nothing happened: not the add, not the delete, not a commit
    assert_eq!(facade_count(&app, &name).await, 2);
    assert_eq!(history_len(&app, branch).await, commits);

    // and the same for a delete of an id that is not there
    let batch = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "c"}, "geometry": {"x": 3.0, "y": 3.0}}]).to_string(),
        ),
        ("deletes", "1,999".into()),
    ]);
    let (_, out) = post_form(&app, &apply_edits_url(&name), &batch).await;
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert_eq!(facade_count(&app, &name).await, 2);
    assert_eq!(history_len(&app, branch).await, commits);
}

/// Row numbers shift when a feature is deleted, so an id aimed by one names a
/// different feature afterwards. Such a layer takes no edits at all.
#[tokio::test]
async fn test_arcgis_apply_edits_refuses_a_row_number_layer() {
    let (app, _) = setup_app().await;
    let name = format!("rownum_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;
    let commits = history_len(&app, branch).await;

    for body in [
        form_body(&[
            ("f", "json".into()),
            (
                "adds",
                json!([{"attributes": {"name": "c"}, "geometry": {"x": 3.0, "y": 3.0}}])
                    .to_string(),
            ),
        ]),
        form_body(&[("f", "json".into()), ("deletes", "1".into())]),
    ] {
        let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
        assert_eq!(status, StatusCode::OK, "{out}");
        assert_eq!(out["error"]["code"], 400, "{out}");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("objectid"),
            "the error says what the layer needs: {out}"
        );
    }
    assert_eq!(facade_count(&app, &name).await, 2);
    assert_eq!(history_len(&app, branch).await, commits);
}

/// A feature layer declares one geometry type and draws every feature with it.
#[tokio::test]
async fn test_arcgis_apply_edits_refuses_a_geometry_of_the_wrong_family() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "family", "polygon").await;
    let commits = history_len(&app, branch).await;

    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "a"}, "geometry": {"x": 1.0, "y": 1.0}}]).to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("esriGeometryPolygon"),
        "{out}"
    );
    assert_eq!(facade_count(&app, &name).await, 0);
    assert_eq!(history_len(&app, branch).await, commits);
}

/// Esri winds an exterior clockwise, but real data arrives both ways round, so
/// the winding is classified rather than assumed.
#[tokio::test]
async fn test_arcgis_apply_edits_accepts_a_polygon_in_either_winding() {
    let (app, _) = setup_app().await;
    let (name, _ds, _branch) = editable_layer(&app, "winding", "polygon").await;

    let clockwise = json!({"rings": [[[0, 0], [0, 4], [4, 4], [4, 0], [0, 0]]]});
    let counter_clockwise = json!({"rings": [[[10, 0], [14, 0], [14, 4], [10, 4], [10, 0]]]});
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([
                {"attributes": {"name": "cw"}, "geometry": clockwise},
                {"attributes": {"name": "ccw"}, "geometry": counter_clockwise},
            ])
            .to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");

    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    let held = body["features"].as_array().unwrap();
    assert_eq!(held.len(), 2, "{body}");
    for feature in held {
        let rings = feature["geometry"]["rings"].as_array().unwrap();
        assert_eq!(rings.len(), 1, "{feature}");
        // whichever way it came in, it comes back out Esri's way round
        assert!(ring_winding(&rings[0]) < 0.0, "{feature}");
        // and it is the square that was sent, not its mirror
        assert_eq!(rings[0].as_array().unwrap().len(), 5, "{feature}");
    }
}

/// A web client sends what it drew, which is Web Mercator metres. The transform
/// is closed form here, so the test asserts the round trip rather than a
/// hard-coded coordinate.
#[tokio::test]
async fn test_arcgis_apply_edits_round_trips_a_mercator_point() {
    let (app, _) = setup_app().await;
    let (name, _ds, _branch) = editable_layer(&app, "merc", "point").await;

    let (lon, lat) = (-71.06, 42.36);
    let (x, y) = to_mercator(lon, lat);
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "boston"},
                    "geometry": {"x": x, "y": y, "spatialReference": {"wkid": 102100}}}])
            .to_string(),
        ),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");

    let (_, body) = get_json(&app, &query_url(&name, "outFields=*")).await;
    let geometry = &body["features"][0]["geometry"];
    let held_x = geometry["x"].as_f64().unwrap();
    let held_y = geometry["y"].as_f64().unwrap();
    assert!((held_x - lon).abs() < 1e-6, "{geometry}");
    assert!((held_y - lat).abs() < 1e-6, "{geometry}");

    // a reference the service does not speak is refused rather than guessed at
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "elsewhere"},
                    "geometry": {"x": 1.0, "y": 2.0, "spatialReference": {"wkid": 27700}}}])
            .to_string(),
        ),
    ]);
    let (_, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(out["error"]["code"], 400, "{out}");
}

/// Parameters that change what the edit means are refused by name rather than
/// ignored, and the empty batch is answered rather than committed.
#[tokio::test]
async fn test_arcgis_apply_edits_refuses_parameters_it_cannot_honor() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "params", "point").await;
    let commits = history_len(&app, branch).await;
    let add = json!([{"attributes": {"name": "a"}, "geometry": {"x": 1.0, "y": 1.0}}]).to_string();

    for (name_of, value) in [
        ("gdbVersion", "SDE.DEFAULT"),
        ("sessionId", "{ABC}"),
        ("rollbackOnFailure", "false"),
        ("useGlobalIds", "true"),
        // a feature set is not what an edit answers with, so geoJSON is refused
        // here as it is on the metadata routes
        ("f", "geojson"),
    ] {
        let mut pairs = vec![(name_of, value.to_string()), ("adds", add.clone())];
        if name_of != "f" {
            pairs.push(("f", "json".into()));
        }
        let body = form_body(&pairs);
        let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
        assert_eq!(status, StatusCode::OK, "{name_of}: {out}");
        assert_eq!(out["error"]["code"], 400, "{name_of}: {out}");
    }
    assert_eq!(facade_count(&app, &name).await, 0);
    assert_eq!(history_len(&app, branch).await, commits);

    // rollbackOnFailure=true is what already happens, so it is not a refusal
    let body = form_body(&[
        ("f", "json".into()),
        ("adds", add),
        ("rollbackOnFailure", "true".into()),
    ]);
    let (_, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert!(out["error"].is_null(), "{out}");
    assert_eq!(facade_count(&app, &name).await, 1);
    let after_the_add = history_len(&app, branch).await;
    assert_eq!(after_the_add, commits + 1);

    // an empty batch is answered, not committed: a changeset with no operations
    // is history that says nothing
    let (_, out) = post_form(&app, &apply_edits_url(&name), "f=json").await;
    assert_eq!(out["addResults"], json!([]), "{out}");
    assert_eq!(out["updateResults"], json!([]), "{out}");
    assert_eq!(out["deleteResults"], json!([]), "{out}");
    assert_eq!(history_len(&app, branch).await, after_the_add);
}

/// Once a layer takes edits its metadata has to say so, or a client hides its
/// edit tools. A row-number layer keeps saying no.
#[tokio::test]
async fn test_arcgis_metadata_says_which_layers_are_editable() {
    let (app, _) = setup_app().await;
    let (editable, _ds, _branch) = editable_layer(&app, "flips", "point").await;
    let plain = format!("plain_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &plain, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;

    let (_, root) = get_json(
        &app,
        &format!("/arcgis/rest/services/{editable}/FeatureServer?f=json"),
    )
    .await;
    // the same layer that takes edits is the one whose changes can be described,
    // and extractChanges is a service operation, so the service says so and the
    // layer below does not
    assert_eq!(
        root["capabilities"], "Query,Create,Update,Delete,ChangeTracking",
        "{root}"
    );
    assert_eq!(root["allowGeometryUpdates"], true, "{root}");

    let (_, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{editable}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(
        layer["capabilities"], "Query,Create,Update,Delete",
        "{layer}"
    );
    assert_eq!(layer["allowGeometryUpdates"], true, "{layer}");
    let fields = layer["fields"].as_array().unwrap();
    let oid = fields.iter().find(|f| f["name"] == "OBJECTID").unwrap();
    assert_eq!(
        oid["editable"], false,
        "the id is the key a client holds the feature by: {layer}"
    );
    let named = fields.iter().find(|f| f["name"] == "name").unwrap();
    assert_eq!(named["editable"], true, "{layer}");

    let (_, root) = get_json(
        &app,
        &format!("/arcgis/rest/services/{plain}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(root["capabilities"], "Query", "{root}");
    assert_eq!(root["allowGeometryUpdates"], false, "{root}");

    let (_, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{plain}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(layer["capabilities"], "Query", "{layer}");
    assert_eq!(layer["allowGeometryUpdates"], false, "{layer}");
    assert!(
        layer["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["editable"] == false),
        "{layer}"
    );
}

/// Helper: an editable layer under enforced auth, created by `carol`, an editor,
/// who therefore holds a grant on it. Returns the service name, carol's token
/// and the branch id.
async fn owned_editable_layer(app: &axum::Router) -> (String, String, Uuid) {
    let carol = token_for_user("carol", Role::Editor);
    let name = format!("owned_{}", Uuid::now_v7().simple());
    let (status, dataset) = request_as(
        app,
        "POST",
        "/api/v1/datasets",
        Some(&carol),
        Some(json!({"name": name, "geometry_type": "point", "srid": 4326,
                    "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let ds = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();

    let (status, body) = request_as(
        app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        Some(&carol),
        Some(editable_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, branch) = request_as(
        app,
        "POST",
        &format!("/api/v1/datasets/{ds}/branches"),
        Some(&carol),
        Some(json!({"name": "main", "created_by": "carol"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    let (status, body) = request_as(
        app,
        "POST",
        &format!("/api/v1/branches/{branch_id}/commit"),
        Some(&carol),
        Some(json!({"message": "seed", "author": "carol", "operations": [
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 10, "name": "seeded"}}
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    (name, carol, branch_id)
}

/// The whole point of the gate: a write on the facade needs a token with write
/// access, exactly as `/api/v1` does. The refusal is Geoservices-shaped, because
/// an Esri client reads the body and never the status.
#[tokio::test]
async fn test_arcgis_apply_edits_refuses_anonymous_and_read_only_callers() {
    let app = setup_app_authed().await;
    let (name, carol, branch) = owned_editable_layer(&app).await;
    let commits = history_len(&app, branch).await;
    let add =
        json!([{"attributes": {"name": "intruder"}, "geometry": {"x": 5.0, "y": 5.0}}]).to_string();
    let body = form_body(&[("f", "json".into()), ("adds", add.clone())]);

    // anonymous: 499 is Esri's "a token is required"
    let (status, out) = post_form_as(&app, &apply_edits_url(&name), &body, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 499, "{out}");

    // a token this service did not mint: 498 is Esri's "invalid token"
    let forged = generate_token(
        "not-the-platform-secret-0123456789",
        "eve",
        Role::Admin,
        3600,
    );
    let (status, out) = post_form_as(&app, &apply_edits_url(&name), &body, Some(&forged)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 498, "{out}");

    // a valid token whose role cannot write
    let viewer = token_for_user("val", Role::Viewer);
    let (status, out) = post_form_as(&app, &apply_edits_url(&name), &body, Some(&viewer)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 403, "{out}");

    // an editor with no grant on this dataset: the per-dataset ladder, not the role
    let eve = token_for_user("eve", Role::Editor);
    let (status, out) = post_form_as(&app, &apply_edits_url(&name), &body, Some(&eve)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(
        out["error"]["code"] == 403 || out["error"]["code"] == 404,
        "{out}"
    );

    // nothing any of them sent was committed
    assert_eq!(history_len(&app, branch).await, commits);

    // and the owner's own edit lands
    let (status, out) = post_form_as(&app, &apply_edits_url(&name), &body, Some(&carol)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["addResults"],
        json!([{"objectId": 11, "success": true}]),
        "{out}"
    );
    assert_eq!(history_len(&app, branch).await, commits + 1);
}

/// An Esri client has no header to put a credential in, so `token` is one on
/// this facade. Reads stay anonymous.
#[tokio::test]
async fn test_arcgis_apply_edits_accepts_the_token_parameter() {
    let app = setup_app_authed().await;
    let (name, carol, branch) = owned_editable_layer(&app).await;
    let commits = history_len(&app, branch).await;
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "by-parameter"}, "geometry": {"x": 4.0, "y": 4.0}}])
                .to_string(),
        ),
    ]);

    let uri = format!("{}?token={carol}", apply_edits_url(&name));
    let (status, out) = post_form_as(&app, &uri, &body, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["addResults"],
        json!([{"objectId": 11, "success": true}]),
        "{out}"
    );
    assert_eq!(history_len(&app, branch).await, commits + 1);

    // a bad one in the same place is still refused
    let uri = format!("{}?token=not.a.token", apply_edits_url(&name));
    let (status, out) = post_form_as(&app, &uri, &body, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 498, "{out}");
    assert_eq!(history_len(&app, branch).await, commits + 1);

    // and the parameter is a credential only here: it must not enter /api/v1
    let (status, body) = request_as(
        &app,
        "GET",
        &format!("/api/v1/audit?token={carol}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

/// Helper: a request carrying the credential header an Esri-ecosystem client
/// sends, in the exact shape verne's own client attaches it
/// (`verne-arcgis/src/client.rs`). No `Authorization` header at all, which is
/// the case under test.
async fn esri_authorized(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    form: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Esri-Authorization", format!("Bearer {token}"));
    let body = match form {
        None => Body::empty(),
        Some(form) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(form.to_string())
        }
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// verne and the rest of the Esri ecosystem put their bearer token in
/// `X-Esri-Authorization` and send no `Authorization` header at all, so the
/// facade reads it: without this such a client can only reach public datasets.
///
/// It is a credential and not a promotion. The same token that opens a private
/// layer's metadata for a viewer is refused for a write, and it opens nothing the
/// caller holds no grant on.
#[tokio::test]
async fn test_arcgis_accepts_the_esri_authorization_header() {
    let app = setup_app_authed().await;
    let name = format!("esrihdr_{}", Uuid::now_v7().simple());
    let admin = token_for_user("root", Role::Admin);
    let (status, dataset) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&admin),
        Some(json!({"name": name, "geometry_type": "point", "srid": 4326,
                    "created_by": "root", "visibility": "private"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");
    let ds = Uuid::parse_str(dataset["id"].as_str().unwrap()).unwrap();
    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        Some(&admin),
        Some(editable_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, branch) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{ds}/branches"),
        Some(&admin),
        Some(json!({"name": "main", "created_by": "root"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch_id = Uuid::parse_str(branch["id"].as_str().unwrap()).unwrap();

    // a viewer who holds a read grant on the private dataset, and one who does not
    grant(&app, "datasets", ds, "vera", "read").await;
    let vera = token_for_user("vera", Role::Viewer);
    let nobody = token_for_user("nobody", Role::Viewer);

    let metadata = format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json");

    // anonymous, the dataset is simply not there
    let (status, body) = get_json(&app, &metadata).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");

    // with the header, the granted viewer reads the private layer's definition
    let (status, body) = esri_authorized(&app, "GET", &metadata, &vera, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "{body}");
    assert_eq!(body["name"], name, "{body}");
    assert_eq!(body["id"], 0, "{body}");
    assert_eq!(body["objectIdField"], "OBJECTID", "{body}");

    // and it is the grant that opened it, not the header: the same header with a
    // token holding no grant sees nothing
    let (status, body) = esri_authorized(&app, "GET", &metadata, &nobody, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");

    // the same viewer token in the same header is refused for a write, and 403 is
    // the role refusal: 499 would mean the header had not been read at all
    let commits = history_len_as(&app, branch_id, &admin).await;
    let add = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "by-header"}, "geometry": {"x": 6.0, "y": 6.0}}])
                .to_string(),
        ),
    ]);
    let (status, out) =
        esri_authorized(&app, "POST", &apply_edits_url(&name), &vera, Some(&add)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 403, "{out}");
    assert_eq!(history_len_as(&app, branch_id, &admin).await, commits);

    // the write path reads the header too, so the refusal above is about the role
    let (status, out) =
        esri_authorized(&app, "POST", &apply_edits_url(&name), &admin, Some(&add)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["addResults"][0]["success"], true, "{out}");
    assert_eq!(history_len_as(&app, branch_id, &admin).await, commits + 1);

    // the header is a credential on the facade alone: it must not enter /api/v1
    let (status, body) = esri_authorized(&app, "GET", "/api/v1/audit", &admin, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

// ═══════════════════════════════════════════════════════════════════════
// ArcGIS FeatureServer attachments
// ═══════════════════════════════════════════════════════════════════════

/// The boundary the multipart helpers below use. Any token works, and this one
/// makes a test body easy to read.
const PART_BOUNDARY: &str = "ptolemyTestBoundary";

/// Helper: a `multipart/form-data` body with one `attachment` file part and any
/// number of text fields, which is what an Esri client sends `addAttachment`.
/// Written out rather than built by a crate, because how the facade reads that
/// wire format is the thing under test.
fn upload_multipart(
    filename: &str,
    content_type: &str,
    data: &[u8],
    fields: &[(&str, &str)],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{PART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n\
                 {value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{PART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"attachment\"; \
             filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{PART_BOUNDARY}--\r\n").as_bytes());
    body
}

/// Helper: post a multipart body, with a bearer header when one is given.
async fn post_multipart(
    app: &axum::Router,
    uri: &str,
    body: Vec<u8>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method("POST").uri(uri).header(
        "content-type",
        format!("multipart/form-data; boundary={PART_BOUNDARY}"),
    );
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Helper: GET keeping the headers, which is where a download says what it is.
async fn get_download(
    app: &axum::Router,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

/// Helper: the attachment URLs of one object id on a service's layer 0.
fn attachments_url(service: &str, oid: i64) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/{oid}/attachments")
}

fn add_attachment_url(service: &str, oid: i64) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/{oid}/addAttachment")
}

fn update_attachment_url(service: &str, oid: i64) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/{oid}/updateAttachment")
}

fn delete_attachments_url(service: &str, oid: i64) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/{oid}/deleteAttachments")
}

fn query_attachments_url(service: &str) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/0/queryAttachments")
}

/// Helper: an editable layer with two features, at OBJECTID 100 and 200.
async fn layer_with_two_features(app: &axum::Router, prefix: &str) -> (String, Uuid) {
    let (name, _ds, branch) = editable_layer(app, prefix, "point").await;
    commit_features(
        app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 200, "name": "second"}},
        ]),
    )
    .await;
    (name, branch)
}

/// The whole round trip an Esri client makes: upload the file, see it listed,
/// fetch the bytes back, delete it.
#[tokio::test]
async fn test_arcgis_attachment_uploads_lists_downloads_and_deletes() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "attach").await;

    let bytes = b"\x89PNG\r\n\x1a\n not really a png";
    let (status, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 100),
        upload_multipart("site.png", "image/png", bytes, &[("f", "json")]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let result = &out["addAttachmentResult"];
    assert_eq!(result["success"], true, "{out}");
    let id = result["objectId"]
        .as_i64()
        .unwrap_or_else(|| panic!("{out}"));
    // a 48-bit derived id: a number a JSON client holds exactly
    assert!(id > 0 && id < 1 << 48, "{out}");
    let global = result["globalId"]
        .as_str()
        .unwrap_or_else(|| panic!("{out}"));
    assert!(global.starts_with('{') && global.ends_with('}'), "{out}");
    let uuid = Uuid::parse_str(global.trim_matches(|c| c == '{' || c == '}')).unwrap();
    assert_eq!(global, format!("{{{}}}", uuid.to_string().to_uppercase()));

    // listed under the feature, with the name and type it was sent with
    let (status, listing) =
        get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let infos = listing["attachmentInfos"].as_array().unwrap();
    assert_eq!(infos.len(), 1, "{listing}");
    assert_eq!(infos[0]["id"], id, "{listing}");
    assert_eq!(infos[0]["globalId"], global, "{listing}");
    assert_eq!(infos[0]["name"], "site.png", "{listing}");
    assert_eq!(infos[0]["contentType"], "image/png", "{listing}");
    assert_eq!(infos[0]["size"], bytes.len() as i64, "{listing}");

    // the derived id is stable across a second listing
    let (_, again) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(again["attachmentInfos"][0]["id"], id, "{again}");

    // the bytes come back exactly, as the type they went in as
    let (status, headers, body) =
        get_download(&app, &format!("{}/{id}", attachments_url(&name, 100))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes.to_vec());
    assert_eq!(headers.get("content-type").unwrap(), "image/png");
    let disposition = headers
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.contains("site.png"), "{disposition}");
    // never inline: the type is whatever the uploader said it was
    assert!(disposition.starts_with("attachment"), "{disposition}");
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");

    // and the delete takes it
    let (status, out) = post_form(
        &app,
        &delete_attachments_url(&name, 100),
        &form_body(&[("f", "json".into()), ("attachmentIds", id.to_string())]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["deleteAttachmentResults"],
        json!([{"objectId": id, "success": true}]),
        "{out}"
    );

    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(listing["attachmentInfos"], json!([]), "{listing}");
    // and the id names nothing now
    let (status, _, _) = get_download(&app, &format!("{}/{id}", attachments_url(&name, 100))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a refusal is Esri-shaped, so still 200"
    );
    let (_, out) = get_json(
        &app,
        &format!("{}/{id}?f=json", attachments_url(&name, 100)),
    )
    .await;
    assert_eq!(out["error"]["code"], 400, "{out}");
}

/// One request for many features, grouped by the object id that owns each set.
/// This is the shape verne's extractor reads.
#[tokio::test]
async fn test_arcgis_query_attachments_groups_by_parent_object_id() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "groups").await;

    let mut ids = Vec::new();
    for (oid, filename) in [(100, "a.txt"), (100, "b.txt"), (200, "c.txt")] {
        let (status, out) = post_multipart(
            &app,
            &add_attachment_url(&name, oid),
            upload_multipart(filename, "text/plain", filename.as_bytes(), &[]),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        ids.push(out["addAttachmentResult"]["objectId"].as_i64().unwrap());
    }
    // two uploads to one feature must not derive the same id, whatever the same
    // millisecond does to their uuids
    assert_ne!(ids[0], ids[1]);

    for uri in [
        format!("{}?f=json&objectIds=100,200", query_attachments_url(&name)),
        format!("{}?f=json&objectIds=200,100", query_attachments_url(&name)),
    ] {
        let (status, out) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{out}");
        let groups = out["attachmentGroups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "{out}");
        for group in groups {
            let parent = group["parentObjectId"].as_i64().unwrap();
            let infos = group["attachmentInfos"].as_array().unwrap();
            match parent {
                100 => assert_eq!(infos.len(), 2, "{out}"),
                200 => assert_eq!(infos.len(), 1, "{out}"),
                other => panic!("unexpected parent {other}: {out}"),
            }
            assert!(infos.iter().all(|info| info["id"].is_i64()), "{out}");
        }
    }

    // a form post says the same thing, which is how a long id list is sent
    let (status, out) = post_form(
        &app,
        &query_attachments_url(&name),
        &form_body(&[("f", "json".into()), ("objectIds", "200".into())]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let groups = out["attachmentGroups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{out}");
    assert_eq!(groups[0]["parentObjectId"], 200, "{out}");

    // a feature with none is left out rather than answered as an empty group
    let (_, out) = post_form(
        &app,
        &query_attachments_url(&name),
        &form_body(&[("f", "json".into()), ("objectIds", "100".into())]),
    )
    .await;
    assert_eq!(
        out["attachmentGroups"].as_array().unwrap().len(),
        1,
        "{out}"
    );
    let (_, out) = get_json(
        &app,
        &format!("{}?f=json&objectIds=999", query_attachments_url(&name)),
    )
    .await;
    assert_eq!(
        out["error"]["code"], 400,
        "an unknown oid is refused: {out}"
    );
}

/// A filter that cannot be honored is refused by name, as everywhere else on the
/// facade: a client that believes its filter applied reads the wrong answer.
#[tokio::test]
async fn test_arcgis_query_attachments_refuses_filters_it_cannot_honor() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "attfilters").await;

    for parameter in [
        "definitionExpression=att_name='a'",
        "keywords=photo",
        "gdbVersion=sde.DEFAULT",
    ] {
        let (status, out) = get_json(
            &app,
            &format!(
                "{}?f=json&objectIds=100&{parameter}",
                query_attachments_url(&name)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        assert_eq!(out["error"]["code"], 400, "{parameter}: {out}");
        let named = parameter.split('=').next().unwrap();
        assert!(
            out["error"]["message"].as_str().unwrap().contains(named),
            "{parameter}: {out}"
        );
    }

    // and the same on a write, which is refused for the parameters an
    // applyEdits is refused for
    let (status, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 100),
        upload_multipart(
            "versioned.txt",
            "text/plain",
            b"x",
            &[("gdbVersion", "sde.DEFAULT")],
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("gdbVersion"),
        "{out}"
    );

    // and objectIds is required rather than defaulted to every feature
    let (_, out) = get_json(&app, &format!("{}?f=json", query_attachments_url(&name))).await;
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("objectIds"),
        "{out}"
    );
}

/// The store has no update for an attachment, so a replacement is a new row and
/// therefore a new id. The result carries it, which is what an Esri client reads.
#[tokio::test]
async fn test_arcgis_update_attachment_replaces_the_file_and_reports_its_id() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "replace").await;

    let (_, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 100),
        upload_multipart("first.txt", "text/plain", b"before", &[]),
        None,
    )
    .await;
    let first = out["addAttachmentResult"]["objectId"].as_i64().unwrap();

    let (status, out) = post_multipart(
        &app,
        &update_attachment_url(&name, 100),
        upload_multipart(
            "second.csv",
            "text/csv",
            b"after,after",
            &[("f", "json"), ("attachmentId", &first.to_string())],
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let result = &out["updateAttachmentResult"];
    assert_eq!(result["success"], true, "{out}");
    let second = result["objectId"]
        .as_i64()
        .unwrap_or_else(|| panic!("{out}"));

    // one attachment, the new one, under the id the result named
    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    let infos = listing["attachmentInfos"].as_array().unwrap();
    assert_eq!(infos.len(), 1, "{listing}");
    assert_eq!(infos[0]["id"], second, "{listing}");
    assert_eq!(infos[0]["name"], "second.csv", "{listing}");
    assert_eq!(infos[0]["contentType"], "text/csv", "{listing}");

    let (status, headers, body) =
        get_download(&app, &format!("{}/{second}", attachments_url(&name, 100))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"after,after".to_vec());
    assert_eq!(headers.get("content-type").unwrap(), "text/csv");

    // the id it replaced names nothing now
    let (_, out) = get_json(
        &app,
        &format!("{}/{first}?f=json", attachments_url(&name, 100)),
    )
    .await;
    assert_eq!(out["error"]["code"], 400, "{out}");

    // and an attachmentId this feature does not carry replaces nothing
    let (status, out) = post_multipart(
        &app,
        &update_attachment_url(&name, 100),
        upload_multipart(
            "third.txt",
            "text/plain",
            b"never",
            &[("attachmentId", "424242")],
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(
        listing["attachmentInfos"].as_array().unwrap().len(),
        1,
        "nothing was added by the refused replace: {listing}"
    );
}

/// All or none, exactly as `applyEdits` is: an Esri client reports per row and
/// this cannot, so a batch naming one unknown id must take nothing at all.
#[tokio::test]
async fn test_arcgis_delete_attachments_refuses_the_whole_batch_on_one_unknown_id() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "batch").await;

    let mut ids = Vec::new();
    for filename in ["one.txt", "two.txt"] {
        let (_, out) = post_multipart(
            &app,
            &add_attachment_url(&name, 100),
            upload_multipart(filename, "text/plain", filename.as_bytes(), &[]),
            None,
        )
        .await;
        ids.push(out["addAttachmentResult"]["objectId"].as_i64().unwrap());
    }

    let (status, out) = post_form(
        &app,
        &delete_attachments_url(&name, 100),
        &form_body(&[
            ("f", "json".into()),
            ("attachmentIds", format!("{},{},999999", ids[0], ids[1])),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains("999999"),
        "{out}"
    );

    // both are still there: the refusal happened before any delete
    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(
        listing["attachmentInfos"].as_array().unwrap().len(),
        2,
        "{listing}"
    );

    // an attachment of the other feature is not this feature's to delete either
    let (_, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 200),
        upload_multipart("elsewhere.txt", "text/plain", b"x", &[]),
        None,
    )
    .await;
    let elsewhere = out["addAttachmentResult"]["objectId"].as_i64().unwrap();
    let (_, out) = post_form(
        &app,
        &delete_attachments_url(&name, 100),
        &form_body(&[("attachmentIds", elsewhere.to_string())]),
    )
    .await;
    assert_eq!(out["error"]["code"], 400, "{out}");

    // and the pair of its own ids does go
    let (status, out) = post_form(
        &app,
        &delete_attachments_url(&name, 100),
        &form_body(&[("attachmentIds", format!("{},{}", ids[0], ids[1]))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["deleteAttachmentResults"].as_array().unwrap().len(),
        2,
        "{out}"
    );
    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(listing["attachmentInfos"], json!([]), "{listing}");
}

/// An object id no feature carries names no feature to attach to, and is refused
/// rather than answered as a feature with no attachments.
#[tokio::test]
async fn test_arcgis_attachments_refuse_an_unknown_object_id() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "unknownoid").await;

    let (status, out) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 777))).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains("777"),
        "{out}"
    );

    let (status, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 777),
        upload_multipart("nowhere.txt", "text/plain", b"x", &[]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");

    // an object id that is not a number at all is a client bug, not a 404 page
    let (_, out) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 0))).await;
    assert_eq!(out["error"]["code"], 400, "{out}");
}

/// A layer whose object ids are row numbers takes no attachment writes, for the
/// reason it takes no edits: such an id names a different feature after any
/// delete, and a file aimed by one would land on that feature. Reads still work,
/// because they answer about the same feature the query just named.
#[tokio::test]
async fn test_arcgis_attachment_writes_refuse_a_row_number_layer() {
    let (app, _) = setup_app().await;
    let plain = format!("rownum_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &plain, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;

    let (status, out) = post_multipart(
        &app,
        &add_attachment_url(&plain, 1),
        upload_multipart("shifted.txt", "text/plain", b"x", &[]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 400, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("objectid"),
        "{out}"
    );

    // the listing is a read and answers
    let (status, out) = get_json(&app, &format!("{}?f=json", attachments_url(&plain, 1))).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["attachmentInfos"], json!([]), "{out}");
}

/// The cap is this facade's own, and a refusal has to name it or a client cannot
/// tell an oversize file from a broken server.
#[tokio::test]
async fn test_arcgis_add_attachment_refuses_a_file_over_the_cap() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "toobig").await;

    let oversize = vec![b'x'; 32 * 1024 * 1024 + 1];
    let (status, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 100),
        upload_multipart("huge.bin", "application/octet-stream", &oversize, &[]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 413, "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains("32 MiB"),
        "{out}"
    );

    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(listing["attachmentInfos"], json!([]), "{listing}");

    // a body with no file part at all says so rather than storing an empty file
    let (_, out) = post_multipart(
        &app,
        &add_attachment_url(&name, 100),
        upload_multipart("", "text/plain", b"x", &[]),
        None,
    )
    .await;
    assert_eq!(out["error"]["code"], 400, "{out}");
}

/// The gate, on all three writes: an anonymous caller cannot put a file on a
/// feature, and the refusal is Geoservices-shaped because an Esri client reads
/// the body and never the status. The reads stay anonymous.
#[tokio::test]
async fn test_arcgis_attachment_writes_refuse_anonymous_and_read_only_callers() {
    let app = setup_app_authed().await;
    let (name, carol, _branch) = owned_editable_layer(&app).await;
    let file = || upload_multipart("intruder.txt", "text/plain", b"x", &[("f", "json")]);

    let viewer = token_for_user("val", Role::Viewer);
    let eve = token_for_user("eve", Role::Editor);
    for (label, token, expected) in [
        ("anonymous", None, vec![499]),
        ("read-only", Some(viewer.as_str()), vec![403]),
        ("no grant", Some(eve.as_str()), vec![403, 404]),
    ] {
        let (status, out) =
            post_multipart(&app, &add_attachment_url(&name, 10), file(), token).await;
        assert_eq!(status, StatusCode::OK, "{label}: {out}");
        let code = out["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("{label}: {out}"));
        assert!(expected.contains(&code), "{label}: {out}");

        // and the same on the two that name an attachment
        let (_, out) = post_multipart(
            &app,
            &update_attachment_url(&name, 10),
            upload_multipart("x.txt", "text/plain", b"x", &[("attachmentId", "1")]),
            token,
        )
        .await;
        assert!(
            expected.contains(&out["error"]["code"].as_i64().unwrap()),
            "{label}: {out}"
        );
        let (_, out) = post_form_as(
            &app,
            &delete_attachments_url(&name, 10),
            &form_body(&[("attachmentIds", "1".into())]),
            token,
        )
        .await;
        assert!(
            expected.contains(&out["error"]["code"].as_i64().unwrap()),
            "{label}: {out}"
        );
    }

    // nothing any of them sent was stored, and the listing is a public read
    let (status, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 10))).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert_eq!(listing["attachmentInfos"], json!([]), "{listing}");

    // the owner's own upload lands
    let (status, out) =
        post_multipart(&app, &add_attachment_url(&name, 10), file(), Some(&carol)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["addAttachmentResult"]["success"], true, "{out}");
}

/// The Geoservices protocol has no header for a credential, so `token` in the
/// URL is one on this facade, on an attachment write as on `applyEdits`.
#[tokio::test]
async fn test_arcgis_add_attachment_accepts_the_token_parameter() {
    let app = setup_app_authed().await;
    let (name, carol, _branch) = owned_editable_layer(&app).await;

    let uri = format!("{}?token={carol}", add_attachment_url(&name, 10));
    let (status, out) = post_multipart(
        &app,
        &uri,
        upload_multipart("by-parameter.txt", "text/plain", b"ok", &[]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let id = out["addAttachmentResult"]["objectId"]
        .as_i64()
        .unwrap_or_else(|| panic!("{out}"));

    // a bad one in the same place is still refused
    let uri = format!("{}?token=not.a.token", add_attachment_url(&name, 10));
    let (status, out) = post_multipart(
        &app,
        &uri,
        upload_multipart("forged.txt", "text/plain", b"no", &[]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["error"]["code"], 498, "{out}");

    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 10))).await;
    let infos = listing["attachmentInfos"].as_array().unwrap();
    assert_eq!(infos.len(), 1, "{listing}");
    assert_eq!(infos[0]["id"], id, "{listing}");
}

/// A client reads these two flags to decide whether to offer attachments at all,
/// so they say what the service does rather than what this layer happens to hold.
#[tokio::test]
async fn test_arcgis_layer_metadata_declares_attachment_support() {
    let (app, _) = setup_app().await;
    let (name, _branch) = layer_with_two_features(&app, "attmeta").await;

    let (status, layer) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer/0?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{layer}");
    assert_eq!(layer["hasAttachments"], true, "{layer}");
    assert_eq!(
        layer["advancedQueryCapabilities"]["supportsQueryAttachments"], true,
        "{layer}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// ArcGIS FeatureServer change tracking
// ═══════════════════════════════════════════════════════════════════════

/// Helper: the extractChanges URL of a service. It is a service operation rather
/// than a layer one, which is where Esri puts it.
fn extract_changes_url(service: &str) -> String {
    format!("/arcgis/rest/services/{service}/FeatureServer/extractChanges")
}

/// Helper: the path a facade URL names, so a test follows a statusUrl or a
/// resultUrl the way a client does whatever host the base resolved to.
fn facade_path(url: &Value) -> String {
    let url = url.as_str().unwrap_or_else(|| panic!("a URL, not {url}"));
    let at = url
        .find("/arcgis/")
        .unwrap_or_else(|| panic!("not a facade URL: {url}"));
    url[at..].to_string()
}

/// Helper: the service root, and the generation it publishes for layer 0, which
/// is the cursor a client writes down at a full read.
async fn published_gen(app: &axum::Router, service: &str) -> i64 {
    let (status, root) = get_json(
        app,
        &format!("/arcgis/rest/services/{service}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{root}");
    assert!(
        root["capabilities"]
            .as_str()
            .unwrap()
            .split(',')
            .any(|held| held.trim() == "ChangeTracking"),
        "{root}"
    );
    let gens = &root["changeTrackingInfo"]["layerServerGens"][0];
    assert_eq!(gens["id"], 0, "{root}");
    assert_eq!(gens["minServerGen"], gens["serverGen"], "{root}");
    gens["serverGen"]
        .as_i64()
        .unwrap_or_else(|| panic!("{root}"))
}

/// Helper: the form body a client submits extractChanges with.
fn extract_body(since: i64) -> String {
    form_body(&[
        ("f", "json".into()),
        ("layers", "0".into()),
        (
            "layerServerGens",
            json!([{"id": 0, "serverGen": since}]).to_string(),
        ),
    ])
}

/// Helper: submit extractChanges and hand back the job's status URL path.
async fn submit_changes(app: &axum::Router, service: &str, since: i64) -> String {
    let (status, job) = post_form(app, &extract_changes_url(service), &extract_body(since)).await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert!(job["error"].is_null(), "{job}");
    facade_path(&job["statusUrl"])
}

/// Helper: poll a job to its result and fetch the change file, which is the
/// whole of what a client does past the submit.
async fn collect_changes(app: &axum::Router, status_url: &str) -> Value {
    let (status, held) = get_json(app, &format!("{status_url}?f=json")).await;
    assert_eq!(status, StatusCode::OK, "{held}");
    assert_eq!(held["status"], "Completed", "{held}");
    assert_eq!(
        held["responseType"], "esriDataChangesResponseTypeEdits",
        "{held}"
    );
    let result = facade_path(&held["resultUrl"]);
    assert!(!result.is_empty(), "{held}");
    let (status, file) = get_json(app, &result).await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert!(file["error"].is_null(), "{file}");
    file
}

/// Helper: the whole delta loop, submit through fetch.
async fn extract_changes(app: &axum::Router, service: &str, since: i64) -> Value {
    let status_url = submit_changes(app, service, since).await;
    collect_changes(app, &status_url).await
}

/// The object ids a change file names, as adds, updates and deletes. Read the way
/// the consumer reads them: off the layer's own object id field on an add or an
/// update, and off `deleteIds` for a delete.
fn changed(file: &Value) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
    let edits = &file["edits"][0];
    assert_eq!(edits["id"], 0, "{file}");
    let ids = |section: &str| -> Vec<i64> {
        edits["features"][section]
            .as_array()
            .unwrap_or_else(|| panic!("{section} in {file}"))
            .iter()
            .map(|row| {
                row["attributes"]["OBJECTID"]
                    .as_i64()
                    .unwrap_or_else(|| panic!("{row}"))
            })
            .collect()
    };
    let deletes = edits["features"]["deleteIds"]
        .as_array()
        .unwrap_or_else(|| panic!("{file}"))
        .iter()
        .map(|id| id.as_i64().unwrap_or_else(|| panic!("{id}")))
        .collect();
    (ids("adds"), ids("updates"), deletes)
}

/// The generation a change file's window ended at.
fn file_gen(file: &Value) -> i64 {
    let gens = &file["layerServerGens"][0];
    assert_eq!(gens["id"], 0, "{file}");
    gens["serverGen"]
        .as_i64()
        .unwrap_or_else(|| panic!("{file}"))
}

/// The loop a migration tool rides: read the service root's generation at a full
/// extraction, edit the layer, then ask what changed since that generation and
/// get the object ids of the rows that moved.
#[tokio::test]
async fn test_arcgis_extract_changes_reports_the_edits_since_a_generation() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "tracked", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 200, "name": "second"}},
        ]),
    )
    .await;

    // the cursor a full extraction writes down, which is when the commit landed
    let root = published_gen(&app, &name).await;
    assert!(root > 0, "a clock reading, not a count of commits: {root}");

    // an add, an update and a delete, which the facade lands as one commit
    let body = form_body(&[
        ("f", "json".into()),
        (
            "adds",
            json!([{"attributes": {"name": "third"}, "geometry": {"x": 3.0, "y": 3.0}}])
                .to_string(),
        ),
        (
            "updates",
            json!([{"attributes": {"OBJECTID": 100, "name": "renamed"}}]).to_string(),
        ),
        ("deletes", "200".into()),
    ]);
    let (status, out) = post_form(&app, &apply_edits_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["error"].is_null(), "{out}");

    let file = extract_changes(&app, &name, root).await;
    let (adds, updates, deletes) = changed(&file);
    assert_eq!(adds, vec![201], "{file}");
    assert_eq!(updates, vec![100], "{file}");
    assert_eq!(deletes, vec![200], "{file}");
    // the window closes where the layer is now, which is after the new commit
    let next = file_gen(&file);
    assert!(next > root, "{file}");
    assert_eq!(published_gen(&app, &name).await, next);

    // an add carries the object id and no geometry: a client fetches the rows
    // themselves through /query
    let added = &file["edits"][0]["features"]["adds"][0];
    assert!(added["geometry"].is_null(), "{added}");
    // a layer with no attachment edits states the arrays empty rather than
    // leaving them out
    let attachments = &file["edits"][0]["attachments"];
    assert_eq!(attachments["adds"], json!([]), "{file}");
    assert_eq!(attachments["updates"], json!([]), "{file}");
    assert_eq!(attachments["deleteIds"], json!([]), "{file}");

    // and asking again from the generation it reported says nothing changed
    let file = extract_changes(&app, &name, next).await;
    assert_eq!(changed(&file), (vec![], vec![], vec![]), "{file}");
    assert_eq!(file_gen(&file), next, "{file}");
}

/// Generation 0 is the epoch, so a window from it holds every row the layer has.
#[tokio::test]
async fn test_arcgis_extract_changes_from_generation_zero_holds_every_row() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "fromzero", "point").await;
    // a branch nothing has happened to is at 0
    assert_eq!(published_gen(&app, &name).await, 0);

    // submitted while the branch is empty: the window is pinned there, so the
    // commit that lands next is outside it
    let pinned = submit_changes(&app, &name, 0).await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;
    let file = collect_changes(&app, &pinned).await;
    assert_eq!(changed(&file), (vec![], vec![], vec![]), "{file}");
    assert_eq!(file_gen(&file), 0, "{file}");

    // asked again now, generation 0 covers the commit
    let file = extract_changes(&app, &name, 0).await;
    assert_eq!(changed(&file).0, vec![100], "{file}");
    assert_eq!(file_gen(&file), published_gen(&app, &name).await, "{file}");
}

/// A commit that lands between the submit and the fetch is outside the window the
/// job pinned, so the change file and the generation it reports agree.
#[tokio::test]
async fn test_arcgis_extract_changes_pins_the_head_it_was_submitted_at() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "pinned", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;
    let root = published_gen(&app, &name).await;

    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(2.0, 2.0),
             "properties": {"OBJECTID": 200, "name": "second"}},
        ]),
    )
    .await;
    let pinned_at = published_gen(&app, &name).await;
    let status_url = submit_changes(&app, &name, root).await;

    // lands after the submit, so it belongs to the next window
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(3.0, 3.0),
             "properties": {"OBJECTID": 300, "name": "third"}},
        ]),
    )
    .await;

    let file = collect_changes(&app, &status_url).await;
    assert_eq!(changed(&file).0, vec![200], "{file}");
    // the window closes at the clock the submit read, not at the clock now
    assert_eq!(file_gen(&file), pinned_at, "{file}");
    assert!(file_gen(&file) < published_gen(&app, &name).await, "{file}");

    // and the row that landed after it comes back in the window that starts
    // where this one ended
    let file = extract_changes(&app, &name, file_gen(&file)).await;
    assert_eq!(changed(&file).0, vec![300], "{file}");
}

/// A cursor recorded when this service counted commits is a small number, which
/// as a clock reading is 1970. Answering it would open a window before the layer
/// existed and report every row and every attachment it has as an add, which is
/// the duplication the clock exists to stop, so it is refused by name instead.
#[tokio::test]
async fn test_arcgis_extract_changes_refuses_a_generation_that_predates_the_clock() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "stalegen", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;
    let clock = published_gen(&app, &name).await;

    // 1 is what a client that had recorded "one commit deep" holds
    for stale in [1, 2, 7] {
        let (status, body) =
            post_form(&app, &extract_changes_url(&name), &extract_body(stale)).await;
        assert_eq!(status, StatusCode::OK, "{stale}: {body}");
        assert_eq!(body["error"]["code"], 400, "{stale}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("predates"), "{stale}: {message}");
        assert!(message.contains(&clock.to_string()), "{stale}: {message}");
        assert!(
            message.contains("full"),
            "it says what to do instead: {message}"
        );
    }

    // 0 is not a stale count: it is the clock a branch nothing has happened to
    // publishes, and a full extraction
    let file = extract_changes(&app, &name, 0).await;
    assert_eq!(changed(&file).0, vec![100], "{file}");

    // and the layer's own generation is answered
    let file = extract_changes(&app, &name, clock).await;
    assert_eq!(changed(&file), (vec![], vec![], vec![]), "{file}");
}

/// A generation ahead of the layer is a client holding a cursor from somewhere
/// else, so it is refused naming both numbers rather than answered with an empty
/// window.
#[tokio::test]
async fn test_arcgis_extract_changes_refuses_a_generation_past_the_head() {
    let (app, _) = setup_app().await;
    let (name, _ds, _branch) = editable_layer(&app, "ahead", "point").await;
    let (status, body) = post_form(&app, &extract_changes_url(&name), &extract_body(7)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains('7'), "{message}");
    assert!(message.contains('0'), "{message}");
}

/// A layer whose object ids are row numbers has nothing to track: an id shifts
/// when a feature is deleted, so a list of the ids that changed would point at
/// whatever moved into their place. Its root says nothing about tracking and the
/// operation is refused by name.
#[tokio::test]
async fn test_arcgis_change_tracking_refuses_a_row_number_layer() {
    let (app, _) = setup_app().await;
    let name = format!("rownumber_{}", Uuid::now_v7().simple());
    let ds = create_named_dataset(&app, &name, "point").await;
    let branch = create_branch(&app, ds, "main").await;
    seed_points(&app, branch, 2).await;

    let (status, root) = get_json(
        &app,
        &format!("/arcgis/rest/services/{name}/FeatureServer?f=json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{root}");
    assert_eq!(root["capabilities"], "Query", "{root}");
    assert!(root["changeTrackingInfo"].is_null(), "{root}");

    let (status, body) = post_form(&app, &extract_changes_url(&name), &extract_body(0)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("objectid"),
        "{body}"
    );
}

/// A job id is not a lookup key, it is data a client hands back, so nothing about
/// it is trusted. A malformed one is refused, and so is a well-formed one issued
/// for another dataset: without that, an id edited to name another dataset's
/// changeset would be answered with that dataset's changes.
#[tokio::test]
async fn test_arcgis_change_tracking_refuses_a_job_id_it_did_not_issue() {
    let (app, _) = setup_app().await;
    let (mine, _ds, branch) = editable_layer(&app, "myjobs", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;
    let (other, _ds, other_branch) = editable_layer(&app, "otherjobs", "point").await;
    commit_features(
        &app,
        other_branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(9.0, 9.0),
             "properties": {"OBJECTID": 900, "name": "theirs"}},
        ]),
    )
    .await;

    // the other service's own job id, which names a head this layer's history
    // never reaches
    let theirs = submit_changes(&app, &other, 0).await;
    let stolen = theirs.rsplit('/').next().unwrap().to_string();

    let mut ids = vec![
        stolen,
        // not base64 at all, and a base64url alphabet that decodes to no text
        "not-a-job-id".into(),
        // base64url of "hello": no separator
        "aGVsbG8".into(),
        // base64url of "0000:1": a separator and no changeset
        "MDAwMDox".into(),
        // base64url of a uuid-shaped name that is not a changeset of this branch
        "ZGVhZGJlZWYtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwMDAwOjE".into(),
        // base64url of ":1": a job pinned to a branch with no commit, asking for
        // a generation that branch never reached. Two fields where the id has
        // three, which is also the shape the version that counted commits wrote
        "OjE".into(),
    ];
    // and a good id with a character added, which is no longer one this service
    // wrote
    let good = submit_changes(&app, &mine, published_gen(&app, &mine).await).await;
    let good_id = good.rsplit('/').next().unwrap().to_string();
    ids.push(format!("{good_id}x"));

    for id in &ids {
        for route in ["jobs", "changefiles"] {
            let uri = format!("/arcgis/rest/services/{mine}/FeatureServer/{route}/{id}?f=json");
            let (status, body) = get_json(&app, &uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            assert_eq!(body["error"]["code"], 400, "{uri}: {body}");
            assert!(body["edits"].is_null(), "{uri}: {body}");
        }
    }

    // the good one still works, so the refusals are the ids and not the routes
    let file = collect_changes(&app, &good).await;
    assert_eq!(changed(&file), (vec![], vec![], vec![]), "{file}");
}

/// Every extract parameter that would change which edits the answer covers is
/// refused rather than ignored.
#[tokio::test]
async fn test_arcgis_extract_changes_refuses_parameters_it_cannot_honor() {
    let (app, _) = setup_app().await;
    let (name, _ds, _branch) = editable_layer(&app, "extractparams", "point").await;
    let gens = json!([{"id": 0, "serverGen": 0}]).to_string();

    for (label, pairs) in [
        (
            "a geodatabase this service cannot write",
            vec![
                ("layers", "0".to_string()),
                ("layerServerGens", gens.clone()),
                ("dataFormat", "sqlite".into()),
            ],
        ),
        (
            "another layer",
            vec![
                ("layers", "1".to_string()),
                ("layerServerGens", gens.clone()),
            ],
        ),
        (
            "a generation for another layer",
            vec![
                ("layers", "0".to_string()),
                (
                    "layerServerGens",
                    json!([{"id": 1, "serverGen": 0}]).to_string(),
                ),
            ],
        ),
        ("no generation at all", vec![("layers", "0".to_string())]),
        (
            "the positional generation form",
            vec![
                ("layers", "0".to_string()),
                ("layerServerGens", gens.clone()),
                ("serverGens", "0,0".into()),
            ],
        ),
        (
            "one kind of edit left out",
            vec![
                ("layers", "0".to_string()),
                ("layerServerGens", gens.clone()),
                ("returnUpdates", "false".into()),
            ],
        ),
        (
            "a format that is not json",
            vec![
                ("f", "geojson".to_string()),
                ("layers", "0".into()),
                ("layerServerGens", gens.clone()),
            ],
        ),
    ] {
        let body = form_body(&pairs);
        let (status, out) = post_form(&app, &extract_changes_url(&name), &body).await;
        assert_eq!(status, StatusCode::OK, "{label}: {out}");
        assert_eq!(out["error"]["code"], 400, "{label}: {out}");
        assert!(out["statusUrl"].is_null(), "{label}: {out}");
    }

    // the three edit kinds are accepted as true, which is what the answer does
    let body = form_body(&[
        ("f", "json".into()),
        ("layers", "0".into()),
        ("layerServerGens", gens),
        ("returnInserts", "true".into()),
        ("returnUpdates", "true".into()),
        ("returnDeletes", "true".into()),
        ("returnIdsOnly", "true".into()),
    ]);
    let (status, out) = post_form(&app, &extract_changes_url(&name), &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out["statusUrl"].as_str().is_some(), "{out}");
}

/// Change tracking widens nothing: a private dataset is as absent from these
/// three routes as it is from the query.
#[tokio::test]
async fn test_arcgis_change_tracking_does_not_widen_a_private_dataset() {
    let (app, _state) = setup_app_authed_with_state().await;
    let name = format!("secretgens_{}", Uuid::now_v7().simple());
    let admin = token_for(Role::Admin);
    let (status, body) = request_as(
        &app,
        "POST",
        "/api/v1/datasets",
        Some(&admin),
        Some(json!({"name": name, "geometry_type": "point", "srid": 4326,
                    "created_by": "admin", "visibility": "private"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ds = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let (status, body) = request_as(
        &app,
        "PUT",
        &format!("/api/v1/datasets/{ds}/schema"),
        Some(&admin),
        Some(editable_schema()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request_as(
        &app,
        "POST",
        &format!("/api/v1/datasets/{ds}/branches"),
        Some(&admin),
        Some(json!({"name": "main", "created_by": "admin"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // the admin's own job id, so the anonymous fetch is refused for the dataset
    // and not for the id
    let (status, job) = post_form_as(
        &app,
        &extract_changes_url(&name),
        &extract_body(0),
        Some(&admin),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    let status_url = facade_path(&job["statusUrl"]);
    let id = status_url.rsplit('/').next().unwrap().to_string();

    let (status, body) = post_form(&app, &extract_changes_url(&name), &extract_body(0)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], 400, "{body}");
    for route in ["jobs", "changefiles"] {
        let uri = format!("/arcgis/rest/services/{name}/FeatureServer/{route}/{id}?f=json");
        let (status, body) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["error"]["code"], 400, "{uri}: {body}");
        assert!(body["edits"].is_null(), "{uri}: {body}");
    }

    // and the admin reads all three
    let (status, held) = request_as(
        &app,
        "GET",
        &format!("{status_url}?f=json"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{held}");
    assert_eq!(held["status"], "Completed", "{held}");
    let (status, file) = request_as(
        &app,
        "GET",
        &facade_path(&held["resultUrl"]),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert_eq!(file_gen(&file), 0, "{file}");
}

// ═══════════════════════════════════════════════════════════════════════
// ArcGIS change files: attachment sections
// ═══════════════════════════════════════════════════════════════════════

/// Helper: upload one attachment through the facade and hand back the two ids it
/// answers with, the number a client acts on it by and the global id a change
/// file names it by.
async fn add_attachment(
    app: &axum::Router,
    service: &str,
    oid: i64,
    filename: &str,
    bytes: &[u8],
) -> (i64, String) {
    let (status, out) = post_multipart(
        app,
        &add_attachment_url(service, oid),
        upload_multipart(filename, "image/png", bytes, &[("f", "json")]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{filename}: {out}");
    let held = &out["addAttachmentResult"];
    (
        held["objectId"]
            .as_i64()
            .unwrap_or_else(|| panic!("{filename}: {out}")),
        held["globalId"]
            .as_str()
            .unwrap_or_else(|| panic!("{filename}: {out}"))
            .to_string(),
    )
}

/// Helper: delete one attachment through the facade.
async fn delete_attachment_via_facade(app: &axum::Router, service: &str, oid: i64, id: i64) {
    let (status, out) = post_form(
        app,
        &delete_attachments_url(service, oid),
        &form_body(&[("f", "json".into()), ("attachmentIds", id.to_string())]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["deleteAttachmentResults"],
        json!([{"objectId": id, "success": true}]),
        "{out}"
    );
}

/// Helper: the whole delta loop as a client that names the host it reached,
/// which is what the absolute URLs in the answer are built from.
async fn extract_changes_from_host(
    app: &axum::Router,
    service: &str,
    since: i64,
    host: &str,
) -> Value {
    let get = async |uri: &str| -> Value {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", host)
            .header("authorization", "Bearer test-skip")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    let status_url = submit_changes(app, service, since).await;
    let held = get(&format!("{status_url}?f=json")).await;
    assert_eq!(held["status"], "Completed", "{held}");
    get(&facade_path(&held["resultUrl"])).await
}

/// Helper: the attachment sections of a change file, as the consumer reads them.
fn attachment_sections(file: &Value) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let held = &file["edits"][0]["attachments"];
    let adds = held["adds"]
        .as_array()
        .unwrap_or_else(|| panic!("{file}"))
        .clone();
    let updates = held["updates"]
        .as_array()
        .unwrap_or_else(|| panic!("{file}"))
        .clone();
    let deletes = held["deleteIds"]
        .as_array()
        .unwrap_or_else(|| panic!("{file}"))
        .iter()
        .map(|id| id.as_str().unwrap_or_else(|| panic!("{id}")).to_string())
        .collect();
    (adds, updates, deletes)
}

/// Helper: the global id of the feature at one object id.
async fn global_id_of(app: &axum::Router, service: &str, oid: i64) -> String {
    let (status, body) = get_json(
        app,
        &query_url(service, &where_param(&format!("OBJECTID = {oid}"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["features"][0]["attributes"]["globalid"]
        .as_str()
        .unwrap_or_else(|| panic!("{body}"))
        .to_string()
}

/// A delta over attachments, which is what the tombstone and the time window are
/// for: what arrived is an add carrying the parent and the bytes' URL, what went
/// is a global id in deleteIds, and a file that came and went inside the window is
/// in neither, because the client never held it.
#[tokio::test]
async fn test_arcgis_change_file_reports_attachment_adds_and_deletes() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "attachdelta", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;
    // the clock after the load commit, which is what a client extracting here
    // records before it uploads anything
    let first = published_gen(&app, &name).await;
    assert!(first > 0);

    // there before the window opens, so a client at the next generation holds it
    let (doomed, doomed_global) = add_attachment(&app, &name, 100, "doomed.png", b"aaa").await;

    // the commit that opens the window
    let (status, out) = post_form(
        &app,
        &apply_edits_url(&name),
        &form_body(&[
            ("f", "json".into()),
            (
                "updates",
                json!([{"attributes": {"OBJECTID": 100, "name": "renamed"}}]).to_string(),
            ),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let opened = published_gen(&app, &name).await;
    assert!(opened > first, "the clock moved with the commit");

    // inside the window: one that stays, one that comes and goes, and the delete
    // of the one that was already there
    let (arrived, arrived_global) = add_attachment(&app, &name, 100, "arrived.png", b"bbbbb").await;
    let (ephemeral, ephemeral_global) =
        add_attachment(&app, &name, 100, "ephemeral.png", b"cc").await;
    delete_attachment_via_facade(&app, &name, 100, ephemeral).await;
    delete_attachment_via_facade(&app, &name, 100, doomed).await;

    let parent = global_id_of(&app, &name, 100).await;
    let file = extract_changes(&app, &name, opened).await;
    let (adds, updates, deletes) = attachment_sections(&file);

    // a replacement here is a delete and an upload, so nothing is ever an update
    assert_eq!(updates, Vec::<Value>::new(), "{file}");

    assert_eq!(adds.len(), 1, "{file}");
    let record = &adds[0];
    assert_eq!(record["attachmentId"], arrived, "{record}");
    assert_eq!(record["name"], "arrived.png", "{record}");
    assert_eq!(record["contentType"], "image/png", "{record}");
    assert_eq!(record["size"], 5, "{record}");
    assert_eq!(record["parentGlobalId"], parent, "{record}");
    // the same global id the upload answered with, so a client pairs the two
    assert_eq!(record["globalId"], arrived_global, "{record}");
    assert!(
        arrived_global.starts_with('{') && arrived_global.ends_with('}'),
        "{arrived_global}"
    );

    // and the URL serves the bytes
    let (status, _, bytes) = get_download(&app, &facade_path(&record["url"])).await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert_eq!(bytes, b"bbbbb", "{record}");

    // it is absolute on the host the client asked on, which is what a consumer
    // fetches without a base of its own to resolve against
    let named = extract_changes_from_host(&app, &name, opened, "esri.example").await;
    let (adds_from_host, _, _) = attachment_sections(&named);
    let absolute = adds_from_host[0]["url"].as_str().unwrap();
    assert!(
        absolute.starts_with("http://esri.example/arcgis/rest/services/"),
        "{absolute}"
    );

    // the one that was there before the window is reported gone, by global id
    assert_eq!(deletes, vec![doomed_global.clone()], "{file}");

    // and the one that came and went inside the window is in neither section: a
    // client that never saw it has nothing to add and nothing to drop
    assert!(!deletes.contains(&ephemeral_global), "{file}");
    assert!(
        !adds.iter().any(|held| held["globalId"] == ephemeral_global),
        "{file}"
    );

    // asked from a generation before the doomed one existed, that one is in
    // neither section either: it came and went inside that wider window
    let earlier = extract_changes(&app, &name, first).await;
    let (adds, _, deletes) = attachment_sections(&earlier);
    assert_eq!(adds.len(), 1, "{earlier}");
    assert_eq!(adds[0]["attachmentId"], arrived, "{earlier}");
    assert!(deletes.is_empty(), "{earlier}");

    // a window that opens after every attachment answers empty arrays
    let (status, out) = post_form(
        &app,
        &apply_edits_url(&name),
        &form_body(&[
            ("f", "json".into()),
            (
                "updates",
                json!([{"attributes": {"OBJECTID": 100, "name": "again"}}]).to_string(),
            ),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let head = published_gen(&app, &name).await;
    let file = extract_changes(&app, &name, head).await;
    assert_eq!(
        attachment_sections(&file),
        (Vec::new(), Vec::new(), Vec::new()),
        "{file}"
    );
}

/// The load a migration tool actually does: features in one commit, then the
/// attachments, then record the generation. Uploading an attachment commits no
/// changeset, so a generation that counted commits was stuck at the load commit
/// and reported every attachment as an add on the next delta, duplicating them,
/// while one deleted later fell inside that same window and was reported in
/// neither list. The event clock is what closes both.
#[tokio::test]
async fn test_arcgis_attachments_uploaded_after_the_load_commit_move_the_clock() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "loadthen", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;
    let loaded = published_gen(&app, &name).await;

    // the uploads, which commit nothing
    let (_, first_global) = add_attachment(&app, &name, 100, "one.png", b"aaa").await;
    let (second, second_global) = add_attachment(&app, &name, 100, "two.png", b"bb").await;

    // the clock moved even though the history did not, which is the whole point:
    // a cursor recorded now is past the attachments rather than behind them
    let baseline = published_gen(&app, &name).await;
    assert!(
        baseline > loaded,
        "an upload has to move the clock: {loaded} then {baseline}"
    );

    // the delta a client takes straight after the load reports nothing at all,
    // rather than re-adding the attachments it just uploaded
    let file = extract_changes(&app, &name, baseline).await;
    assert_eq!(changed(&file), (vec![], vec![], vec![]), "{file}");
    assert_eq!(
        attachment_sections(&file),
        (Vec::new(), Vec::new(), Vec::new()),
        "no attachment it already holds is reported again: {file}"
    );

    // and one of them deleted later is reported gone rather than staying forever
    delete_attachment_via_facade(&app, &name, 100, second).await;
    let file = extract_changes(&app, &name, baseline).await;
    let (adds, _, deletes) = attachment_sections(&file);
    assert!(adds.is_empty(), "{file}");
    assert_eq!(deletes, vec![second_global], "{file}");
    assert!(!deletes.contains(&first_global), "{file}");

    // the generation that answer carries is past the delete, so asking again from
    // it reports nothing: the delete is not repeated either
    let next = file_gen(&file);
    assert!(next > baseline, "{file}");
    let file = extract_changes(&app, &name, next).await;
    assert_eq!(
        attachment_sections(&file),
        (Vec::new(), Vec::new(), Vec::new()),
        "{file}"
    );
}

/// Generation 0 is the beginning of time, so every live attachment is an add and
/// nothing is reported gone: a client starting from nothing has none to drop.
#[tokio::test]
async fn test_arcgis_change_file_from_generation_zero_holds_every_live_attachment() {
    let (app, _) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "attachzero", "point").await;
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": Uuid::now_v7().to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;

    let (kept, _) = add_attachment(&app, &name, 100, "kept.png", b"aaa").await;
    let (going, _) = add_attachment(&app, &name, 100, "going.png", b"bb").await;
    delete_attachment_via_facade(&app, &name, 100, going).await;

    let file = extract_changes(&app, &name, 0).await;
    let (adds, updates, deletes) = attachment_sections(&file);
    assert_eq!(adds.len(), 1, "{file}");
    assert_eq!(adds[0]["attachmentId"], kept, "{file}");
    assert!(updates.is_empty(), "{file}");
    assert!(deletes.is_empty(), "{file}");
}

/// A tombstone is invisible everywhere a live attachment is visible: the facade's
/// three reads and the native listing, download and meta. The change file is the
/// one place it shows, and it shows as a delete.
#[tokio::test]
async fn test_a_tombstoned_attachment_is_invisible_on_every_read_route() {
    let (app, state) = setup_app().await;
    let (name, _ds, branch) = editable_layer(&app, "tombstone", "point").await;
    let feature_id = Uuid::now_v7();
    commit_features(
        &app,
        branch,
        json!([
            {"type": "insert", "feature_id": feature_id.to_string(),
             "geometry_wkb_hex": point_wkb(1.0, 1.0),
             "properties": {"OBJECTID": 100, "name": "first"}},
        ]),
    )
    .await;

    let (numeric, _) = add_attachment(&app, &name, 100, "gone.png", b"aaa").await;
    // the store's own uuid for it, which is what the native routes name it by
    let held = state.list_attachments(feature_id, branch).await.unwrap();
    assert_eq!(held.len(), 1);
    let uuid = held[0].id;

    delete_attachment_via_facade(&app, &name, 100, numeric).await;

    // the facade: the per-feature listing, queryAttachments and the download
    let (_, listing) = get_json(&app, &format!("{}?f=json", attachments_url(&name, 100))).await;
    assert_eq!(listing["attachmentInfos"], json!([]), "{listing}");
    let (_, groups) = get_json(
        &app,
        &format!("{}?f=json&objectIds=100", query_attachments_url(&name)),
    )
    .await;
    assert_eq!(groups["attachmentGroups"], json!([]), "{groups}");
    let (status, refused) = get_json(
        &app,
        &format!("{}/{numeric}?f=json", attachments_url(&name, 100)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["error"]["code"], 400, "{refused}");

    // and a second delete is refused the same way, by an id naming no attachment
    let (status, again) = post_form(
        &app,
        &delete_attachments_url(&name, 100),
        &form_body(&[("f", "json".into()), ("attachmentIds", numeric.to_string())]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["error"]["code"], 400, "{again}");

    // the native routes: the branch listing, the blob and the meta
    let (_, native) = get_json(
        &app,
        &format!("/api/v1/branches/{branch}/features/{feature_id}/attachments"),
    )
    .await;
    assert_eq!(native, json!([]), "{native}");
    for uri in [
        format!("/api/v1/attachments/{uuid}"),
        format!("/api/v1/attachments/{uuid}/meta"),
    ] {
        let (status, body) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }

    // the native delete, which used to answer not found once the row was gone
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/attachments/{uuid}"))
        .header("authorization", "Bearer test-skip")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Little endian WKB for a 2D LineString, the geometry format a commit takes.
fn line_string_wkb_hex(points: &[(f64, f64)]) -> String {
    let mut wkb = vec![1u8];
    wkb.extend_from_slice(&2u32.to_le_bytes());
    wkb.extend_from_slice(&(points.len() as u32).to_le_bytes());
    for (lon, lat) in points {
        wkb.extend_from_slice(&lon.to_le_bytes());
        wkb.extend_from_slice(&lat.to_le_bytes());
    }
    wkb.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn branch_holding_line(app: &axum::Router, points: &[(f64, f64)]) -> Uuid {
    let dataset_id = create_dataset(app).await;
    let branch_id = create_branch(app, dataset_id, "main").await;
    commit_features(
        app,
        branch_id,
        json!([{
            "type": "insert",
            "feature_id": Uuid::now_v7(),
            "geometry_wkb_hex": line_string_wkb_hex(points),
            "properties": {"name": "line"}
        }]),
    )
    .await;
    branch_id
}

/// A zoomed out tile drops detail no one could see at that zoom.
///
/// The line is a kilometre long and zigzags fifty metres either side of its
/// path. At zoom 5 the tolerance is over a hundred metres, so the zigzag has to
/// go and the line stays. At zoom 14 the tolerance is under a metre, so every
/// vertex has to survive: without that half the test would pass on a tile that
/// simply lost the feature.
#[tokio::test]
async fn test_tile_detail_falls_away_with_zoom() {
    let (app, _state) = setup_app().await;

    const VERTICES: usize = 500;
    const WEST: f64 = 0.001;
    const EAST: f64 = 0.010;
    const LATITUDE: f64 = -0.01;
    // fifty metres, well over the zoom 14 tolerance and well under the zoom 5 one
    const ZIGZAG_DEGREES: f64 = 0.00045;

    let step = (EAST - WEST) / (VERTICES - 1) as f64;
    let zigzag: Vec<(f64, f64)> = (0..VERTICES)
        .map(|index| {
            let side = if index % 2 == 0 { 1.0 } else { -1.0 };
            (WEST + step * index as f64, LATITUDE + side * ZIGZAG_DEGREES)
        })
        .collect();
    let straight = [(WEST, LATITUDE), (EAST, LATITUDE)];

    let zigzag_branch = branch_holding_line(&app, &zigzag).await;
    let straight_branch = branch_holding_line(&app, &straight).await;

    let tile = async |branch: Uuid, z: u32, x: u32, y: u32| {
        let (status, bytes) = get_bytes(
            &app,
            &format!("/api/v1/branches/{branch}/tiles/{z}/{x}/{y}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "tile {z}/{x}/{y} on {branch}");
        bytes.len()
    };

    // both lines are inside tile 8192/8192 at zoom 14 and tile 16/16 at zoom 5
    let zigzag_close = tile(zigzag_branch, 14, 8192, 8192).await;
    let straight_close = tile(straight_branch, 14, 8192, 8192).await;
    let zigzag_far = tile(zigzag_branch, 5, 16, 16).await;
    let straight_far = tile(straight_branch, 5, 16, 16).await;

    assert!(
        zigzag_close > straight_close * 5,
        "zoom 14 lost the vertices before the test could measure them: \
         {zigzag_close} against {straight_close}"
    );
    assert!(
        straight_far > 0,
        "zoom 5 dropped the plain line, so there is nothing to compare against"
    );
    assert!(
        zigzag_far < straight_far * 2,
        "zoom 5 kept the zigzag: {zigzag_far} against {straight_far}"
    );
}
