//! Integration tests for the Ptolemy versioned geodatabase.
//!
//! Requires a running PostgreSQL instance with PostGIS.
//! Set DATABASE_URL env var to run these tests.
//! Example: DATABASE_URL=postgres://postgres:postgres@localhost/ptolemy_test cargo test

use ptolemy_core::branch::Branch;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::{DiffOp, NativeGeometry};
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

/// The backfill runs the real migration file, not a copy of its SQL, inside a
/// transaction that is rolled back so it cannot grant on another test's rows.
/// It is written to be re-runnable, which is what makes that possible.
#[tokio::test]
async fn test_backfill_grants_admin_to_the_creator_but_not_to_a_placeholder() {
    let store = setup().await;
    let owned = create_test_dataset(&store).await;
    store
        .grant_dataset_permission(owned.id, "someone-else", "write", "root")
        .await
        .unwrap();

    let mut tx = store.unguarded_pool().begin().await.unwrap();
    let mut ids = Vec::new();
    for creator in ["carol", "unknown", "  "] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO datasets (id, name, srid, geometry_type, created_by)
             VALUES ($1, $2, 4326, 'point', $3)",
        )
        .bind(id)
        .bind(format!("backfill_{id}"))
        .bind(creator)
        .execute(&mut *tx)
        .await
        .unwrap();
        ids.push(id);
    }

    sqlx::raw_sql(include_str!(
        "../migrations/027_backfill_creator_admin_grants.sql"
    ))
    .execute(&mut *tx)
    .await
    .unwrap();

    let grants = |dataset_id: Uuid| {
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT user_id, permission, granted_by FROM dataset_permissions
              WHERE dataset_id = $1",
        )
        .bind(dataset_id)
    };

    let carol = grants(ids[0]).fetch_all(&mut *tx).await.unwrap();
    assert_eq!(
        carol,
        vec![("carol".into(), "admin".into(), "carol".into())],
        "the creator did not become admin"
    );
    for (id, who) in [(ids[1], "unknown"), (ids[2], "blank")] {
        assert!(
            grants(id).fetch_all(&mut *tx).await.unwrap().is_empty(),
            "backfilled a grant to {who}"
        );
    }

    // a dataset that already had a row keeps exactly that row
    let untouched = grants(owned.id).fetch_all(&mut *tx).await.unwrap();
    assert_eq!(
        untouched,
        vec![("someone-else".into(), "write".into(), "root".into())],
        "the backfill added an owner to a dataset that had rows"
    );

    tx.rollback().await.unwrap();
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(3.0, 4.0),
                    properties: json!({"name": "School"}),
                    native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"b": 2}),
                    native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
        MergeResult::AlreadyUpToDate => panic!("Expected a merge commit, not up to date"),
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
                native: None,
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
                native: None,
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
                native: None,
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
        MergeResult::AlreadyUpToDate => panic!("Expected conflict, not up to date"),
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
                native: None,
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
        MergeResult::AlreadyUpToDate => panic!("Expected a merge commit, not up to date"),
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
        MergeResult::AlreadyUpToDate => panic!("Expected conflict, not up to date"),
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
                native: None,
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
                native: None,
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
                native: None,
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
        MergeResult::AlreadyUpToDate => panic!("Expected conflict, not up to date"),
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"name": "B"}),
                    native: None,
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
                    native: None,
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
                    native: None,
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
        MergeResult::AlreadyUpToDate => panic!("Expected conflicts, not up to date"),
    }
}

/// Edits to DIFFERENT attributes of the same feature auto-merge.
#[tokio::test]
async fn test_merge_different_attributes_same_feature_merges() {
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
                native: None,
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
                native: None,
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
                native: None,
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    let result = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    let MergeResult::Success(cs) = result else {
        panic!("disjoint attributes should merge, got {result:?}");
    };
    let merged = store
        .get_feature_at(f1, cs.id)
        .await
        .unwrap()
        .expect("feature still on main");
    assert_eq!(merged.properties["name"], "Central Park");
    assert_eq!(merged.properties["capacity"], 250);
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
                native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(0.1, 0.1)),
                    properties: Some(json!({"name": "Base v2"})),
                    native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f4,
                    geometry_wkb: point_wkb(4.0, 4.0),
                    properties: json!({"name": "Feat4"}),
                    native: None,
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
                native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({"name": "Feat2"}),
                    native: None,
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
                native: None,
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
                native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({"name": "Feat2"}),
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
            ],
            &W,
        )
        .await
        .unwrap();

    let first = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    let MergeResult::Success(merge_cs) = first else {
        panic!("Expected first merge to succeed");
    };
    assert_eq!(
        merge_cs.merge_parent_id,
        store.get_branch(feature.id).await.unwrap().head,
        "a merge commit records the source head it brought in"
    );
    let after_first = snapshot(&store, main.id).await;
    let head_after_first = store.get_branch(main.id).await.unwrap().head;

    // Re-merging an already-merged branch is up to date: no conflict, no state
    // change, and no changeset
    let second = store.merge(feature.id, main.id, "alice", &W).await.unwrap();
    assert!(
        matches!(second, MergeResult::AlreadyUpToDate),
        "re-merge of a merged branch must be up to date"
    );
    assert_eq!(snapshot(&store, main.id).await, after_first);
    assert_eq!(
        store.get_branch(main.id).await.unwrap().head,
        head_after_first,
        "an up-to-date merge must not commit"
    );
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
        native: None,
        valid_from: None,
        valid_to: None,
    }];
    let ops_b = [DiffOp::Insert {
        feature_id: fb,
        geometry_wkb: point_wkb(2.0, 2.0),
        properties: json!({"who": "b"}),
        native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Update {
                    feature_id: f1,
                    geometry_wkb: Some(point_wkb(1.0, 1.0)),
                    properties: Some(json!({"v": 2})),
                    native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f2,
                    geometry_wkb: point_wkb(1.0, 1.0),
                    properties: json!({"name": "School"}),
                    native: None,
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: f3,
                    geometry_wkb: point_wkb(3.0, 3.0),
                    properties: json!({"name": "Cafe"}),
                    native: None,
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
            .fetch_all(store.read_pool())
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
                native: None,
                valid_from: None,
                valid_to: None,
            }
        } else {
            DiffOp::Update {
                feature_id: f1,
                geometry_wkb: Some(point_wkb(i as f64, 0.0)),
                properties: Some(json!({"step": i})),
                native: None,
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
            native: None,
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
                native: None,
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
        .fetch_one(store.read_pool())
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
        .fetch_one(store.read_pool())
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
                    native: None,
                    valid_from: None,
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: Uuid::now_v7(),
                    geometry_wkb: linestring_wkb(),
                    properties: json!({}),
                    native: None,
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
                    native: None,
                    valid_from: Some(ts(1_000)),
                    valid_to: Some(ts(2_000)),
                },
                DiffOp::Insert {
                    feature_id: untimed,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({}),
                    native: None,
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
                    native: None,
                    valid_from: Some(ts(1_000)),
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: open_start,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({}),
                    native: None,
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
                native: None,
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
                native: None,
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
                    native: None,
                    valid_from: Some(ts(1_000)),
                    valid_to: Some(ts(2_000)),
                },
                DiffOp::Insert {
                    feature_id: open_end,
                    geometry_wkb: point_wkb(2.0, 2.0),
                    properties: json!({}),
                    native: None,
                    valid_from: Some(ts(1_500)),
                    valid_to: None,
                },
                DiffOp::Insert {
                    feature_id: open_start,
                    geometry_wkb: point_wkb(3.0, 3.0),
                    properties: json!({}),
                    native: None,
                    valid_from: None,
                    valid_to: Some(ts(1_500)),
                },
                DiffOp::Insert {
                    feature_id: untimed,
                    geometry_wkb: point_wkb(4.0, 4.0),
                    properties: json!({}),
                    native: None,
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
        .execute(store.unguarded_pool())
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

// ═══════════════════════════════════════════════════════════════════════
// Attachment Tombstones
// ═══════════════════════════════════════════════════════════════════════

/// One feature attachment on a branch, of `size` bytes.
fn attachment_of(
    feature_id: Uuid,
    branch_id: Uuid,
    name: &str,
    size: usize,
) -> ptolemy_storage::Attachment {
    ptolemy_storage::Attachment {
        id: Uuid::now_v7(),
        feature_id: Some(feature_id),
        branch_id: Some(branch_id),
        dataset_id: None,
        name: name.to_string(),
        content_type: "image/png".to_string(),
        size_bytes: size as i64,
        data: vec![0u8; size],
        thumbnail: None,
        metadata: json!({}),
        created_by: "test".to_string(),
        created_at: OffsetDateTime::now_utc(),
    }
}

/// A deleted attachment is a tombstone rather than a gone row, and every read
/// has to answer as though it were gone: the listing leaves it out and the two
/// single-attachment reads refuse it by not-found.
#[tokio::test]
async fn test_deleted_attachment_is_invisible_to_every_read() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;
    let feature = Uuid::now_v7();

    let kept = attachment_of(feature, branch.id, "kept.png", 4);
    let going = attachment_of(feature, branch.id, "going.png", 9);
    store.create_attachment(&kept).await.unwrap();
    store.create_attachment(&going).await.unwrap();
    assert_eq!(
        store
            .list_attachments(feature, branch.id)
            .await
            .unwrap()
            .len(),
        2
    );

    store.delete_attachment(going.id).await.unwrap();

    // the listing, which is what a size total is added up from: the tombstone's
    // bytes are not part of what the dataset holds any more
    let held = store.list_attachments(feature, branch.id).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, kept.id);
    assert_eq!(held.iter().map(|meta| meta.size_bytes).sum::<i64>(), 4);

    // the blob read a download runs, and the one a meta read runs
    assert!(store.get_attachment(going.id).await.is_err());
    assert!(store.get_attachment(kept.id).await.is_ok());

    // the row is still there, which is what a change file reports the delete off
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments WHERE id = $1")
        .bind(going.id)
        .fetch_one(store.unguarded_pool())
        .await
        .unwrap();
    assert_eq!(live, 1);
}

/// The same refusal a second delete met when the row was hard deleted: the
/// ladder resolves a tombstone to nothing, so it is not found.
#[tokio::test]
async fn test_deleting_a_tombstoned_attachment_is_refused_as_not_found() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;
    let feature = Uuid::now_v7();

    let held = attachment_of(feature, branch.id, "once.png", 3);
    store.create_attachment(&held).await.unwrap();
    store
        .ensure_attachment_writable(held.id, &W)
        .await
        .expect("live");
    store.delete_attachment(held.id).await.unwrap();

    let refused = store.ensure_attachment_writable(held.id, &W).await;
    assert!(
        matches!(refused, Err(ptolemy_storage::StoreError::NotFound(_))),
        "{refused:?}"
    );

    // and the tombstone keeps the time it was first stamped with, so a window
    // reports the delete when it happened rather than when it was asked for again
    let first: OffsetDateTime =
        sqlx::query_scalar("SELECT deleted_at FROM attachments WHERE id = $1")
            .bind(held.id)
            .fetch_one(store.unguarded_pool())
            .await
            .unwrap();
    store.delete_attachment(held.id).await.unwrap();
    let again: OffsetDateTime =
        sqlx::query_scalar("SELECT deleted_at FROM attachments WHERE id = $1")
            .bind(held.id)
            .fetch_one(store.unguarded_pool())
            .await
            .unwrap();
    assert_eq!(first, again);
}

/// A dataset attachment is tombstoned the same way, and the dataset listing is
/// the read that has to leave it out.
#[tokio::test]
async fn test_deleted_dataset_attachment_leaves_the_dataset_listing() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;

    let mut held = attachment_of(Uuid::now_v7(), Uuid::now_v7(), "icon.png", 5);
    held.feature_id = None;
    held.branch_id = None;
    held.dataset_id = Some(ds.id);
    store.create_attachment(&held).await.unwrap();
    assert_eq!(
        store.list_dataset_attachments(ds.id).await.unwrap().len(),
        1
    );

    store.delete_attachment(held.id).await.unwrap();
    assert!(
        store
            .list_dataset_attachments(ds.id)
            .await
            .unwrap()
            .is_empty()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Native Geometry Tests
// ═══════════════════════════════════════════════════════════════════════

/// The original must come back byte for byte, so the coordinates use every
/// mantissa bit a survey double could.
fn native_point() -> Vec<u8> {
    point_wkb(500000.123456789, 4649776.987654321)
}

async fn insert_with_native(
    store: &PgStore,
    branch_id: Uuid,
    fid: Uuid,
    native: Option<NativeGeometry>,
) {
    store
        .commit(
            branch_id,
            "import",
            "alice",
            &[DiffOp::Insert {
                feature_id: fid,
                geometry_wkb: point_wkb(-69.99, 41.99),
                properties: json!({}),
                native,
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_native_geometry_round_trip_exact() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;
    let fid = Uuid::now_v7();

    let original = native_point();
    insert_with_native(
        &store,
        branch.id,
        fid,
        NativeGeometry::epsg(original.clone(), 26919),
    )
    .await;

    let native = store
        .native_geometry(branch.id, fid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(native.wkb(), original.as_slice());
    assert_eq!(native.srid(), Some(26919));
}

#[tokio::test]
async fn test_native_geometry_none_without_original() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;
    let fid = Uuid::now_v7();

    insert_with_native(&store, branch.id, fid, None).await;

    assert!(
        store
            .native_geometry(branch.id, fid)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_native_geometry_not_inherited_on_update() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;
    let fid = Uuid::now_v7();

    insert_with_native(
        &store,
        branch.id,
        fid,
        NativeGeometry::epsg(native_point(), 26919),
    )
    .await;

    // an edit's new version has no original, even though the import's does
    store
        .commit(
            branch.id,
            "edit",
            "bob",
            &[DiffOp::Update {
                feature_id: fid,
                geometry_wkb: Some(point_wkb(-70.01, 42.01)),
                properties: None,
                native: None,
                valid_from: None,
                valid_to: None,
            }],
            &W,
        )
        .await
        .unwrap();

    assert!(
        store
            .native_geometry(branch.id, fid)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_native_geometry_survives_merge() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let main = create_test_branch(&store, ds.id, "main").await;

    let base = Uuid::now_v7();
    insert_with_native(&store, main.id, base, None).await;

    let main_updated = store.get_branch(main.id).await.unwrap();
    let feature_branch = Branch {
        id: Uuid::now_v7(),
        dataset_id: ds.id,
        name: "import".to_string(),
        head: main_updated.head,
        created_at: OffsetDateTime::now_utc(),
        created_by: "bob".to_string(),
    };
    store.create_branch(&feature_branch, &W).await.unwrap();

    let fid = Uuid::now_v7();
    let original = native_point();
    insert_with_native(
        &store,
        feature_branch.id,
        fid,
        NativeGeometry::epsg(original.clone(), 26919),
    )
    .await;
    // a second feature whose reference only a WKT can name, so the merge is
    // proven to carry both ways of saying one
    let fid_wkt = Uuid::now_v7();
    insert_with_native(
        &store,
        feature_branch.id,
        fid_wkt,
        NativeGeometry::wkt(original.clone(), COMPOUND_WKT.into()),
    )
    .await;

    let result = store
        .merge(feature_branch.id, main.id, "alice", &W)
        .await
        .unwrap();
    assert!(matches!(result, MergeResult::Success { .. }));

    let native = store.native_geometry(main.id, fid).await.unwrap().unwrap();
    assert_eq!(native.wkb(), original.as_slice());
    assert_eq!(native.srid(), Some(26919));

    let native = store
        .native_geometry(main.id, fid_wkt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(native.wkb(), original.as_slice());
    assert_eq!(native.srid(), None);
    assert_eq!(native.crs_wkt(), Some(COMPOUND_WKT));
}

/// A reference no single EPSG code names, abbreviated: storage keeps the
/// string as given, so nothing here depends on it being resolvable.
const COMPOUND_WKT: &str =
    "COMPD_CS[\"NAD83 + NAVD88 height\",GEOGCS[\"NAD83\"],VERT_CS[\"NAVD88 height\"]]";

#[tokio::test]
async fn test_native_geometry_wkt_round_trip_exact() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;
    let fid = Uuid::now_v7();

    let original = native_point();
    insert_with_native(
        &store,
        branch.id,
        fid,
        NativeGeometry::wkt(original.clone(), COMPOUND_WKT.into()),
    )
    .await;

    let native = store
        .native_geometry(branch.id, fid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(native.wkb(), original.as_slice());
    assert_eq!(native.srid(), None);
    assert_eq!(native.crs_wkt(), Some(COMPOUND_WKT));
}

#[tokio::test]
async fn test_native_geometry_unknown_feature_not_found() {
    let store = setup().await;
    let ds = create_test_dataset(&store).await;
    let branch = create_test_branch(&store, ds.id, "main").await;

    assert!(
        store
            .native_geometry(branch.id, Uuid::now_v7())
            .await
            .is_err()
    );
}
