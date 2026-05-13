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
use serde::Serialize;
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
        let sql_017 = include_str!("../migrations/017_temporal_attachments_replication.sql");
        sqlx::raw_sql(sql_017).execute(&self.pool).await?;
        let sql_018 = include_str!("../migrations/018_rbac_compaction.sql");
        sqlx::raw_sql(sql_018).execute(&self.pool).await?;
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

    /// Topology-aware merge: performs three-way merge with topology validation.
    ///
    /// After computing the merge, validates topology rules for the dataset.
    /// If topology violations are found, attempts auto-repair (e.g., snapping
    /// shared boundaries). Returns violations that cannot be auto-fixed.
    pub async fn merge_with_topology(
        &self,
        source_branch_id: Uuid,
        target_branch_id: Uuid,
        author: &str,
        auto_repair: bool,
    ) -> Result<TopologyMergeResult, StoreError> {
        // First, do the normal merge
        let result = self
            .merge(source_branch_id, target_branch_id, author)
            .await?;

        match result {
            MergeResult::Conflicts(conflicts) => Ok(TopologyMergeResult::MergeConflicts(conflicts)),
            MergeResult::Success(changeset) => {
                // Now validate topology rules for the dataset
                let target = self.get_branch(target_branch_id).await?;
                let violations = self
                    .validate_topology_rules(target.dataset_id, target_branch_id)
                    .await?;

                if violations.is_empty() {
                    return Ok(TopologyMergeResult::Success {
                        changeset,
                        topology_violations: vec![],
                        auto_repaired: vec![],
                    });
                }

                if auto_repair {
                    // Attempt to auto-repair topology violations
                    let (repaired, remaining) = self
                        .auto_repair_topology(target_branch_id, &violations, author)
                        .await?;

                    if remaining.is_empty() {
                        Ok(TopologyMergeResult::Success {
                            changeset,
                            topology_violations: vec![],
                            auto_repaired: repaired,
                        })
                    } else {
                        Ok(TopologyMergeResult::TopologyViolations {
                            changeset,
                            violations: remaining,
                            auto_repaired: repaired,
                        })
                    }
                } else {
                    Ok(TopologyMergeResult::TopologyViolations {
                        changeset,
                        violations,
                        auto_repaired: vec![],
                    })
                }
            }
        }
    }

    /// Validate topology rules defined for a dataset against current features.
    pub async fn validate_topology_rules(
        &self,
        _dataset_id: Uuid,
        branch_id: Uuid,
    ) -> Result<Vec<TopologyViolation>, StoreError> {
        let mut violations = Vec::new();

        // Check for overlapping polygons (MustNotOverlap rule)
        let overlaps = sqlx::query(
            "SELECT a.feature_id as fid_a, b.feature_id as fid_b,
                    ST_AsGeoJSON(ST_Intersection(a.geometry, b.geometry))::jsonb as overlap_geom,
                    ST_Area(ST_Intersection(a.geometry, b.geometry)) as overlap_area
             FROM feature_versions a
             JOIN feature_versions b ON a.feature_id < b.feature_id
              AND a.branch_id = b.branch_id
              AND ST_Overlaps(a.geometry, b.geometry)
             WHERE a.branch_id = $1 AND a.is_deleted = false AND b.is_deleted = false
             LIMIT 100",
        )
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        for row in &overlaps {
            violations.push(TopologyViolation {
                rule: "must_not_overlap".into(),
                feature_a: row.get("fid_a"),
                feature_b: Some(row.get("fid_b")),
                description: format!(
                    "Features overlap with area {}",
                    row.get::<f64, _>("overlap_area")
                ),
                overlap_geometry: row.get("overlap_geom"),
                auto_repairable: true,
            });
        }

        // Check for gaps between adjacent polygons (NoGaps)
        let gaps = sqlx::query(
            "WITH all_geom AS (
                SELECT ST_Union(geometry) as merged
                FROM feature_versions
                WHERE branch_id = $1 AND is_deleted = false
                  AND GeometryType(geometry) IN ('POLYGON', 'MULTIPOLYGON')
            )
            SELECT ST_AsGeoJSON(
                ST_Difference(
                    ST_ConvexHull(merged),
                    merged
                )
            )::jsonb as gap_geom,
            ST_Area(ST_Difference(ST_ConvexHull(merged), merged)) as gap_area
            FROM all_geom
            WHERE ST_Area(ST_Difference(ST_ConvexHull(merged), merged)) > 0.000001",
        )
        .bind(branch_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(gap_row) = gaps {
            let gap_area: f64 = gap_row.get("gap_area");
            if gap_area > 0.0 {
                violations.push(TopologyViolation {
                    rule: "no_gaps".into(),
                    feature_a: Uuid::nil(),
                    feature_b: None,
                    description: format!("Gap detected with area {gap_area}"),
                    overlap_geometry: gap_row.get("gap_geom"),
                    auto_repairable: false,
                });
            }
        }

        // Check for shared boundary consistency
        let boundary_issues = sqlx::query(
            "SELECT a.feature_id as fid_a, b.feature_id as fid_b,
                    ST_AsGeoJSON(ST_Intersection(ST_Boundary(a.geometry), ST_Boundary(b.geometry)))::jsonb as shared_boundary,
                    ST_Length(ST_Intersection(ST_Boundary(a.geometry), ST_Boundary(b.geometry))) as shared_length
             FROM feature_versions a
             JOIN feature_versions b ON a.feature_id < b.feature_id
              AND a.branch_id = b.branch_id
              AND ST_Touches(a.geometry, b.geometry)
             WHERE a.branch_id = $1 AND a.is_deleted = false AND b.is_deleted = false
               AND NOT ST_Equals(
                   ST_Snap(ST_Intersection(ST_Boundary(a.geometry), ST_Boundary(b.geometry)), a.geometry, 0.00001),
                   ST_Intersection(ST_Boundary(a.geometry), ST_Boundary(b.geometry))
               )
             LIMIT 50",
        )
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        for row in &boundary_issues {
            violations.push(TopologyViolation {
                rule: "shared_boundary_consistency".into(),
                feature_a: row.get("fid_a"),
                feature_b: Some(row.get("fid_b")),
                description: "Shared boundary has vertex mismatch (needs snapping)".into(),
                overlap_geometry: row.get("shared_boundary"),
                auto_repairable: true,
            });
        }

        Ok(violations)
    }

    /// Auto-repair topology violations where possible.
    ///
    /// - Overlapping polygons: compute difference to remove overlap from the newer feature
    /// - Boundary mismatches: snap vertices to shared boundary
    async fn auto_repair_topology(
        &self,
        branch_id: Uuid,
        violations: &[TopologyViolation],
        author: &str,
    ) -> Result<(Vec<TopologyRepair>, Vec<TopologyViolation>), StoreError> {
        let mut repaired: Vec<TopologyRepair> = Vec::new();
        let mut remaining: Vec<TopologyViolation> = Vec::new();
        let mut repair_ops: Vec<DiffOp> = Vec::new();

        for violation in violations {
            if !violation.auto_repairable {
                remaining.push(violation.clone());
                continue;
            }

            match violation.rule.as_str() {
                "must_not_overlap" => {
                    // Fix overlap by computing ST_Difference on the second feature
                    if let Some(fid_b) = violation.feature_b {
                        let row = sqlx::query(
                            "SELECT ST_AsBinary(
                                ST_Difference(b.geometry, a.geometry)
                            ) as fixed_geom
                            FROM feature_versions a
                            JOIN feature_versions b ON b.feature_id = $2
                            WHERE a.feature_id = $1 AND a.branch_id = $3
                              AND b.branch_id = $3
                              AND a.is_deleted = false AND b.is_deleted = false
                            LIMIT 1",
                        )
                        .bind(violation.feature_a)
                        .bind(fid_b)
                        .bind(branch_id)
                        .fetch_optional(&self.pool)
                        .await?;

                        if let Some(r) = row {
                            let fixed_wkb: Vec<u8> = r.get("fixed_geom");
                            repair_ops.push(DiffOp::Update {
                                feature_id: fid_b,
                                geometry_wkb: Some(fixed_wkb),
                                properties: None,
                            });
                            repaired.push(TopologyRepair {
                                feature_id: fid_b,
                                rule: violation.rule.clone(),
                                action: "removed overlap via ST_Difference".into(),
                            });
                        } else {
                            remaining.push(violation.clone());
                        }
                    }
                }
                "shared_boundary_consistency" => {
                    // Fix by snapping the second feature to the first
                    if let Some(fid_b) = violation.feature_b {
                        let row = sqlx::query(
                            "SELECT ST_AsBinary(
                                ST_Snap(b.geometry, a.geometry, 0.00001)
                            ) as snapped_geom
                            FROM feature_versions a
                            JOIN feature_versions b ON b.feature_id = $2
                            WHERE a.feature_id = $1 AND a.branch_id = $3
                              AND b.branch_id = $3
                              AND a.is_deleted = false AND b.is_deleted = false
                            LIMIT 1",
                        )
                        .bind(violation.feature_a)
                        .bind(fid_b)
                        .bind(branch_id)
                        .fetch_optional(&self.pool)
                        .await?;

                        if let Some(r) = row {
                            let snapped_wkb: Vec<u8> = r.get("snapped_geom");
                            repair_ops.push(DiffOp::Update {
                                feature_id: fid_b,
                                geometry_wkb: Some(snapped_wkb),
                                properties: None,
                            });
                            repaired.push(TopologyRepair {
                                feature_id: fid_b,
                                rule: violation.rule.clone(),
                                action: "snapped to shared boundary".into(),
                            });
                        } else {
                            remaining.push(violation.clone());
                        }
                    }
                }
                _ => {
                    remaining.push(violation.clone());
                }
            }
        }

        // Commit repairs as a separate changeset
        if !repair_ops.is_empty() {
            self.commit(
                branch_id,
                "auto-repair topology violations",
                author,
                &repair_ops,
            )
            .await?;
        }

        Ok((repaired, remaining))
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

    // ─── Feature Attachments ────────────────────────────────────────────

    pub async fn create_attachment(&self, attachment: &Attachment) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO attachments (id, feature_id, branch_id, name, content_type, size_bytes, data, thumbnail, metadata, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(attachment.id)
        .bind(attachment.feature_id)
        .bind(attachment.branch_id)
        .bind(&attachment.name)
        .bind(&attachment.content_type)
        .bind(attachment.size_bytes)
        .bind(&attachment.data)
        .bind(&attachment.thumbnail)
        .bind(&attachment.metadata)
        .bind(&attachment.created_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_attachments(
        &self,
        feature_id: Uuid,
        branch_id: Uuid,
    ) -> Result<Vec<AttachmentMeta>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, feature_id, branch_id, name, content_type, size_bytes, metadata, created_by, created_at
             FROM attachments
             WHERE feature_id = $1 AND branch_id = $2
             ORDER BY created_at DESC",
        )
        .bind(feature_id)
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| AttachmentMeta {
                id: row.get("id"),
                feature_id: row.get("feature_id"),
                branch_id: row.get("branch_id"),
                name: row.get("name"),
                content_type: row.get("content_type"),
                size_bytes: row.get("size_bytes"),
                metadata: row.get("metadata"),
                created_by: row.get("created_by"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    pub async fn get_attachment(&self, id: Uuid) -> Result<Attachment, StoreError> {
        let row = sqlx::query(
            "SELECT id, feature_id, branch_id, name, content_type, size_bytes, data, thumbnail, metadata, created_by, created_at
             FROM attachments WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("attachment {id}")))?;

        Ok(Attachment {
            id: row.get("id"),
            feature_id: row.get("feature_id"),
            branch_id: row.get("branch_id"),
            name: row.get("name"),
            content_type: row.get("content_type"),
            size_bytes: row.get("size_bytes"),
            data: row.get("data"),
            thumbnail: row.get("thumbnail"),
            metadata: row.get("metadata"),
            created_by: row.get("created_by"),
            created_at: row.get("created_at"),
        })
    }

    pub async fn delete_attachment(&self, id: Uuid) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM attachments WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ─── Schema Evolution ───────────────────────────────────────────────

    pub async fn apply_schema_migration(
        &self,
        migration: &SchemaMigration,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO schema_migrations (id, dataset_id, version, description, migration_type, field_name, old_definition, new_definition, applied_by, rollback_sql)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(migration.id)
        .bind(migration.dataset_id)
        .bind(migration.version)
        .bind(&migration.description)
        .bind(&migration.migration_type)
        .bind(&migration.field_name)
        .bind(&migration.old_definition)
        .bind(&migration.new_definition)
        .bind(&migration.applied_by)
        .bind(&migration.rollback_sql)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_schema_migrations(
        &self,
        dataset_id: Uuid,
    ) -> Result<Vec<SchemaMigration>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, version, description, migration_type, field_name, old_definition, new_definition, applied_by, applied_at, rollback_sql
             FROM schema_migrations
             WHERE dataset_id = $1
             ORDER BY version ASC",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| SchemaMigration {
                id: row.get("id"),
                dataset_id: row.get("dataset_id"),
                version: row.get("version"),
                description: row.get("description"),
                migration_type: row.get("migration_type"),
                field_name: row.get("field_name"),
                old_definition: row.get("old_definition"),
                new_definition: row.get("new_definition"),
                applied_by: row.get("applied_by"),
                applied_at: row.get("applied_at"),
                rollback_sql: row.get("rollback_sql"),
            })
            .collect())
    }

    pub async fn get_schema_version(&self, dataset_id: Uuid) -> Result<i32, StoreError> {
        let row = sqlx::query(
            "SELECT COALESCE(MAX(version), 0) as version FROM schema_migrations WHERE dataset_id = $1",
        )
        .bind(dataset_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("version"))
    }

    // ─── Replication / Change Feed ──────────────────────────────────────

    pub async fn append_change_feed(
        &self,
        changeset_id: Uuid,
        branch_id: Uuid,
        operation_type: &str,
        payload: &serde_json::Value,
    ) -> Result<i64, StoreError> {
        let row = sqlx::query(
            "INSERT INTO change_feed (changeset_id, branch_id, operation_type, payload)
             VALUES ($1, $2, $3, $4)
             RETURNING sequence_id",
        )
        .bind(changeset_id)
        .bind(branch_id)
        .bind(operation_type)
        .bind(payload)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("sequence_id"))
    }

    pub async fn get_change_feed(
        &self,
        branch_id: Uuid,
        since_sequence: i64,
        limit: i64,
    ) -> Result<Vec<ChangeFeedEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT sequence_id, changeset_id, branch_id, operation_type, payload, created_at
             FROM change_feed
             WHERE branch_id = $1 AND sequence_id > $2
             ORDER BY sequence_id ASC
             LIMIT $3",
        )
        .bind(branch_id)
        .bind(since_sequence)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ChangeFeedEntry {
                sequence_id: row.get("sequence_id"),
                changeset_id: row.get("changeset_id"),
                branch_id: row.get("branch_id"),
                operation_type: row.get("operation_type"),
                payload: row.get("payload"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    pub async fn register_peer(&self, peer: &ReplicationPeer) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO replication_peers (id, name, endpoint_url, direction, status)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (name) DO UPDATE SET endpoint_url = $3, direction = $4, status = $5",
        )
        .bind(peer.id)
        .bind(&peer.name)
        .bind(&peer.endpoint_url)
        .bind(&peer.direction)
        .bind(&peer.status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_peers(&self) -> Result<Vec<ReplicationPeer>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, endpoint_url, last_sync_changeset, last_sync_at, direction, status, created_at
             FROM replication_peers ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ReplicationPeer {
                id: row.get("id"),
                name: row.get("name"),
                endpoint_url: row.get("endpoint_url"),
                last_sync_changeset: row.get("last_sync_changeset"),
                last_sync_at: row.get("last_sync_at"),
                direction: row.get("direction"),
                status: row.get("status"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    pub async fn update_peer_sync(
        &self,
        peer_id: Uuid,
        changeset_id: Uuid,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE replication_peers SET last_sync_changeset = $2, last_sync_at = now() WHERE id = $1",
        )
        .bind(peer_id)
        .bind(changeset_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ─── RBAC: Per-dataset and Per-branch Permissions ───────────────────

    pub async fn grant_dataset_permission(
        &self,
        dataset_id: Uuid,
        user_id: &str,
        permission: &str,
        granted_by: &str,
    ) -> Result<DatasetPermission, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO dataset_permissions (id, dataset_id, user_id, permission, granted_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (dataset_id, user_id)
             DO UPDATE SET permission = $4, granted_by = $5, granted_at = now()",
        )
        .bind(id)
        .bind(dataset_id)
        .bind(user_id)
        .bind(permission)
        .bind(granted_by)
        .execute(&self.pool)
        .await?;

        Ok(DatasetPermission {
            id,
            dataset_id,
            user_id: user_id.to_string(),
            permission: permission.to_string(),
            granted_by: granted_by.to_string(),
            granted_at: OffsetDateTime::now_utc(),
        })
    }

    pub async fn revoke_dataset_permission(
        &self,
        dataset_id: Uuid,
        user_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM dataset_permissions WHERE dataset_id = $1 AND user_id = $2")
            .bind(dataset_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_dataset_permissions(
        &self,
        dataset_id: Uuid,
    ) -> Result<Vec<DatasetPermission>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, user_id, permission, granted_by, granted_at
             FROM dataset_permissions WHERE dataset_id = $1 ORDER BY granted_at",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DatasetPermission {
                id: r.get("id"),
                dataset_id: r.get("dataset_id"),
                user_id: r.get("user_id"),
                permission: r.get("permission"),
                granted_by: r.get("granted_by"),
                granted_at: r.get("granted_at"),
            })
            .collect())
    }

    /// Check if a user has at least the specified permission level on a dataset.
    /// Permission hierarchy: admin > write > read.
    pub async fn check_dataset_permission(
        &self,
        dataset_id: Uuid,
        user_id: &str,
        required: &str,
    ) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "SELECT permission FROM dataset_permissions
             WHERE dataset_id = $1 AND user_id = $2",
        )
        .bind(dataset_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            let perm: String = r.get("permission");
            Ok(permission_level(&perm) >= permission_level(required))
        } else {
            // Check org membership fallback
            let org_row = sqlx::query(
                "SELECT om.role FROM org_members om
                 JOIN datasets d ON d.org_id = om.org_id
                 WHERE d.id = $1 AND om.user_id = $2",
            )
            .bind(dataset_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;

            if let Some(r) = org_row {
                let role: String = r.get("role");
                Ok(org_role_to_permission(&role) >= permission_level(required))
            } else {
                Ok(false)
            }
        }
    }

    pub async fn grant_branch_permission(
        &self,
        branch_id: Uuid,
        user_id: &str,
        permission: &str,
        granted_by: &str,
    ) -> Result<BranchPermission, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO branch_permissions (id, branch_id, user_id, permission, granted_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (branch_id, user_id)
             DO UPDATE SET permission = $4, granted_by = $5, granted_at = now()",
        )
        .bind(id)
        .bind(branch_id)
        .bind(user_id)
        .bind(permission)
        .bind(granted_by)
        .execute(&self.pool)
        .await?;

        Ok(BranchPermission {
            id,
            branch_id,
            user_id: user_id.to_string(),
            permission: permission.to_string(),
            granted_by: granted_by.to_string(),
            granted_at: OffsetDateTime::now_utc(),
        })
    }

    pub async fn revoke_branch_permission(
        &self,
        branch_id: Uuid,
        user_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM branch_permissions WHERE branch_id = $1 AND user_id = $2")
            .bind(branch_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn check_branch_permission(
        &self,
        branch_id: Uuid,
        user_id: &str,
        required: &str,
    ) -> Result<bool, StoreError> {
        // Check direct branch permission first
        let row = sqlx::query(
            "SELECT permission FROM branch_permissions
             WHERE branch_id = $1 AND user_id = $2",
        )
        .bind(branch_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            let perm: String = r.get("permission");
            return Ok(permission_level(&perm) >= permission_level(required));
        }

        // Fallback: check dataset-level permission
        let ds_row = sqlx::query(
            "SELECT dp.permission FROM dataset_permissions dp
             JOIN branches b ON b.dataset_id = dp.dataset_id
             WHERE b.id = $1 AND dp.user_id = $2",
        )
        .bind(branch_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = ds_row {
            let perm: String = r.get("permission");
            return Ok(permission_level(&perm) >= permission_level(required));
        }

        // Fallback: check org membership
        let org_row = sqlx::query(
            "SELECT om.role FROM org_members om
             JOIN datasets d ON d.org_id = om.org_id
             JOIN branches b ON b.dataset_id = d.id
             WHERE b.id = $1 AND om.user_id = $2",
        )
        .bind(branch_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = org_row {
            let role: String = r.get("role");
            Ok(org_role_to_permission(&role) >= permission_level(required))
        } else {
            Ok(false)
        }
    }

    // ─── Version Compaction / Garbage Collection ────────────────────────

    /// Compact old feature versions on a branch, keeping only the N most
    /// recent versions per feature. Returns the number of versions removed.
    pub async fn compact_versions(
        &self,
        branch_id: Uuid,
        keep_latest: i32,
    ) -> Result<CompactionResult, StoreError> {
        let run_id = Uuid::now_v7();
        let branch = self.get_branch(branch_id).await?;

        // Count versions before
        let before: i64 = sqlx::query(
            "SELECT COUNT(*) as cnt FROM feature_versions fv
             JOIN changesets c ON fv.changeset_id = c.id
             WHERE c.branch_id = $1",
        )
        .bind(branch_id)
        .fetch_one(&self.pool)
        .await?
        .get("cnt");

        // Record compaction run
        sqlx::query(
            "INSERT INTO compaction_runs (id, dataset_id, branch_id, versions_before, keep_latest, status)
             VALUES ($1, $2, $3, $4, $5, 'running')",
        )
        .bind(run_id)
        .bind(branch.dataset_id)
        .bind(branch_id)
        .bind(before)
        .bind(keep_latest)
        .execute(&self.pool)
        .await?;

        // Delete old versions, keeping the N most recent per feature
        let deleted = sqlx::query(
            "WITH ranked AS (
                SELECT fv.id,
                    ROW_NUMBER() OVER (PARTITION BY fv.feature_id ORDER BY fv.created_at DESC) as rn
                FROM feature_versions fv
                JOIN changesets c ON fv.changeset_id = c.id
                WHERE c.branch_id = $1
            )
            DELETE FROM feature_versions
            WHERE id IN (SELECT id FROM ranked WHERE rn > $2)",
        )
        .bind(branch_id)
        .bind(keep_latest)
        .execute(&self.pool)
        .await?;

        let removed = deleted.rows_affected() as i64;
        let after = before - removed;

        // Estimate bytes freed (rough: ~200 bytes per version row)
        let bytes_freed = removed * 200;

        // Update compaction run record
        sqlx::query(
            "UPDATE compaction_runs
             SET versions_after = $2, versions_removed = $3, bytes_freed = $4,
                 completed_at = now(), status = 'completed'
             WHERE id = $1",
        )
        .bind(run_id)
        .bind(after)
        .bind(removed)
        .bind(bytes_freed)
        .execute(&self.pool)
        .await?;

        Ok(CompactionResult {
            run_id,
            branch_id,
            versions_before: before,
            versions_after: after,
            versions_removed: removed,
            bytes_freed,
        })
    }

    /// List past compaction runs for a dataset.
    pub async fn list_compaction_runs(
        &self,
        dataset_id: Uuid,
    ) -> Result<Vec<CompactionRun>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, dataset_id, branch_id, versions_before, versions_after,
                    versions_removed, bytes_freed, keep_latest, started_at, completed_at, status
             FROM compaction_runs
             WHERE dataset_id = $1
             ORDER BY started_at DESC
             LIMIT 50",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| CompactionRun {
                id: r.get("id"),
                dataset_id: r.get("dataset_id"),
                branch_id: r.get("branch_id"),
                versions_before: r.get("versions_before"),
                versions_after: r.get("versions_after"),
                versions_removed: r.get("versions_removed"),
                bytes_freed: r.get("bytes_freed"),
                keep_latest: r.get("keep_latest"),
                started_at: r.get("started_at"),
                completed_at: r.get("completed_at"),
                status: r.get("status"),
            })
            .collect())
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

/// Result of a topology-aware merge.
pub enum TopologyMergeResult {
    /// Merge succeeded with no topology violations.
    Success {
        changeset: Changeset,
        topology_violations: Vec<TopologyViolation>,
        auto_repaired: Vec<TopologyRepair>,
    },
    /// Merge produced feature-level conflicts (same as normal merge).
    MergeConflicts(Vec<ConflictInfo>),
    /// Merge succeeded but topology rules are violated.
    TopologyViolations {
        changeset: Changeset,
        violations: Vec<TopologyViolation>,
        auto_repaired: Vec<TopologyRepair>,
    },
}

/// A topology rule violation detected after merge.
#[derive(Debug, Clone, Serialize)]
pub struct TopologyViolation {
    pub rule: String,
    pub feature_a: Uuid,
    pub feature_b: Option<Uuid>,
    pub description: String,
    pub overlap_geometry: Option<serde_json::Value>,
    pub auto_repairable: bool,
}

/// Record of an auto-repair applied to fix a topology violation.
#[derive(Debug, Clone, Serialize)]
pub struct TopologyRepair {
    pub feature_id: Uuid,
    pub rule: String,
    pub action: String,
}

// ─── Attachment types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub id: Uuid,
    pub feature_id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    #[serde(skip_serializing)]
    pub data: Vec<u8>,
    #[serde(skip_serializing)]
    pub thumbnail: Option<Vec<u8>>,
    pub metadata: serde_json::Value,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Attachment metadata without binary data (for listing).
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentMeta {
    pub id: Uuid,
    pub feature_id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub metadata: serde_json::Value,
    pub created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ─── Schema Evolution types ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SchemaMigration {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub version: i32,
    pub description: String,
    pub migration_type: String,
    pub field_name: Option<String>,
    pub old_definition: Option<serde_json::Value>,
    pub new_definition: Option<serde_json::Value>,
    pub applied_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub applied_at: OffsetDateTime,
    pub rollback_sql: Option<String>,
}

// ─── Replication types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ChangeFeedEntry {
    pub sequence_id: i64,
    pub changeset_id: Uuid,
    pub branch_id: Uuid,
    pub operation_type: String,
    pub payload: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ReplicationPeer {
    pub id: Uuid,
    pub name: String,
    pub endpoint_url: Option<String>,
    pub last_sync_changeset: Option<Uuid>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_sync_at: Option<OffsetDateTime>,
    pub direction: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ─── RBAC types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct DatasetPermission {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub user_id: String,
    pub permission: String,
    pub granted_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub granted_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchPermission {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub user_id: String,
    pub permission: String,
    pub granted_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub granted_at: OffsetDateTime,
}

// ─── Compaction types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CompactionResult {
    pub run_id: Uuid,
    pub branch_id: Uuid,
    pub versions_before: i64,
    pub versions_after: i64,
    pub versions_removed: i64,
    pub bytes_freed: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionRun {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub versions_before: i64,
    pub versions_after: i64,
    pub versions_removed: i64,
    pub bytes_freed: i64,
    pub keep_latest: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    pub status: String,
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Permission level: higher = more access.
fn permission_level(perm: &str) -> u8 {
    match perm {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

/// Map org role to permission level.
fn org_role_to_permission(role: &str) -> u8 {
    match role {
        "admin" | "owner" => 3,
        "editor" | "member" => 2,
        "viewer" => 1,
        _ => 0,
    }
}

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
