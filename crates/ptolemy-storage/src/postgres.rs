// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! PostgreSQL/PostGIS backend for the versioned feature store.

use ptolemy_core::branch::Branch;
use ptolemy_core::changeset::Changeset;
use ptolemy_core::dataset::{Dataset, GeometryType};
use ptolemy_core::diff::{Diff, DiffOp};
use ptolemy_core::Feature;
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
        let sql = include_str!("../migrations/001_initial.sql");
        sqlx::raw_sql(sql).execute(&self.pool).await?;
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
    pub async fn commit(
        &self,
        branch_id: Uuid,
        message: &str,
        author: &str,
        operations: &[DiffOp],
    ) -> Result<Changeset, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Get current branch head
        let branch_row = sqlx::query("SELECT head, dataset_id FROM branches WHERE id = $1 FOR UPDATE")
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
    pub async fn list_features_at_head(
        &self,
        branch_id: Uuid,
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
        let all_features: std::collections::HashSet<Uuid> = ours_map
            .keys()
            .chain(theirs_map.keys())
            .copied()
            .collect();

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
}

// ─── Merge types ────────────────────────────────────────────────────

#[derive(Debug)]
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
