// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! PostgreSQL/PostGIS backend for the versioned feature store.

use ptolemy_core::Feature;
use ptolemy_core::branch::Branch;
use ptolemy_core::changeset::Changeset;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::{Diff, DiffOp};
use ptolemy_core::event::{Event, Webhook};
use ptolemy_core::review::{MergeRequest, MergeRequestStatus, ReviewComment};
use ptolemy_core::schema::{
    DatasetSchema, FieldDef, GeometryRules, QualityReport, QualityStatistics, TopologyRule,
};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Run migrations embedded in this crate.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let sql_001 = include_str!("../migrations/001_initial.sql");
        sqlx::raw_sql(sql_001).execute(&self.pool).await?;
        let sql_002 = include_str!("../migrations/002_reviews.sql");
        sqlx::raw_sql(sql_002).execute(&self.pool).await?;
        let sql_003 = include_str!("../migrations/003_schema_topology.sql");
        sqlx::raw_sql(sql_003).execute(&self.pool).await?;
        let sql_004 = include_str!("../migrations/004_webhooks.sql");
        sqlx::raw_sql(sql_004).execute(&self.pool).await?;
        let sql_005 = include_str!("../migrations/005_audit.sql");
        sqlx::raw_sql(sql_005).execute(&self.pool).await?;
        let sql_006 = include_str!("../migrations/006_locks.sql");
        sqlx::raw_sql(sql_006).execute(&self.pool).await?;
        let sql_007 = include_str!("../migrations/007_catalog.sql");
        sqlx::raw_sql(sql_007).execute(&self.pool).await?;
        let sql_008 = include_str!("../migrations/008_tenancy.sql");
        sqlx::raw_sql(sql_008).execute(&self.pool).await?;
        let sql_009 = include_str!("../migrations/009_networks.sql");
        sqlx::raw_sql(sql_009).execute(&self.pool).await?;
        let sql_010 = include_str!("../migrations/010_linear_ref.sql");
        sqlx::raw_sql(sql_010).execute(&self.pool).await?;
        let sql_011 = include_str!("../migrations/011_raster.sql");
        sqlx::raw_sql(sql_011).execute(&self.pool).await?;
        let sql_012 = include_str!("../migrations/012_domains_rules.sql");
        sqlx::raw_sql(sql_012).execute(&self.pool).await?;
        let sql_013 = include_str!("../migrations/013_relationships.sql");
        sqlx::raw_sql(sql_013).execute(&self.pool).await?;
        let sql_014 = include_str!("../migrations/014_cartography.sql");
        sqlx::raw_sql(sql_014).execute(&self.pool).await?;
        let sql_015 = include_str!("../migrations/015_extensions.sql");
        sqlx::raw_sql(sql_015).execute(&self.pool).await?;
        let sql_016 = include_str!("../migrations/016_features_view.sql");
        sqlx::raw_sql(sql_016).execute(&self.pool).await?;
        Ok(())
    }

    // ─── Dataset CRUD ───────────────────────────────────────────────

    pub async fn create_dataset(&self, ds: &Dataset) -> Result<(), StoreError> {
        let geom_type = format!("{:?}", ds.geometry_type).to_lowercase();
        sqlx::query(
            "INSERT INTO datasets (id, name, srid, geometry_type, created_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(ds.id)
        .bind(&ds.name)
        .bind(ds.srid)
        .bind(&geom_type)
        .bind(ds.created_at)
        .bind(&ds.created_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_dataset(&self, id: Uuid) -> Result<Dataset, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, srid, geometry_type, created_at, created_by FROM datasets WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("dataset {id}")))?;

        Ok(Dataset {
            id: row.get("id"),
            name: row.get("name"),
            srid: row.get("srid"),
            geometry_type: parse_geometry_type(row.get::<String, _>("geometry_type")),
            created_at: row.get("created_at"),
            created_by: row.get("created_by"),
        })
    }

    pub async fn list_datasets(&self) -> Result<Vec<Dataset>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, srid, geometry_type, created_at, created_by FROM datasets ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Dataset {
                id: row.get("id"),
                name: row.get("name"),
                srid: row.get("srid"),
                geometry_type: parse_geometry_type(row.get::<String, _>("geometry_type")),
                created_at: row.get("created_at"),
                created_by: row.get("created_by"),
            })
            .collect())
    }

    // ─── Branch CRUD ────────────────────────────────────────────────

    pub async fn create_branch(&self, branch: &Branch) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO branches (id, dataset_id, name, head, created_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(branch.id)
        .bind(branch.dataset_id)
        .bind(&branch.name)
        .bind(branch.head)
        .bind(branch.created_at)
        .bind(&branch.created_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_branch(&self, id: Uuid) -> Result<Branch, StoreError> {
        let row = sqlx::query(
            "SELECT id, dataset_id, name, head, created_at, created_by FROM branches WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("branch {id}")))?;

        Ok(Branch {
            id: row.get("id"),
            dataset_id: row.get("dataset_id"),
            name: row.get("name"),
            head: row.get("head"),
            created_at: row.get("created_at"),
            created_by: row.get("created_by"),
        })
    }

    pub async fn list_branches(&self, dataset_id: Uuid) -> Result<Vec<Branch>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, name, head, created_at, created_by FROM branches WHERE dataset_id = $1 ORDER BY name",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Branch {
                id: row.get("id"),
                dataset_id: row.get("dataset_id"),
                name: row.get("name"),
                head: row.get("head"),
                created_at: row.get("created_at"),
                created_by: row.get("created_by"),
            })
            .collect())
    }

    // ─── Changeset / Commit ─────────────────────────────────────────

    /// Create a new changeset and advance the branch head.
    /// Validate commit operations against the dataset schema (if one exists).
    /// Returns validation errors. Empty = all valid.
    pub async fn validate_commit(
        &self,
        dataset_id: Uuid,
        operations: &[DiffOp],
    ) -> Result<Vec<ptolemy_core::schema::ValidationError>, StoreError> {
        let schema = self.get_dataset_schema(dataset_id).await?;
        let Some(schema) = schema else {
            return Ok(vec![]); // No schema = no validation
        };

        let mut errors = Vec::new();
        for op in operations {
            match op {
                DiffOp::Insert {
                    feature_id,
                    properties,
                    ..
                }
                | DiffOp::Update {
                    feature_id,
                    properties: Some(properties),
                    ..
                } => {
                    let errs = schema.validate_properties(*feature_id, properties);
                    errors.extend(errs);
                }
                _ => {}
            }
        }
        Ok(errors)
    }

    pub async fn commit(
        &self,
        branch_id: Uuid,
        message: &str,
        author: &str,
        operations: &[DiffOp],
    ) -> Result<Changeset, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Get current branch head
        let branch_row =
            sqlx::query("SELECT head, dataset_id FROM branches WHERE id = $1 FOR UPDATE")
                .bind(branch_id)
                .fetch_one(&mut *tx)
                .await?;
        let parent_id: Option<Uuid> = branch_row.get("head");
        let dataset_id: Uuid = branch_row.get("dataset_id");

        // Create changeset
        let changeset_id = Uuid::now_v7();
        let now = OffsetDateTime::now_utc();
        sqlx::query(
            "INSERT INTO changesets (id, branch_id, parent_id, message, author, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(changeset_id)
        .bind(branch_id)
        .bind(parent_id)
        .bind(message)
        .bind(author)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        // Apply operations as feature_versions
        for op in operations {
            match op {
                DiffOp::Insert {
                    feature_id,
                    geometry_wkb,
                    properties,
                } => {
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties)
                         VALUES ($1, $2, $3, 'insert', ST_GeomFromWKB($4, 4326), $5)",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .bind(geometry_wkb)
                    .bind(properties)
                    .execute(&mut *tx)
                    .await?;
                }
                DiffOp::Update {
                    feature_id,
                    geometry_wkb,
                    properties,
                } => {
                    let geom = if let Some(wkb) = geometry_wkb {
                        wkb.clone()
                    } else {
                        let row = sqlx::query(
                            "SELECT ST_AsBinary(geometry) as geom FROM feature_versions
                             WHERE feature_id = $1 AND operation != 'delete'
                             ORDER BY created_at DESC LIMIT 1",
                        )
                        .bind(feature_id)
                        .fetch_one(&mut *tx)
                        .await?;
                        row.get::<Vec<u8>, _>("geom")
                    };
                    let props = if let Some(p) = properties {
                        p.clone()
                    } else {
                        let row = sqlx::query(
                            "SELECT properties FROM feature_versions
                             WHERE feature_id = $1 AND operation != 'delete'
                             ORDER BY created_at DESC LIMIT 1",
                        )
                        .bind(feature_id)
                        .fetch_one(&mut *tx)
                        .await?;
                        row.get::<serde_json::Value, _>("properties")
                    };
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties)
                         VALUES ($1, $2, $3, 'update', ST_GeomFromWKB($4, 4326), $5)",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .bind(&geom)
                    .bind(&props)
                    .execute(&mut *tx)
                    .await?;
                }
                DiffOp::Delete { feature_id } => {
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties)
                         VALUES ($1, $2, $3, 'delete', NULL, '{}')",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }

        // Advance branch head
        sqlx::query("UPDATE branches SET head = $1 WHERE id = $2")
            .bind(changeset_id)
            .bind(branch_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(Changeset {
            id: changeset_id,
            branch_id,
            parent_id,
            message: message.to_string(),
            author: author.to_string(),
            created_at: now,
        })
    }

    // ─── Feature Queries ────────────────────────────────────────────

    /// Get the current state of all features on a branch (at its head).
    pub async fn list_features_at_head(&self, branch_id: Uuid) -> Result<Vec<Feature>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.dataset_id, fv.operation,
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, dataset_id, geometry_wkb, properties
            FROM latest
            WHERE operation != 'delete'",
        )
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
            })
            .collect())
    }

    /// Get a single feature's state at a specific changeset.
    pub async fn get_feature_at(
        &self,
        feature_id: Uuid,
        changeset_id: Uuid,
    ) -> Result<Option<Feature>, StoreError> {
        let row = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT id, parent_id FROM changesets WHERE id = $2
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
            )
            SELECT fv.feature_id, fv.dataset_id, fv.operation,
                   ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            WHERE fv.feature_id = $1
            ORDER BY fv.created_at DESC
            LIMIT 1",
        )
        .bind(feature_id)
        .bind(changeset_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) if r.get::<String, _>("operation") != "delete" => Ok(Some(Feature {
                id: r.get("feature_id"),
                dataset_id: r.get("dataset_id"),
                geometry_wkb: r.get("geometry_wkb"),
                properties: r.get("properties"),
            })),
            _ => Ok(None),
        }
    }

    // ─── Diff ───────────────────────────────────────────────────────

    /// Compute the diff between two changesets (what changed from `from` to `to`).
    pub async fn diff(
        &self,
        from_changeset: Option<Uuid>,
        to_changeset: Uuid,
    ) -> Result<Diff, StoreError> {
        let rows = if let Some(from_id) = from_changeset {
            sqlx::query(
                "WITH RECURSIVE
                to_chain AS (
                    SELECT id, parent_id FROM changesets WHERE id = $2
                  UNION ALL
                    SELECT c.id, c.parent_id FROM changesets c JOIN to_chain ch ON ch.parent_id = c.id
                ),
                from_chain AS (
                    SELECT id, parent_id FROM changesets WHERE id = $1
                  UNION ALL
                    SELECT c.id, c.parent_id FROM changesets c JOIN from_chain ch ON ch.parent_id = c.id
                ),
                new_changesets AS (
                    SELECT id FROM to_chain EXCEPT SELECT id FROM from_chain
                )
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation,
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                FROM feature_versions fv
                JOIN new_changesets nc ON fv.changeset_id = nc.id
                ORDER BY fv.feature_id, fv.created_at DESC",
            )
            .bind(from_id)
            .bind(to_changeset)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "WITH RECURSIVE chain AS (
                    SELECT id, parent_id FROM changesets WHERE id = $1
                  UNION ALL
                    SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
                )
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation,
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC",
            )
            .bind(to_changeset)
            .fetch_all(&self.pool)
            .await?
        };

        let operations = rows
            .into_iter()
            .map(|row| {
                let op: String = row.get("operation");
                let feature_id: Uuid = row.get("feature_id");
                match op.as_str() {
                    "insert" => DiffOp::Insert {
                        feature_id,
                        geometry_wkb: row.get("geometry_wkb"),
                        properties: row.get("properties"),
                    },
                    "update" => DiffOp::Update {
                        feature_id,
                        geometry_wkb: Some(row.get("geometry_wkb")),
                        properties: Some(row.get("properties")),
                    },
                    "delete" => DiffOp::Delete { feature_id },
                    _ => unreachable!(),
                }
            })
            .collect();

        Ok(Diff {
            from_changeset,
            to_changeset,
            operations,
        })
    }

    // ─── Merge ──────────────────────────────────────────────────────

    /// Find the common ancestor of two changesets (merge base).
    pub async fn find_merge_base(
        &self,
        changeset_a: Uuid,
        changeset_b: Uuid,
    ) -> Result<Option<Uuid>, StoreError> {
        let row = sqlx::query(
            "WITH RECURSIVE
            ancestors_a AS (
                SELECT id, parent_id FROM changesets WHERE id = $1
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c JOIN ancestors_a a ON a.parent_id = c.id
            ),
            ancestors_b AS (
                SELECT id, parent_id FROM changesets WHERE id = $2
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c JOIN ancestors_b b ON b.parent_id = c.id
            )
            SELECT a.id FROM ancestors_a a
            JOIN ancestors_b b ON a.id = b.id
            LIMIT 1",
        )
        .bind(changeset_a)
        .bind(changeset_b)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.get("id")))
    }

    /// Three-way merge: merge `source_branch` into `target_branch`.
    /// Returns the merge changeset, or a list of conflicts if any exist.
    pub async fn merge(
        &self,
        source_branch_id: Uuid,
        target_branch_id: Uuid,
        author: &str,
    ) -> Result<MergeResult, StoreError> {
        let source = self.get_branch(source_branch_id).await?;
        let target = self.get_branch(target_branch_id).await?;

        let source_head = source
            .head
            .ok_or_else(|| StoreError::Conflict("source branch has no commits".into()))?;
        let target_head = target
            .head
            .ok_or_else(|| StoreError::Conflict("target branch has no commits".into()))?;

        // Find merge base
        let base = self.find_merge_base(source_head, target_head).await?;

        // Compute diffs from base to each head
        let diff_ours = self.diff(base, target_head).await?;
        let diff_theirs = self.diff(base, source_head).await?;

        // Build maps of feature_id -> operation
        let ours_map: std::collections::HashMap<Uuid, &DiffOp> = diff_ours
            .operations
            .iter()
            .map(|op| (op_feature_id(op), op))
            .collect();
        let theirs_map: std::collections::HashMap<Uuid, &DiffOp> = diff_theirs
            .operations
            .iter()
            .map(|op| (op_feature_id(op), op))
            .collect();

        let mut merged_ops: Vec<DiffOp> = Vec::new();
        let mut conflicts: Vec<ConflictInfo> = Vec::new();

        // All features touched by either side
        let all_features: std::collections::HashSet<Uuid> =
            ours_map.keys().chain(theirs_map.keys()).copied().collect();

        for fid in all_features {
            match (ours_map.get(&fid), theirs_map.get(&fid)) {
                (Some(ours), None) => {
                    merged_ops.push((*ours).clone());
                }
                (None, Some(theirs)) => {
                    merged_ops.push((*theirs).clone());
                }
                (Some(ours), Some(theirs)) => {
                    if ops_equal(ours, theirs) {
                        merged_ops.push((*ours).clone());
                    } else {
                        conflicts.push(ConflictInfo {
                            feature_id: fid,
                            ours: (*ours).clone(),
                            theirs: (*theirs).clone(),
                        });
                    }
                }
                (None, None) => unreachable!(),
            }
        }

        if !conflicts.is_empty() {
            return Ok(MergeResult::Conflicts(conflicts));
        }

        // No conflicts — create merge commit on target branch
        let changeset = self
            .commit(
                target_branch_id,
                &format!("Merge branch '{}' into '{}'", source.name, target.name),
                author,
                &merged_ops,
            )
            .await?;

        Ok(MergeResult::Success(changeset))
    }

    // ─── History ────────────────────────────────────────────────────

    pub async fn get_branch_history(
        &self,
        branch_id: Uuid,
        limit: i64,
    ) -> Result<Vec<Changeset>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.* FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.* FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            )
            SELECT id, branch_id, parent_id, message, author, created_at
            FROM chain
            LIMIT $2",
        )
        .bind(branch_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Changeset {
                id: row.get("id"),
                branch_id: row.get("branch_id"),
                parent_id: row.get("parent_id"),
                message: row.get("message"),
                author: row.get("author"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    // ─── Paginated Feature List ─────────────────────────────────────

    /// List features with cursor-based pagination.
    pub async fn list_features_paginated(
        &self,
        branch_id: Uuid,
        cursor: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Feature>, StoreError> {
        let query = if let Some(cursor_id) = cursor {
            sqlx::query(
                "WITH RECURSIVE chain AS (
                    SELECT c.id, c.parent_id
                    FROM changesets c
                    JOIN branches b ON b.head = c.id
                    WHERE b.id = $1
                  UNION ALL
                    SELECT c.id, c.parent_id
                    FROM changesets c
                    JOIN chain ch ON ch.parent_id = c.id
                ),
                latest AS (
                    SELECT DISTINCT ON (fv.feature_id)
                        fv.feature_id, fv.dataset_id, fv.operation,
                        ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                    FROM feature_versions fv
                    JOIN chain ch ON fv.changeset_id = ch.id
                    ORDER BY fv.feature_id, fv.created_at DESC
                )
                SELECT feature_id, dataset_id, geometry_wkb, properties
                FROM latest
                WHERE operation != 'delete' AND feature_id > $2
                ORDER BY feature_id
                LIMIT $3",
            )
            .bind(branch_id)
            .bind(cursor_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "WITH RECURSIVE chain AS (
                    SELECT c.id, c.parent_id
                    FROM changesets c
                    JOIN branches b ON b.head = c.id
                    WHERE b.id = $1
                  UNION ALL
                    SELECT c.id, c.parent_id
                    FROM changesets c
                    JOIN chain ch ON ch.parent_id = c.id
                ),
                latest AS (
                    SELECT DISTINCT ON (fv.feature_id)
                        fv.feature_id, fv.dataset_id, fv.operation,
                        ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                    FROM feature_versions fv
                    JOIN chain ch ON fv.changeset_id = ch.id
                    ORDER BY fv.feature_id, fv.created_at DESC
                )
                SELECT feature_id, dataset_id, geometry_wkb, properties
                FROM latest
                WHERE operation != 'delete'
                ORDER BY feature_id
                LIMIT $2",
            )
            .bind(branch_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(query
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
            })
            .collect())
    }

    // ─── Spatial Queries ────────────────────────────────────────────

    /// Get features within a bounding box.
    pub async fn features_in_bbox(
        &self,
        branch_id: Uuid,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
        limit: i64,
    ) -> Result<Vec<Feature>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.dataset_id, fv.operation,
                    fv.geometry, ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, dataset_id, geometry_wkb, properties
            FROM latest
            WHERE operation != 'delete'
              AND geometry && ST_MakeEnvelope($2, $3, $4, $5, 4326)
            LIMIT $6",
        )
        .bind(branch_id)
        .bind(min_x)
        .bind(min_y)
        .bind(max_x)
        .bind(max_y)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
            })
            .collect())
    }

    /// Get features intersecting a GeoJSON geometry.
    pub async fn features_intersecting(
        &self,
        branch_id: Uuid,
        geojson_geometry: &str,
        limit: i64,
    ) -> Result<Vec<Feature>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.dataset_id, fv.operation,
                    fv.geometry, ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, dataset_id, geometry_wkb, properties
            FROM latest
            WHERE operation != 'delete'
              AND ST_Intersects(geometry, ST_GeomFromGeoJSON($2))
            LIMIT $3",
        )
        .bind(branch_id)
        .bind(geojson_geometry)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
            })
            .collect())
    }

    /// Get features contained within a GeoJSON geometry.
    pub async fn features_within(
        &self,
        branch_id: Uuid,
        geojson_geometry: &str,
        limit: i64,
    ) -> Result<Vec<Feature>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.dataset_id, fv.operation,
                    fv.geometry, ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, dataset_id, geometry_wkb, properties
            FROM latest
            WHERE operation != 'delete'
              AND ST_Within(geometry, ST_GeomFromGeoJSON($2))
            LIMIT $3",
        )
        .bind(branch_id)
        .bind(geojson_geometry)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
            })
            .collect())
    }

    /// Count features at branch head.
    pub async fn count_features_at_head(&self, branch_id: Uuid) -> Result<i64, StoreError> {
        let row = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT COUNT(*) as cnt
            FROM latest
            WHERE operation != 'delete'",
        )
        .bind(branch_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get::<i64, _>("cnt"))
    }

    // ─── MVT Tile Generation ────────────────────────────────────────

    /// Generate a Mapbox Vector Tile for features on a branch at the given z/x/y.
    pub async fn get_mvt_tile(
        &self,
        branch_id: Uuid,
        z: u32,
        x: u32,
        y: u32,
    ) -> Result<Vec<u8>, StoreError> {
        let row = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN branches b ON b.head = c.id
                WHERE b.id = $1
              UNION ALL
                SELECT c.id, c.parent_id
                FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation, fv.geometry, fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            ),
            bounds AS (
                SELECT ST_TileEnvelope($2::integer, $3::integer, $4::integer) AS geom
            ),
            mvtgeom AS (
                SELECT ST_AsMVTGeom(
                    ST_Transform(l.geometry, 3857),
                    b.geom,
                    4096, 64, true
                ) AS geom,
                l.feature_id,
                l.properties
                FROM latest l, bounds b
                WHERE l.operation != 'delete'
                  AND l.geometry IS NOT NULL
                  AND ST_Intersects(l.geometry, ST_Transform(b.geom, 4326))
            )
            SELECT COALESCE(ST_AsMVT(mvtgeom.*, 'features', 4096, 'geom'), ''::bytea) AS tile
            FROM mvtgeom",
        )
        .bind(branch_id)
        .bind(z as i32)
        .bind(x as i32)
        .bind(y as i32)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get::<Vec<u8>, _>("tile"))
    }

    // ─── Merge Requests (Reviews) ───────────────────────────────────

    pub async fn create_merge_request(&self, mr: &MergeRequest) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO merge_requests (id, dataset_id, source_branch_id, target_branch_id, title, description, author, status, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(mr.id)
        .bind(mr.dataset_id)
        .bind(mr.source_branch_id)
        .bind(mr.target_branch_id)
        .bind(&mr.title)
        .bind(&mr.description)
        .bind(&mr.author)
        .bind(format!("{:?}", mr.status).to_lowercase())
        .bind(mr.created_at)
        .bind(mr.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_merge_request(&self, id: Uuid) -> Result<MergeRequest, StoreError> {
        let row = sqlx::query(
            "SELECT id, dataset_id, source_branch_id, target_branch_id, title, description, author, status, created_at, updated_at
             FROM merge_requests WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("merge_request {id}")))?;

        Ok(MergeRequest {
            id: row.get("id"),
            dataset_id: row.get("dataset_id"),
            source_branch_id: row.get("source_branch_id"),
            target_branch_id: row.get("target_branch_id"),
            title: row.get("title"),
            description: row.get("description"),
            author: row.get("author"),
            status: parse_mr_status(row.get::<String, _>("status")),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    pub async fn list_merge_requests(
        &self,
        dataset_id: Uuid,
        status_filter: Option<&str>,
    ) -> Result<Vec<MergeRequest>, StoreError> {
        let rows = if let Some(status) = status_filter {
            sqlx::query(
                "SELECT id, dataset_id, source_branch_id, target_branch_id, title, description, author, status, created_at, updated_at
                 FROM merge_requests WHERE dataset_id = $1 AND status = $2 ORDER BY created_at DESC",
            )
            .bind(dataset_id)
            .bind(status)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, dataset_id, source_branch_id, target_branch_id, title, description, author, status, created_at, updated_at
                 FROM merge_requests WHERE dataset_id = $1 ORDER BY created_at DESC",
            )
            .bind(dataset_id)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(|row| MergeRequest {
                id: row.get("id"),
                dataset_id: row.get("dataset_id"),
                source_branch_id: row.get("source_branch_id"),
                target_branch_id: row.get("target_branch_id"),
                title: row.get("title"),
                description: row.get("description"),
                author: row.get("author"),
                status: parse_mr_status(row.get::<String, _>("status")),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }

    pub async fn update_merge_request_status(
        &self,
        id: Uuid,
        status: &MergeRequestStatus,
    ) -> Result<(), StoreError> {
        let status_str = format!("{:?}", status).to_lowercase();
        let result =
            sqlx::query("UPDATE merge_requests SET status = $1, updated_at = now() WHERE id = $2")
                .bind(&status_str)
                .bind(id)
                .execute(&self.pool)
                .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("merge_request {id}")));
        }
        Ok(())
    }

    pub async fn add_review_comment(&self, comment: &ReviewComment) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO review_comments (id, merge_request_id, feature_id, author, body, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(comment.id)
        .bind(comment.merge_request_id)
        .bind(comment.feature_id)
        .bind(&comment.author)
        .bind(&comment.body)
        .bind(comment.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_review_comments(
        &self,
        merge_request_id: Uuid,
    ) -> Result<Vec<ReviewComment>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, merge_request_id, feature_id, author, body, created_at
             FROM review_comments WHERE merge_request_id = $1 ORDER BY created_at",
        )
        .bind(merge_request_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ReviewComment {
                id: row.get("id"),
                merge_request_id: row.get("merge_request_id"),
                feature_id: row.get("feature_id"),
                author: row.get("author"),
                body: row.get("body"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    // ─── Schema & Topology ──────────────────────────────────────────

    pub async fn set_dataset_schema(&self, schema: &DatasetSchema) -> Result<(), StoreError> {
        let fields_json = serde_json::to_value(&schema.fields).unwrap();
        let rules_json = serde_json::to_value(&schema.geometry_rules).unwrap();
        sqlx::query(
            "INSERT INTO dataset_schemas (dataset_id, fields, geometry_rules)
             VALUES ($1, $2, $3)
             ON CONFLICT (dataset_id) DO UPDATE SET fields = $2, geometry_rules = $3",
        )
        .bind(schema.dataset_id)
        .bind(&fields_json)
        .bind(&rules_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_dataset_schema(
        &self,
        dataset_id: Uuid,
    ) -> Result<Option<DatasetSchema>, StoreError> {
        let row = sqlx::query(
            "SELECT dataset_id, fields, geometry_rules FROM dataset_schemas WHERE dataset_id = $1",
        )
        .bind(dataset_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let fields: Vec<FieldDef> = serde_json::from_value(r.get("fields")).unwrap_or_default();
            let geometry_rules: GeometryRules = serde_json::from_value(r.get("geometry_rules"))
                .unwrap_or(GeometryRules {
                    allowed_types: vec![],
                    bounds: None,
                    max_vertices: None,
                });
            DatasetSchema {
                dataset_id: r.get("dataset_id"),
                fields,
                geometry_rules,
            }
        }))
    }

    pub async fn add_topology_rule(&self, rule: &TopologyRule) -> Result<(), StoreError> {
        let rule_type_json = serde_json::to_value(&rule.rule_type).unwrap();
        sqlx::query(
            "INSERT INTO topology_rules (id, dataset_id, rule_type, description)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(rule.id)
        .bind(rule.dataset_id)
        .bind(&rule_type_json)
        .bind(&rule.description)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_topology_rules(
        &self,
        dataset_id: Uuid,
    ) -> Result<Vec<TopologyRule>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, rule_type, description FROM topology_rules WHERE dataset_id = $1",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let rule_type = serde_json::from_value(row.get("rule_type")).unwrap();
                TopologyRule {
                    id: row.get("id"),
                    dataset_id: row.get("dataset_id"),
                    rule_type,
                    description: row.get("description"),
                }
            })
            .collect())
    }

    pub async fn delete_topology_rule(&self, id: Uuid) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM topology_rules WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Run data quality checks on a branch and return a report.
    pub async fn quality_report(&self, branch_id: Uuid) -> Result<QualityReport, StoreError> {
        let total = self.count_features_at_head(branch_id).await?;

        // Check for null/invalid geometries
        let stats_row = sqlx::query(
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
            SELECT
                COUNT(*) FILTER (WHERE operation != 'delete' AND geometry IS NULL) as null_geom,
                COUNT(*) FILTER (WHERE operation != 'delete' AND geometry IS NOT NULL AND NOT ST_IsValid(geometry)) as invalid_geom,
                COUNT(*) FILTER (WHERE operation != 'delete') as total
            FROM latest",
        )
        .bind(branch_id)
        .fetch_one(&self.pool)
        .await?;

        let null_geometry_count: i64 = stats_row.get("null_geom");
        let invalid_geometry_count: i64 = stats_row.get("invalid_geom");
        let valid_features = total - null_geometry_count - invalid_geometry_count;

        Ok(QualityReport {
            branch_id,
            total_features: total,
            valid_features,
            errors: vec![],
            statistics: QualityStatistics {
                null_geometry_count,
                invalid_geometry_count,
                null_fields: vec![],
                out_of_bounds_count: 0,
            },
        })
    }

    /// Repair invalid geometries on a branch (creates a new commit).
    pub async fn repair_geometries(
        &self,
        branch_id: Uuid,
        author: &str,
    ) -> Result<Option<Changeset>, StoreError> {
        // Find features with invalid geometries
        let rows = sqlx::query(
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
            SELECT feature_id, ST_AsBinary(ST_MakeValid(geometry)) as fixed_geom, properties
            FROM latest
            WHERE operation != 'delete'
              AND geometry IS NOT NULL
              AND NOT ST_IsValid(geometry)",
        )
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(None);
        }

        let ops: Vec<DiffOp> = rows
            .into_iter()
            .map(|row| DiffOp::Update {
                feature_id: row.get("feature_id"),
                geometry_wkb: Some(row.get("fixed_geom")),
                properties: None,
            })
            .collect();

        let count = ops.len();
        let changeset = self
            .commit(
                branch_id,
                &format!("Auto-repair: fixed {} invalid geometries", count),
                author,
                &ops,
            )
            .await?;

        Ok(Some(changeset))
    }

    // ─── Webhooks & Events (CDC) ────────────────────────────────────

    pub async fn create_webhook(&self, wh: &Webhook) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO webhooks (id, dataset_id, url, events, secret, active)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(wh.id)
        .bind(wh.dataset_id)
        .bind(&wh.url)
        .bind(&wh.events)
        .bind(&wh.secret)
        .bind(wh.active)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_webhooks(&self, dataset_id: Uuid) -> Result<Vec<Webhook>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, url, events, secret, active FROM webhooks WHERE dataset_id = $1",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Webhook {
                id: row.get("id"),
                dataset_id: row.get("dataset_id"),
                url: row.get("url"),
                events: row.get("events"),
                secret: row.get("secret"),
                active: row.get("active"),
            })
            .collect())
    }

    pub async fn delete_webhook(&self, id: Uuid) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM webhooks WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn emit_event(
        &self,
        dataset_id: Uuid,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<Event, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO events (id, dataset_id, event_type, payload)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(dataset_id)
        .bind(event_type)
        .bind(payload)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query("SELECT created_at FROM events WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;

        Ok(Event {
            id,
            dataset_id,
            event_type: event_type.to_string(),
            payload: payload.clone(),
            created_at: row.get("created_at"),
        })
    }

    pub async fn list_events(
        &self,
        dataset_id: Uuid,
        limit: i64,
    ) -> Result<Vec<Event>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, event_type, payload, created_at
             FROM events
             WHERE dataset_id = $1
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(dataset_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Event {
                id: row.get("id"),
                dataset_id: row.get("dataset_id"),
                event_type: row.get("event_type"),
                payload: row.get("payload"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    // ─── Audit Log ──────────────────────────────────────────────────

    pub async fn audit_log(
        &self,
        actor: &str,
        action: &str,
        resource_type: &str,
        resource_id: Option<Uuid>,
        details: &serde_json::Value,
        ip_address: Option<&str>,
    ) -> Result<(), StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO audit_log (id, actor, action, resource_type, resource_id, details, ip_address)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(actor)
        .bind(action)
        .bind(resource_type)
        .bind(resource_id)
        .bind(details)
        .bind(ip_address)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_audit_log(
        &self,
        limit: i64,
        actor: Option<&str>,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        let rows = if let Some(a) = actor {
            sqlx::query(
                "SELECT id, actor, action, resource_type, resource_id, details, ip_address, created_at
                 FROM audit_log WHERE actor = $1 ORDER BY created_at DESC LIMIT $2",
            )
            .bind(a)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, actor, action, resource_type, resource_id, details, ip_address, created_at
                 FROM audit_log ORDER BY created_at DESC LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows
            .into_iter()
            .map(|row| AuditEntry {
                id: row.get("id"),
                actor: row.get("actor"),
                action: row.get("action"),
                resource_type: row.get("resource_type"),
                resource_id: row.get("resource_id"),
                details: row.get("details"),
                ip_address: row.get("ip_address"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    // ─── Temporal Queries ─────────────────────────────────────────────

    /// Get features as they existed at a specific point in time on a branch.
    pub async fn features_at_time(
        &self,
        branch_id: Uuid,
        at: OffsetDateTime,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Feature>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                -- Find the changeset that was head at the given time
                SELECT c.id, c.parent_id FROM changesets c
                WHERE c.branch_id = $1 AND c.created_at <= $2
                ORDER BY c.created_at DESC LIMIT 1
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation,
                    ST_AsGeoJSON(fv.geometry)::jsonb as geojson,
                    fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                WHERE fv.created_at <= $2
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, geojson, properties
            FROM latest
            WHERE operation != 'delete'
            LIMIT $3 OFFSET $4",
        )
        .bind(branch_id)
        .bind(at)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let geojson: Option<serde_json::Value> = row.get("geojson");
                let geom_str = geojson.map(|g| g.to_string()).unwrap_or_default();
                Feature {
                    id: row.get("feature_id"),
                    dataset_id: Uuid::nil(),
                    geometry_wkb: geom_str.into_bytes(),
                    properties: row.get("properties"),
                }
            })
            .collect())
    }

    // ─── Feature Locks ──────────────────────────────────────────────

    pub async fn lock_feature(
        &self,
        feature_id: Uuid,
        branch_id: Uuid,
        locked_by: &str,
        duration_minutes: i64,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        // Clean up expired locks first
        sqlx::query("DELETE FROM feature_locks WHERE expires_at < now()")
            .execute(&self.pool)
            .await?;

        // Check if already locked by someone else
        let existing = sqlx::query(
            "SELECT locked_by FROM feature_locks WHERE feature_id = $1 AND branch_id = $2 AND expires_at > now()",
        )
        .bind(feature_id)
        .bind(branch_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            let owner: String = row.get("locked_by");
            if owner != locked_by {
                return Err(StoreError::Conflict(format!(
                    "feature {} is locked by '{}'",
                    feature_id, owner
                )));
            }
            // Refresh lock
            sqlx::query(
                "UPDATE feature_locks SET expires_at = now() + make_interval(mins => $3), reason = $4
                 WHERE feature_id = $1 AND branch_id = $2",
            )
            .bind(feature_id)
            .bind(branch_id)
            .bind(duration_minutes as f64)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO feature_locks (feature_id, branch_id, locked_by, expires_at, reason)
                 VALUES ($1, $2, $3, now() + make_interval(mins => $4), $5)",
            )
            .bind(feature_id)
            .bind(branch_id)
            .bind(locked_by)
            .bind(duration_minutes as f64)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn unlock_feature(
        &self,
        feature_id: Uuid,
        branch_id: Uuid,
        actor: &str,
    ) -> Result<(), StoreError> {
        let existing = sqlx::query(
            "SELECT locked_by FROM feature_locks WHERE feature_id = $1 AND branch_id = $2",
        )
        .bind(feature_id)
        .bind(branch_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            let owner: String = row.get("locked_by");
            if owner != actor {
                return Err(StoreError::Conflict(format!(
                    "cannot unlock: feature {} is locked by '{}'",
                    feature_id, owner
                )));
            }
        }

        sqlx::query("DELETE FROM feature_locks WHERE feature_id = $1 AND branch_id = $2")
            .bind(feature_id)
            .bind(branch_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_locks(&self, branch_id: Uuid) -> Result<Vec<FeatureLock>, StoreError> {
        let rows = sqlx::query(
            "SELECT feature_id, branch_id, locked_by, locked_at, expires_at, reason
             FROM feature_locks WHERE branch_id = $1 AND expires_at > now()",
        )
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| FeatureLock {
                feature_id: row.get("feature_id"),
                branch_id: row.get("branch_id"),
                locked_by: row.get("locked_by"),
                locked_at: row.get("locked_at"),
                expires_at: row.get("expires_at"),
                reason: row.get("reason"),
            })
            .collect())
    }

    /// Check if any operations touch locked features.
    pub async fn check_locks(
        &self,
        branch_id: Uuid,
        actor: &str,
        operations: &[DiffOp],
    ) -> Result<Vec<Uuid>, StoreError> {
        let mut blocked = Vec::new();
        for op in operations {
            let fid = match op {
                DiffOp::Update { feature_id, .. } | DiffOp::Delete { feature_id } => *feature_id,
                DiffOp::Insert { .. } => continue,
            };
            let row = sqlx::query(
                "SELECT locked_by FROM feature_locks
                 WHERE feature_id = $1 AND branch_id = $2 AND expires_at > now()",
            )
            .bind(fid)
            .bind(branch_id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(r) = row {
                let owner: String = r.get("locked_by");
                if owner != actor {
                    blocked.push(fid);
                }
            }
        }
        Ok(blocked)
    }
}

// ─── Audit types ────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    pub id: Uuid,
    pub actor: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<Uuid>,
    pub details: serde_json::Value,
    pub ip_address: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ─── Lock types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct FeatureLock {
    pub feature_id: Uuid,
    pub branch_id: Uuid,
    pub locked_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub locked_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub reason: Option<String>,
}

// ─── Merge types ────────────────────────────────────────────────────]
pub enum MergeResult {
    Success(Changeset),
    Conflicts(Vec<ConflictInfo>),
}

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub feature_id: Uuid,
    pub ours: DiffOp,
    pub theirs: DiffOp,
}

// ─── Helpers ────────────────────────────────────────────────────────

fn op_feature_id(op: &DiffOp) -> Uuid {
    match op {
        DiffOp::Insert { feature_id, .. }
        | DiffOp::Update { feature_id, .. }
        | DiffOp::Delete { feature_id } => *feature_id,
    }
}

fn ops_equal(a: &DiffOp, b: &DiffOp) -> bool {
    match (a, b) {
        (
            DiffOp::Insert {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
            },
            DiffOp::Insert {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
            },
        ) => fa == fb && ga == gb && pa == pb,
        (
            DiffOp::Update {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
            },
            DiffOp::Update {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
            },
        ) => fa == fb && ga == gb && pa == pb,
        (DiffOp::Delete { feature_id: fa }, DiffOp::Delete { feature_id: fb }) => fa == fb,
        _ => false,
    }
}

fn parse_geometry_type(s: String) -> GeometryType {
    match s.as_str() {
        "point" => GeometryType::Point,
        "linestring" => GeometryType::LineString,
        "polygon" => GeometryType::Polygon,
        "multipoint" => GeometryType::MultiPoint,
        "multilinestring" => GeometryType::MultiLineString,
        "multipolygon" => GeometryType::MultiPolygon,
        "geometrycollection" => GeometryType::GeometryCollection,
        _ => GeometryType::Point,
    }
}

fn parse_mr_status(s: String) -> MergeRequestStatus {
    match s.as_str() {
        "open" => MergeRequestStatus::Open,
        "approved" => MergeRequestStatus::Approved,
        "merged" => MergeRequestStatus::Merged,
        "closed" => MergeRequestStatus::Closed,
        _ => MergeRequestStatus::Open,
    }
}
