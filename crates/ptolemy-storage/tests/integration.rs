//! Integration tests for the Ptolemy versioned geodatabase.
//!
//! Requires a running PostgreSQL instance with PostGIS.
//! Set DATABASE_URL env var to run these tests.
//! Example: DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test

use ptolemy_core::branch::Branch;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::DiffOp;
use ptolemy_storage::permission::{Reader, Writer};
use ptolemy_storage::postgres::{MergeResult, PgStore};
use serde_json::json;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// These tests exercise storage, not permissions, so every write is unenforced.
/// The permission ladder has its own tests in the api integration suite.
const W: Writer = Writer::Unenforced;

/// A reader that sees every dataset, matching this suite's unenforced writes.
const ALL: Reader = Reader {
    bypass: true,
    id: None,
};

/// WKB for POINT(0 0) in SRID 4326 (little-endian)
fn point_wkb(x: f64, y: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(21);
    buf.push(0x01); // little-endian
    buf.extend_from_slice(&1u32.to_le_bytes()); // type: Point
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf
}

async fn setup() -> PgStore {
    setup_with_analyze_threshold(ptolemy_storage::DEFAULT_ANALYZE_ROW_THRESHOLD).await
}

/// Fresh schema with the bulk-write ANALYZE threshold pinned, so a test does
/// not depend on the ambient environment.
async fn setup_with_analyze_threshold(rows: usize) -> PgStore {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ptolemy_test".to_string());
    let pool = PgPool::connect(&url)
        .await
        .expect("Failed to connect to database");

    // Clean slate
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS conflicts CASCADE;
         DROP TABLE IF EXISTS attachments CASCADE;
         DROP TABLE IF EXISTS feature_versions CASCADE;
         DROP TABLE IF EXISTS changesets CASCADE;
         DROP TABLE IF EXISTS branches CASCADE;
         DROP TABLE IF EXISTS datasets CASCADE;
         DROP TABLE IF EXISTS _sqlx_migrations CASCADE;",
    )
    .execute(&pool)
    .await
    .unwrap();

    let store = PgStore::with_analyze_threshold(pool, rows);
    store.migrate().await.unwrap();
    store
}

async fn create_test_dataset(store: &PgStore) -> Dataset {
    let ds = Dataset {
        id: Uuid::now_v7(),
        name: format!("test_dataset_{}", Uuid::now_v7()),
        srid: 4326,
        geometry_type: GeometryType::Point,
        created_at: OffsetDateTime::now_utc(),
        created_by: "test".to_string(),
        external: None,
        visibility: Default::default(),
    };
    store.create_dataset(&ds, None).await.unwrap();
    ds
}

async fn create_test_branch(store: &PgStore, dataset_id: Uuid, name: &str) -> Branch {
    let branch = Branch {
        id: Uuid::now_v7(),
        dataset_id,
        name: name.to_string(),
        head: None,
        created_at: OffsetDateTime::now_utc(),
        created_by: "test".to_string(),
    };
    store.create_branch(&branch, &W).await.unwrap();
    branch
}

// ═══════════════════════════════════════════════════════════════════════
// Dataset Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_create_and_get_dataset() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;

    let fetched = store.get_dataset(ds.id).await.unwrap();
    assert_eq!(fetched.name, ds.name);
    assert_eq!(fetched.srid, 4326);
    assert_eq!(fetched.geometry_type, GeometryType::Point);
}

#[tokio::test]
async fn test_list_datasets() {
    let store = setup().await;
    create_test_dataset(&store).await;
    create_test_dataset(&store).await;

    let datasets = store.list_datasets(&ALL).await.unwrap();
    assert!(datasets.len() >= 2);
}

#[tokio::test]
async fn test_get_nonexistent_dataset() {
    let store = setup().await;
    let result = store.get_dataset(Uuid::now_v7()).await;
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════
// Branch Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_create_and_get_branch() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let fetched = store.get_branch(branch.id).await.unwrap();
    assert_eq!(fetched.name, "main");
    assert_eq!(fetched.dataset_id, ds.id);
    assert_eq!(fetched.head, None);
}

#[tokio::test]
async fn test_list_branches() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    create_test_branch(&store, ds.id, "main").await;
    create_test_branch(&store, ds.id, "dev").await;

    let branches = store.list_branches(ds.id).await.unwrap();
    assert_eq!(branches.len(), 2);
}

// ═══════════════════════════════════════════════════════════════════════
// Commit & Feature Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_commit_insert_features() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    let changeset = store
        .commit(
            branch.id,
            "Add two points",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: f1,
                    geometry_wkb: point_wkb(1.0, 2.0),
                    properties: json!({"name": "Park"}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(3.0, 4.0),
                    properties: json!({"name": "School"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    assert_eq!(changeset.message, "Add two points");
    assert_eq!(changeset.author, "alice");
    assert_eq!(changeset.parent_id, None); // first commit

    // Branch head should be updated
    let updated_branch = store.get_branch(branch.id).await.unwrap();
    assert_eq!(updated_branch.head, Some(changeset.id));

    // Features should be queryable
    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 2);
}

#[tokio::test]
async fn test_commit_update_feature() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    // Insert
    store
        .commit(
            branch.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(1.0, 2.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Update properties only
    let c2 = store
        .commit(
            branch.id,
            "Rename park",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: None, // keep geometry
                properties: Some(json!({"name": "Central Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    assert!(c2.parent_id.is_some());

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 1);
    assert_eq!(features[0].properties["name"], "Central Park");
}

#[tokio::test]
async fn test_commit_delete_feature() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    store
        .commit(
            branch.id,
            "Add",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(1.0, 2.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    store
        .commit(
            branch.id,
            "Delete",
            "alice",
            &[DiffOp::Delete { feature_id: f1 }],
            &W,
        )
        .await
        .unwrap();

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 0);
}

#[tokio::test]
async fn test_feature_at_specific_changeset() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    let c1 = store
        .commit(
            branch.id,
            "v1",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(1.0, 2.0),
                properties: json!({"version": 1}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    store
        .commit(
            branch.id,
            "v2",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: None,
                properties: Some(json!({"version": 2})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Time-travel: get feature at c1
    let feat = store.get_feature_at(f1, c1.id).await.unwrap().unwrap();
    assert_eq!(feat.properties["version"], 1);
}

// ═══════════════════════════════════════════════════════════════════════
// History Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_branch_history() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    store
        .commit(
            branch.id,
            "First",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    store
        .commit(
            branch.id,
            "Second",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(1.0, 1.0)),
                properties: Some(json!({"updated": true})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let history = store.get_branch_history(branch.id, 100).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].message, "Second"); // most recent first
    assert_eq!(history[1].message, "First");
}

// ═══════════════════════════════════════════════════════════════════════
// Diff Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_diff_from_root() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    let c1 = store
        .commit(
            branch.id,
            "Add features",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: f1,
                    geometry_wkb: point_wkb(0.0, 0.0),
                    properties: json!({"a": 1}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"b": 2}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let diff = store.diff(None, c1.id).await.unwrap();
    assert_eq!(diff.operations.len(), 2);
}

#[tokio::test]
async fn test_diff_between_changesets() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();

    let c1 = store
        .commit(
            branch.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let c2 = store
        .commit(
            branch.id,
            "Add another",
            "alice",
            &[DiffOp::Insert {
                feature_id: f2,
                geometry_wkb: point_wkb(1.0, 1.0),
                properties: json!({}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let diff = store.diff(Some(c1.id), c2.id).await.unwrap();
    assert_eq!(diff.operations.len(), 1); // only f2 is new
}

// ═══════════════════════════════════════════════════════════════════════
// Merge Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_merge_no_conflicts() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    // Initial commit on main
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Origin"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Create feature branch from main's head
    let main_updated = store.get_branch(main.id).await.unwrap();
    let feature_branch = Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "feature".to_string(),
        head: main_updated.head, // fork from main's head
        created_at: OffsetDateTime::now_utc(),
        created_by: "bob".to_string(),
    };
    store.create_branch(&feature_branch, &W).await.unwrap();

    // Add a new feature on the feature branch
    let f2 = Uuid::now_v7();
    store
        .commit(
            feature_branch.id,
            "Add school",
            "bob",
            &[DiffOp::Insert {
                feature_id: f2,
                geometry_wkb: point_wkb(5.0, 5.0),
                properties: json!({"name": "School"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Meanwhile, update f1 on main
    store
        .commit(
            main.id,
            "Rename origin",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: None,
                properties: Some(json!({"name": "Town Center"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Merge feature -> main (no conflicts: different features modified)
    let result = store
        .merge(feature_branch.id, main.id, "alice", &W)
        .await
        .unwrap();
    match result {
        MergeResult::Success(changeset) => {
            assert!(changeset.message.contains("Merge"));
        }
        MergeResult::Conflicts(c) => panic!("Expected no conflicts, got {c:?}"),
    }

    // Main should now have both features with latest state
    let features = store.list_features_at_head(main.id).await.unwrap();
    assert_eq!(features.len(), 2);
}

#[tokio::test]
async fn test_merge_with_conflicts() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    // Initial commit
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Fork
    let main_updated = store.get_branch(main.id).await.unwrap();
    let feature_branch = Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "feature".to_string(),
        head: main_updated.head,
        created_at: OffsetDateTime::now_utc(),
        created_by: "bob".to_string(),
    };
    store.create_branch(&feature_branch, &W).await.unwrap();

    // Both sides modify the SAME feature differently
    store
        .commit(
            main.id,
            "Alice renames",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: None,
                properties: Some(json!({"name": "Central Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    store
        .commit(
            feature_branch.id,
            "Bob moves",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(10.0, 10.0)),
                properties: Some(json!({"name": "Park", "moved": true})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Merge should detect conflict
    let result = store
        .merge(feature_branch.id, main.id, "alice", &W)
        .await
        .unwrap();
    match result {
        MergeResult::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].feature_id, f1);
        }
        MergeResult::Success(_) => panic!("Expected conflict!"),
    }
}

#[tokio::test]
async fn test_merge_same_change_no_conflict() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let main_updated = store.get_branch(main.id).await.unwrap();
    let feature_branch = Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "feature".to_string(),
        head: main_updated.head,
        created_at: OffsetDateTime::now_utc(),
        created_by: "bob".to_string(),
    };
    store.create_branch(&feature_branch, &W).await.unwrap();

    // Both sides delete the same feature — should NOT conflict
    store
        .commit(
            main.id,
            "Alice deletes",
            "alice",
            &[DiffOp::Delete { feature_id: f1 }],
            &W,
        )
        .await
        .unwrap();

    store
        .commit(
            feature_branch.id,
            "Bob also deletes",
            "bob",
            &[DiffOp::Delete { feature_id: f1 }],
            &W,
        )
        .await
        .unwrap();

    let result = store
        .merge(feature_branch.id, main.id, "alice", &W)
        .await
        .unwrap();
    match result {
        MergeResult::Success(_) => {} // Good — same operation = no conflict
        MergeResult::Conflicts(c) => panic!("Expected no conflict for identical ops, got {c:?}"),
    }
}

#[tokio::test]
async fn test_merge_base_finding() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    let c1 = store
        .commit(
            main.id,
            "Root",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Fork at c1
    let feature_branch = Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "feature".to_string(),
        head: Some(c1.id),
        created_at: OffsetDateTime::now_utc(),
        created_by: "bob".to_string(),
    };
    store.create_branch(&feature_branch, &W).await.unwrap();

    // Advance both
    let c2 = store
        .commit(
            main.id,
            "Main advance",
            "alice",
            &[DiffOp::Insert {
                feature_id: Uuid::now_v7(),
                geometry_wkb: point_wkb(1.0, 0.0),
                properties: json!({}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let c3 = store
        .commit(
            feature_branch.id,
            "Feature advance",
            "bob",
            &[DiffOp::Insert {
                feature_id: Uuid::now_v7(),
                geometry_wkb: point_wkb(0.0, 1.0),
                properties: json!({}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    // Merge base should be c1
    let base = store.find_merge_base(c2.id, c3.id).await.unwrap();
    assert_eq!(base, Some(c1.id));
}

/// Fork a new branch at another branch's current head.
async fn fork_branch(store: &PgStore, dataset_id: Uuid, from_branch: Uuid, name: &str) -> Branch {
    let parent = store.get_branch(from_branch).await.unwrap();
    let branch = Branch {
        id: Uuid::now_v7(),
        dataset_id,
        name: name.to_string(),
        head: parent.head,
        created_at: OffsetDateTime::now_utc(),
        created_by: "test".to_string(),
    };
    store.create_branch(&branch, &W).await.unwrap();
    branch
}

/// Snapshot a branch head as feature_id -> (geometry_wkb, properties).
async fn snapshot(
    store: &PgStore,
    branch_id: Uuid,
) -> std::collections::BTreeMap<Uuid, (Vec<u8>, serde_json::Value)> {
    store
        .list_features_at_head(branch_id)
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.id, (f.geometry_wkb, f.properties)))
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// Merge Depth Tests
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_merge_conflict_geometry_edit_edit() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    // Both sides move the same feature to different places
    store
        .commit(
            main.id,
            "Alice moves",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(1.0, 1.0)),
                properties: Some(json!({"name": "Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();
    store
        .commit(
            feature.id,
            "Bob moves",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(2.0, 2.0)),
                properties: Some(json!({"name": "Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    match result {
        MergeResult::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].feature_id, f1);
            assert!(matches!(conflicts[0].ours, DiffOp::Update { .. }));
            assert!(matches!(conflicts[0].theirs, DiffOp::Update { .. }));
        }
        MergeResult::Success(_) => panic!("Expected geometry edit-edit conflict"),
    }
}

#[tokio::test]
async fn test_merge_conflict_attribute_edit_edit() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    // Both sides rename the same feature differently, geometry untouched
    store
        .commit(
            main.id,
            "Alice renames",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(0.0, 0.0)),
                properties: Some(json!({"name": "North Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();
    store
        .commit(
            feature.id,
            "Bob renames",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(0.0, 0.0)),
                properties: Some(json!({"name": "South Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    match result {
        MergeResult::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].feature_id, f1);
        }
        MergeResult::Success(_) => panic!("Expected attribute edit-edit conflict"),
    }
}

#[tokio::test]
async fn test_merge_conflict_delete_vs_edit_both_directions() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: f1,
                    geometry_wkb: point_wkb(0.0, 0.0),
                    properties: json!({"name": "A"}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"name": "B"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    // f1: target deletes, source edits. f2: target edits, source deletes.
    store
        .commit(
            main.id,
            "Alice deletes f1, edits f2",
            "alice",
            &[
                DiffOp::Delete { feature_id: f1 },
                DiffOp::Update {
                    feature_id: f2,
                    geometry_wkb: Some(point_wkb(1.5, 1.5)),
                    properties: Some(json!({"name": "B"})),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();
    store
        .commit(
            feature.id,
            "Bob edits f1, deletes f2",
            "bob",
            &[
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(0.5, 0.5)),
                    properties: Some(json!({"name": "A"})),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Delete { feature_id: f2 },
            ],
            &W,
        )
        .await
        .unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    match result {
        MergeResult::Conflicts(mut conflicts) => {
            conflicts.sort_by_key(|c| c.feature_id);
            let mut expected = [f1, f2];
            expected.sort();
            assert_eq!(conflicts.len(), 2);
            assert_eq!(conflicts[0].feature_id, expected[0]);
            assert_eq!(conflicts[1].feature_id, expected[1]);
            for c in &conflicts {
                let delete_on_one_side = matches!(c.ours, DiffOp::Delete { .. })
                    ^ matches!(c.theirs, DiffOp::Delete { .. });
                assert!(delete_on_one_side, "expected delete-vs-edit shape: {c:?}");
            }
        }
        MergeResult::Success(_) => panic!("Expected delete-vs-edit conflicts"),
    }
}

/// Edits to DIFFERENT attributes of the same feature still conflict:
/// merge is feature-level (ops_equal compares whole ops), not attribute-level.
#[tokio::test]
async fn test_merge_different_attributes_same_feature_conflicts() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Park", "capacity": 100}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    store
        .commit(
            main.id,
            "Alice edits name",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(0.0, 0.0)),
                properties: Some(json!({"name": "Central Park", "capacity": 100})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();
    store
        .commit(
            feature.id,
            "Bob edits capacity",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(0.0, 0.0)),
                properties: Some(json!({"name": "Park", "capacity": 250})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    match result {
        MergeResult::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].feature_id, f1);
        }
        MergeResult::Success(_) => {
            panic!("Merge is feature-level; disjoint attribute edits must conflict")
        }
    }
}

#[tokio::test]
async fn test_merge_disjoint_feature_sets_clean() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Base"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    let f2 = Uuid::now_v7();
    let f3 = Uuid::now_v7();
    let f4 = Uuid::now_v7();
    // Main: insert f2 and update f1. Feature: insert f3 and f4.
    store
        .commit(
            main.id,
            "Main work",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({"name": "Main2"}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(0.1, 0.1)),
                    properties: Some(json!({"name": "Base v2"})),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();
    store
        .commit(
            feature.id,
            "Feature work",
            "bob",
            &[
                DiffOp::Insert {
                    feature_id: f3,
                    geometry_wkb: point_wkb(3.0, 3.0),
                    properties: json!({"name": "Feat3"}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f4,
                    geometry_wkb: point_wkb(4.0, 4.0),
                    properties: json!({"name": "Feat4"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    let MergeResult::Success(_) = result else {
        panic!("Expected clean merge of disjoint feature sets");
    };

    let merged = snapshot(&store, main.id).await;
    assert_eq!(merged.len(), 4);
    assert_eq!(merged[&f1].1["name"], "Base v2");
    assert_eq!(merged[&f2].1["name"], "Main2");
    assert_eq!(merged[&f3].1["name"], "Feat3");
    assert_eq!(merged[&f4].1["name"], "Feat4");
}

/// Round-trip: diff(pre-merge head, merge commit) applied on top of the
/// pre-merge head must reproduce the merged state exactly.
#[tokio::test]
async fn test_diff_round_trip_after_merge() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Base"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    let f2 = Uuid::now_v7();
    let f3 = Uuid::now_v7();
    // Feature branch: update f1, insert f2. Main: insert f3.
    store
        .commit(
            feature.id,
            "Feature work",
            "bob",
            &[
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(9.0, 9.0)),
                    properties: Some(json!({"name": "Base moved"})),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({"name": "Feat2"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();
    store
        .commit(
            main.id,
            "Main work",
            "alice",
            &[DiffOp::Insert {
                feature_id: f3,
                geometry_wkb: point_wkb(3.0, 3.0),
                properties: json!({"name": "Main3"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let pre_merge_head = store.get_branch(main.id).await.unwrap().head.unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    let MergeResult::Success(merge_cs) = result else {
        panic!("Expected clean merge");
    };

    // Replay the diff onto a branch parked at the pre-merge head
    let diff = store.diff(Some(pre_merge_head), merge_cs.id).await.unwrap();
    let replay = Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "replay".to_string(),
        head: Some(pre_merge_head),
        created_at: OffsetDateTime::now_utc(),
        created_by: "test".to_string(),
    };
    store.create_branch(&replay, &W).await.unwrap();
    store
        .commit(replay.id, "Replay diff", "test", &diff.operations, &W)
        .await
        .unwrap();

    assert_eq!(
        snapshot(&store, replay.id).await,
        snapshot(&store, main.id).await,
        "diff(a,b) applied to a must equal b"
    );
}

#[tokio::test]
async fn test_merge_idempotent_remerge() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Base"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    let f2 = Uuid::now_v7();
    store
        .commit(
            feature.id,
            "Feature work",
            "bob",
            &[
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(9.0, 9.0)),
                    properties: Some(json!({"name": "Base moved"})),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({"name": "Feat2"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let first = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    let MergeResult::Success(_) = first else {
        panic!("Expected first merge to succeed");
    };
    let after_first = snapshot(&store, main.id).await;

    // Re-merging an already-merged branch must not conflict or change state
    let second = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    let MergeResult::Success(_) = second else {
        panic!("Re-merge of merged branch must not conflict");
    };
    assert_eq!(snapshot(&store, main.id).await, after_first);
}

#[tokio::test]
async fn test_concurrent_commits_same_branch_no_lost_update() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let fa = Uuid::now_v7();
    let fb = Uuid::now_v7();
    let ops_a = [DiffOp::Insert {
        feature_id: fa,
        geometry_wkb: point_wkb(1.0, 1.0),
        properties: json!({"who": "a"}),
        valid_from: None,
        valid_to: None,
    }];
    let ops_b = [DiffOp::Insert {
        feature_id: fb,
        geometry_wkb: point_wkb(2.0, 2.0),
        properties: json!({"who": "b"}),
        valid_from: None,
        valid_to: None,
    }];
    let (ra, rb) = tokio::join!(
        store.commit(branch.id, "A", "alice", &ops_a, &W),
        store.commit(branch.id, "B", "bob", &ops_b, &W),
    );
    let ca = ra.unwrap();
    let cb = rb.unwrap();

    // One commit must be the parent of the other; neither may be lost
    let parents = [ca.parent_id, cb.parent_id];
    assert!(
        parents.contains(&None)
            && (parents.contains(&Some(ca.id)) || parents.contains(&Some(cb.id))),
        "commits must chain, got parents {parents:?}"
    );
    let history = store.get_branch_history(branch.id, 100).await.unwrap();
    assert_eq!(history.len(), 2);
    let features = snapshot(&store, branch.id).await;
    assert_eq!(features.len(), 2);
    assert!(features.contains_key(&fa) && features.contains_key(&fb));
}

/// A partial update (geometry or properties omitted) must fill the gap from
/// the committing branch's own state, never from another branch.
#[tokio::test]
async fn test_partial_update_does_not_leak_across_branches() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(0.0, 0.0),
                properties: json!({"name": "Park"}),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    // The other branch moves f1 and renames it
    store
        .commit(
            feature.id,
            "Bob moves and renames",
            "bob",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(9.0, 9.0)),
                properties: Some(json!({"name": "Bob's Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let main_before = snapshot(&store, main.id).await;

    // Properties-only update on main: geometry must stay main's, not Bob's
    store
        .commit(
            main.id,
            "Alice renames only",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: None,
                properties: Some(json!({"name": "Alice's Park"})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();
    let main_after = snapshot(&store, main.id).await;
    assert_eq!(
        main_after[&f1].0, main_before[&f1].0,
        "geometry-preserving update pulled geometry from another branch"
    );
    assert_eq!(main_after[&f1].1["name"], "Alice's Park");

    // Geometry-only update on main: properties must stay main's, not Bob's
    store
        .commit(
            main.id,
            "Alice nudges only",
            "alice",
            &[DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(0.5, 0.5)),
                properties: None,
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();
    let main_final = snapshot(&store, main.id).await;
    assert_eq!(
        main_final[&f1].1["name"], "Alice's Park",
        "properties-preserving update pulled properties from another branch"
    );

    // The other branch is untouched throughout
    let feature_state = snapshot(&store, feature.id).await;
    assert_eq!(feature_state[&f1].1["name"], "Bob's Park");
}

#[tokio::test]
async fn test_insert_then_update_same_feature_in_one_commit() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    store
        .commit(
            branch.id,
            "Insert and update",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: f1,
                    geometry_wkb: point_wkb(0.0, 0.0),
                    properties: json!({"v": 1}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(1.0, 1.0)),
                    properties: Some(json!({"v": 2})),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let features = snapshot(&store, branch.id).await;
    assert_eq!(features.len(), 1);
    assert_eq!(
        features[&f1].1["v"], 2,
        "later op in the same commit must win"
    );
}

/// The "features" SQL view must resolve each branch by walking its ancestor
/// chain: forks see pre-fork features, and each branch sees its own edits.
#[tokio::test]
async fn test_features_view_inherits_pre_fork_features() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();
    let f2 = Uuid::now_v7();
    store
        .commit(
            main.id,
            "Initial",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: f1,
                    geometry_wkb: point_wkb(0.0, 0.0),
                    properties: json!({"name": "Park"}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"name": "School"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let feature = fork_branch(&store, ds.id, main.id, "feature").await;

    // Edit f1 and add f3 on the fork only
    let f3 = Uuid::now_v7();
    store
        .commit(
            feature.id,
            "Fork work",
            "bob",
            &[
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(9.0, 9.0)),
                    properties: Some(json!({"name": "Bob's Park"})),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f3,
                    geometry_wkb: point_wkb(3.0, 3.0),
                    properties: json!({"name": "Cafe"}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    async fn view_features(
        store: &PgStore,
        branch_id: Uuid,
    ) -> std::collections::BTreeMap<Uuid, serde_json::Value> {
        sqlx::query("SELECT id, properties FROM features WHERE branch_id = $1")
            .bind(branch_id)
            .fetch_all(store.pool())
            .await
            .unwrap()
            .into_iter()
            .map(|r| {
                (
                    r.get::<Uuid, _>("id"),
                    r.get::<serde_json::Value, _>("properties"),
                )
            })
            .collect()
    }

    // Fork sees inherited f2, its own f1 edit, and its new f3
    let fork_view = view_features(&store, feature.id).await;
    assert_eq!(fork_view.len(), 3, "fork must see inherited features");
    assert_eq!(fork_view[&f1]["name"], "Bob's Park");
    assert_eq!(fork_view[&f2]["name"], "School");
    assert_eq!(fork_view[&f3]["name"], "Cafe");

    // Main still sees its own versions, untouched by the fork
    let main_view = view_features(&store, main.id).await;
    assert_eq!(main_view.len(), 2);
    assert_eq!(main_view[&f1]["name"], "Park");
    assert_eq!(main_view[&f2]["name"], "School");
}

// ═══════════════════════════════════════════════════════════════════════
// Edge Cases
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_multiple_commits_chain() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    // Chain of 5 commits
    for i in 0..5 {
        let op = if i == 0 {
            DiffOp::Insert {
                feature_id: f1,
                geometry_wkb: point_wkb(i as f64, 0.0),
                properties: json!({"step": i}),
                valid_from: None,
                valid_to: None,
            }
        } else {
            DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(i as f64, 0.0)),
                properties: Some(json!({"step": i})),
                valid_from: None,
                valid_to: None,
            }
        };
        store
            .commit(branch.id, &format!("Step {i}"), "alice", &[op], &W)
            .await
            .unwrap();
    }

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 1);
    assert_eq!(features[0].properties["step"], 4);

    let history = store.get_branch_history(branch.id, 100).await.unwrap();
    assert_eq!(history.len(), 5);
}

#[tokio::test]
async fn test_empty_commit() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    // Commit with no operations (allowed, like an empty git commit)
    let c = store
        .commit(branch.id, "Empty", "alice", &[], &W)
        .await
        .unwrap();
    assert_eq!(c.message, "Empty");

    let updated = store.get_branch(branch.id).await.unwrap();
    assert_eq!(updated.head, Some(c.id));
}

#[tokio::test]
async fn test_delete_nonexistent_feature_at_head() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let f1 = Uuid::now_v7();

    // Delete a feature that was never inserted — should still record the op
    store
        .commit(
            branch.id,
            "Ghost delete",
            "alice",
            &[DiffOp::Delete { feature_id: f1 }],
            &W,
        )
        .await
        .unwrap();

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 0);
}

#[tokio::test]
async fn test_insert_many_features() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let ops: Vec<DiffOp> = (0..100)
        .map(|i| DiffOp::Insert {
            feature_id: Uuid::now_v7(),
            geometry_wkb: point_wkb(i as f64, i as f64),
            properties: json!({"index": i}),
            valid_from: None,
            valid_to: None,
        })
        .collect();

    store
        .commit(branch.id, "Bulk insert", "alice", &ops, &W)
        .await
        .unwrap();

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 100);
}

/// A bulk import leaves postgres with pre-import statistics, and the recursive
/// changeset walk every read builds on then gets a plan sized for an empty
/// table. The write path has to refresh them itself; waiting for autoanalyze
/// costs minutes of slow reads.
#[tokio::test]
async fn test_bulk_commit_refreshes_planner_statistics() {
    let store = setup_with_analyze_threshold(50).await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let inserts = |n: usize| -> Vec<DiffOp> {
        (0..n)
            .map(|i| DiffOp::Insert {
                feature_id: Uuid::now_v7(),
                geometry_wkb: point_wkb(i as f64, i as f64),
                properties: json!({"index": i}),
                valid_from: None,
                valid_to: None,
            })
            .collect()
    };

    store
        .commit(branch.id, "Small", "alice", &inserts(10), &W)
        .await
        .unwrap();
    assert_eq!(
        store.analyzer().scheduled(),
        0,
        "a commit under the threshold must not analyze"
    );

    store
        .commit(branch.id, "Bulk", "alice", &inserts(50), &W)
        .await
        .unwrap();
    assert_eq!(store.analyzer().scheduled(), 1);

    store.analyzer().wait_idle().await;

    // last_analyze counts only explicit ANALYZE, so a background autoanalyze
    // cannot make this pass on its own.
    for table in ["feature_versions", "changesets", "branches"] {
        let row = sqlx::query(
            "SELECT c.reltuples, s.last_analyze
               FROM pg_class c JOIN pg_stat_user_tables s ON s.relid = c.oid
              WHERE c.relname = $1",
        )
        .bind(table)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            row.get::<Option<OffsetDateTime>, _>("last_analyze")
                .is_some(),
            "{table} was never analyzed"
        );
        assert!(
            row.get::<f32, _>("reltuples") >= 0.0,
            "{table} still has no row estimate"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Mixed Geometry Datasets
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_mixed_geometry_dataset_round_trips() {
    let store = setup().await;
    let ds = Dataset {
        id: Uuid::now_v7(),
        name: format!("mixed_{}", Uuid::now_v7()),
        srid: 4326,
        geometry_type: GeometryType::Geometry,
        created_at: OffsetDateTime::now_utc(),
        created_by: "test".to_string(),
        external: None,
        visibility: Default::default(),
    };
    store.create_dataset(&ds, None).await.unwrap();

    let fetched = store.get_dataset(ds.id).await.unwrap();
    assert_eq!(fetched.geometry_type, GeometryType::Geometry);

    let stored: String = sqlx::query("SELECT geometry_type FROM datasets WHERE id = $1")
        .bind(ds.id)
        .fetch_one(store.pool())
        .await
        .unwrap()
        .get("geometry_type");
    assert_eq!(stored, "geometry");
}

#[tokio::test]
async fn test_mixed_geometry_dataset_accepts_any_geometry() {
    let store = setup().await;
    let ds = Dataset {
        id: Uuid::now_v7(),
        name: format!("mixed_{}", Uuid::now_v7()),
        srid: 4326,
        geometry_type: GeometryType::Geometry,
        created_at: OffsetDateTime::now_utc(),
        created_by: "test".to_string(),
        external: None,
        visibility: Default::default(),
    };
    store.create_dataset(&ds, None).await.unwrap();
    let branch = create_test_branch(&store, ds.id, "main").await;

    // a point and a linestring in one dataset
    store
        .commit(
            branch.id,
            "mixed",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: Uuid::now_v7(),
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({}),
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: Uuid::now_v7(),
                    geometry_wkb: linestring_wkb(),
                    properties: json!({}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 2);
}

/// WKB for LINESTRING(0 0, 1 1) (little-endian).
fn linestring_wkb() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(0x01);
    buf.extend_from_slice(&2u32.to_le_bytes()); // type: LineString
    buf.extend_from_slice(&2u32.to_le_bytes()); // point count
    for (x, y) in [(0.0f64, 0.0f64), (1.0, 1.0)] {
        buf.extend_from_slice(&x.to_le_bytes());
        buf.extend_from_slice(&y.to_le_bytes());
    }
    buf
}

// ═══════════════════════════════════════════════════════════════════════
// Feature Valid Time
// ═══════════════════════════════════════════════════════════════════════

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

#[tokio::test]
async fn test_valid_time_round_trips() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let timed = Uuid::now_v7();
    let untimed = Uuid::now_v7();
    store
        .commit(
            branch.id,
            "with and without a valid time",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: timed,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({}),
                    valid_from: Some(ts(1_000)),
                    valid_to: Some(ts(2_000)),
                },
                DiffOp::Insert {
                    feature_id: untimed,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let features = store
        .list_features_paginated(branch.id, None, 100, None)
        .await
        .unwrap();
    let got = |id: Uuid| features.iter().find(|f| f.id == id).unwrap();
    assert_eq!(got(timed).valid_from, Some(ts(1_000)));
    assert_eq!(got(timed).valid_to, Some(ts(2_000)));
    assert_eq!(got(untimed).valid_from, None);
    assert_eq!(got(untimed).valid_to, None);
}

#[tokio::test]
async fn test_valid_time_open_ranges_round_trip() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let open_end = Uuid::now_v7();
    let open_start = Uuid::now_v7();
    store
        .commit(
            branch.id,
            "open ranges",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: open_end,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({}),
                    valid_from: Some(ts(1_000)),
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: open_start,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({}),
                    valid_from: None,
                    valid_to: Some(ts(2_000)),
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let features = store
        .list_features_paginated(branch.id, None, 100, None)
        .await
        .unwrap();
    let got = |id: Uuid| features.iter().find(|f| f.id == id).unwrap();
    assert_eq!(got(open_end).valid_from, Some(ts(1_000)));
    assert_eq!(got(open_end).valid_to, None);
    assert_eq!(got(open_start).valid_from, None);
    assert_eq!(got(open_start).valid_to, Some(ts(2_000)));
}

#[tokio::test]
async fn test_update_keeps_valid_time_when_omitted() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let fid = Uuid::now_v7();
    store
        .commit(
            branch.id,
            "insert",
            "alice",
            &[DiffOp::Insert {
                feature_id: fid,
                geometry_wkb: point_wkb(1.0, 1.0),
                properties: json!({"n": 1}),
                valid_from: Some(ts(1_000)),
                valid_to: Some(ts(2_000)),
            }],
            &W,
        )
        .await
        .unwrap();

    store
        .commit(
            branch.id,
            "edit properties only",
            "alice",
            &[DiffOp::Update {
                feature_id: fid,
                geometry_wkb: None,
                properties: Some(json!({"n": 2})),
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let features = store
        .list_features_paginated(branch.id, None, 100, None)
        .await
        .unwrap();
    let f = features.iter().find(|f| f.id == fid).unwrap();
    assert_eq!(f.properties["n"], 2);
    assert_eq!(f.valid_from, Some(ts(1_000)));
    assert_eq!(f.valid_to, Some(ts(2_000)));
}

#[tokio::test]
async fn test_valid_at_filters_features() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let closed = Uuid::now_v7();
    let open_end = Uuid::now_v7();
    let open_start = Uuid::now_v7();
    let untimed = Uuid::now_v7();
    store
        .commit(
            branch.id,
            "four shapes of valid time",
            "alice",
            &[
                DiffOp::Insert {
                    feature_id: closed,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({}),
                    valid_from: Some(ts(1_000)),
                    valid_to: Some(ts(2_000)),
                },
                DiffOp::Insert {
                    feature_id: open_end,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({}),
                    valid_from: Some(ts(1_500)),
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: open_start,
                    geometry_wkb: point_wkb(3.0, 3.0),
                    properties: json!({}),
                    valid_from: None,
                    valid_to: Some(ts(1_500)),
                },
                DiffOp::Insert {
                    feature_id: untimed,
                    geometry_wkb: point_wkb(4.0, 4.0),
                    properties: json!({}),
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    async fn at(store: &PgStore, branch_id: Uuid, t: i64) -> Vec<Uuid> {
        store
            .list_features_paginated(branch_id, None, 100, Some(ts(t)))
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect()
    }

    // t=500: before the closed range starts, before open_end starts,
    // inside open_start, and untimed always matches
    let ids = at(&store, branch.id, 500).await;
    assert!(!ids.contains(&closed));
    assert!(!ids.contains(&open_end));
    assert!(ids.contains(&open_start));
    assert!(ids.contains(&untimed));

    // t=1200: inside the closed range and still inside open_start's (-inf, 1500)
    let ids = at(&store, branch.id, 1_200).await;
    assert!(ids.contains(&closed));
    assert!(!ids.contains(&open_end));
    assert!(ids.contains(&open_start));
    assert!(ids.contains(&untimed));

    // t=1500: closed still covers it, open_end starts here, open_start ends
    // here and is excluded because the range is half-open
    let ids = at(&store, branch.id, 1_500).await;
    assert!(ids.contains(&closed));
    assert!(ids.contains(&open_end));
    assert!(!ids.contains(&open_start));

    // t=2000: the closed range's end is excluded, open_end runs on
    let ids = at(&store, branch.id, 2_000).await;
    assert!(!ids.contains(&closed));
    assert!(ids.contains(&open_end));
}

// ═══════════════════════════════════════════════════════════════════════
// Attachment Ownership
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_attachment_owner_check_rejects_bad_rows() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    let insert = |feature: Option<Uuid>, branch: Option<Uuid>, dataset: Option<Uuid>| {
        sqlx::query(
            "INSERT INTO attachments (id, feature_id, branch_id, dataset_id, name, data, created_by)
             VALUES ($1, $2, $3, $4, 'x', '\\x00'::bytea, 'test')",
        )
        .bind(Uuid::now_v7())
        .bind(feature)
        .bind(branch)
        .bind(dataset)
        .execute(store.pool())
    };

    let fid = Uuid::now_v7();
    // both owners set
    assert!(
        insert(Some(fid), Some(branch.id), Some(ds.id))
            .await
            .is_err()
    );
    // neither owner set
    assert!(insert(None, None, None).await.is_err());
    // a branch without a feature is half an owner
    assert!(insert(None, Some(branch.id), None).await.is_err());
    // each of the two valid shapes is accepted
    assert!(insert(Some(fid), Some(branch.id), None).await.is_ok());
    assert!(insert(None, None, Some(ds.id)).await.is_ok());
}
