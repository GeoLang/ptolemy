//! Integration tests for the Ptolemy versioned geodatabase.
//!
//! Requires a running PostgreSQL instance with PostGIS.
//! Set DATABASE_URL env var to run these tests.
//! Example: DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test

use ptolemy_core::branch::Branch;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::DiffOp;
use ptolemy_storage::postgres::{MergeResult, PgStore};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

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
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ptolemy_test".to_string());
    let pool = PgPool::connect(&url).await.expect("Failed to connect to database");

    // Clean slate
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS conflicts CASCADE;
         DROP TABLE IF EXISTS feature_versions CASCADE;
         DROP TABLE IF EXISTS changesets CASCADE;
         DROP TABLE IF EXISTS branches CASCADE;
         DROP TABLE IF EXISTS datasets CASCADE;"
    )
    .execute(&pool)
    .await
    .unwrap();

    let store = PgStore::new(pool);
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
    };
    store.create_dataset(&ds).await.unwrap();
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
    store.create_branch(&branch).await.unwrap();
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

    let datasets = store.list_datasets().await.unwrap();
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
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(3.0, 4.0),
                    properties: json!({"name": "School"}),
                },
            ],
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
            }],
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
            }],
        )
        .await
        .unwrap();

    assert_eq!(c2.parent_id.is_some(), true);

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
            }],
        )
        .await
        .unwrap();

    store
        .commit(branch.id, "Delete", "alice", &[DiffOp::Delete { feature_id: f1 }])
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
            }],
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
            }],
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
            }],
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
            }],
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
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"b": 2}),
                },
            ],
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
            }],
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
            }],
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
            }],
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
    store.create_branch(&feature_branch).await.unwrap();

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
            }],
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
            }],
        )
        .await
        .unwrap();

    // Merge feature -> main (no conflicts: different features modified)
    let result = store.merge(feature_branch.id, main.id, "alice").await.unwrap();
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
            }],
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
    store.create_branch(&feature_branch).await.unwrap();

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
            }],
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
            }],
        )
        .await
        .unwrap();

    // Merge should detect conflict
    let result = store.merge(feature_branch.id, main.id, "alice").await.unwrap();
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
            }],
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
    store.create_branch(&feature_branch).await.unwrap();

    // Both sides delete the same feature — should NOT conflict
    store
        .commit(
            main.id,
            "Alice deletes",
            "alice",
            &[DiffOp::Delete { feature_id: f1 }],
        )
        .await
        .unwrap();

    store
        .commit(
            feature_branch.id,
            "Bob also deletes",
            "bob",
            &[DiffOp::Delete { feature_id: f1 }],
        )
        .await
        .unwrap();

    let result = store.merge(feature_branch.id, main.id, "alice").await.unwrap();
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
            }],
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
    store.create_branch(&feature_branch).await.unwrap();

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
            }],
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
            }],
        )
        .await
        .unwrap();

    // Merge base should be c1
    let base = store.find_merge_base(c2.id, c3.id).await.unwrap();
    assert_eq!(base, Some(c1.id));
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
            }
        } else {
            DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(i as f64, 0.0)),
                properties: Some(json!({"step": i})),
            }
        };
        store
            .commit(branch.id, &format!("Step {i}"), "alice", &[op])
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
    let c = store.commit(branch.id, "Empty", "alice", &[]).await.unwrap();
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
        .commit(branch.id, "Ghost delete", "alice", &[DiffOp::Delete { feature_id: f1 }])
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
        })
        .collect();

    store
        .commit(branch.id, "Bulk insert", "alice", &ops)
        .await
        .unwrap();

    let features = store.list_features_at_head(branch.id).await.unwrap();
    assert_eq!(features.len(), 100);
}
