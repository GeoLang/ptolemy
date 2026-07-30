// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! PostgreSQL/PostGIS backend for the versioned feature store.

use crate::analyze::Analyzer;
use crate::permission::{
    Check, Reader, Scope, Writer, permission_level, visible_datasets_sql, write_allowed,
};
use ptolemy_core::Feature;
use ptolemy_core::branch::Branch;
use ptolemy_core::changeset::Changeset;
use ptolemy_core::dataset::{Dataset, GeometryType, Visibility};
use ptolemy_core::diff::{Diff, DiffOp};
use ptolemy_core::event::{Event, Webhook};
use ptolemy_core::external::{ExternalSource, ExternalTable};
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
    #[error("forbidden: {0}")]
    Forbidden(String),
}

/// Columns every read query needs from `latest`. Public so a handler that builds
/// its own query can ask for the same projection.
pub const LATEST_COLUMNS: &str = "fv.feature_id, fv.dataset_id, fv.operation, fv.geometry, fv.properties, fv.valid_from, fv.valid_to";

/// Latest live version of each feature on the branch bound to `$1`, resolved by
/// walking the branch head's ancestor chain. Shared by the read queries so the
/// external variant only has to swap the FROM clause. `columns` is the
/// projection: the DISTINCT ON sorts every version in the chain, so a query
/// that only counts should not drag geometry through it.
fn latest_cte(columns: &str) -> String {
    format!(
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
            {columns}
        FROM feature_versions fv
        JOIN chain ch ON fv.changeset_id = ch.id
        ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
    )"
    )
}

/// The `features` view's rows for one branch, as a derived table to put after
/// FROM (the caller supplies the alias).
///
/// The view itself cannot do this: it walks the changeset chain of *every* branch
/// in the database and only then does its consumer's `WHERE branch_id = …` throw
/// the other branches away, so a read's cost is set by total instance history
/// rather than by the dataset being queried. A view takes no parameters, so the
/// scoped form has to be built per query — the same ancestor-chain walk the bbox
/// reads already use through `latest_cte`.
///
/// Columns match the view: `id, branch_id, dataset_id, geometry, properties,
/// created_at`, deletes excluded. `branch_expr` is SQL for the branch id, so a
/// query that binds the branch somewhere other than `$1` passes its own
/// placeholder; it is a caller-side constant, never request data.
pub fn branch_features_subquery(branch_expr: &str) -> String {
    format!(
        "(WITH RECURSIVE chain AS (
              SELECT c.id, c.parent_id FROM changesets c
                JOIN branches b ON b.head = c.id
               WHERE b.id = {branch_expr}
            UNION ALL
              SELECT c.id, c.parent_id FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
          ),
          live AS (
              SELECT DISTINCT ON (fv.feature_id)
                     fv.feature_id AS id, fv.dataset_id, fv.operation,
                     fv.geometry, fv.properties, fv.created_at,
                     fv.valid_from, fv.valid_to
                FROM feature_versions fv JOIN chain ch ON fv.changeset_id = ch.id
               ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
          )
          SELECT id, {branch_expr}::uuid AS branch_id, dataset_id, geometry,
                 properties, created_at, valid_from, valid_to
            FROM live WHERE operation <> 'delete')"
    )
}

/// Keeps rows whose valid time covers the instant at `placeholder`, which the
/// caller always binds: NULL there means no filter, so one query shape serves
/// both. Half-open, [valid_from, valid_to), and a NULL end is unbounded.
fn valid_at_predicate(placeholder: &str) -> String {
    format!(
        "({placeholder}::timestamptz IS NULL
                  OR ((valid_from IS NULL OR valid_from <= {placeholder})
                      AND (valid_to IS NULL OR valid_to > {placeholder})))"
    )
}

/// Rejection message for every mutation aimed at an external dataset.
pub const EXTERNAL_READ_ONLY: &str =
    "dataset is external (read-only): it is a view over a PostGIS relation ptolemy does not own";

/// Optional second database for external reads. Point it at a read-only role so
/// the guarantee holds at the database, not only in this process.
pub const EXTERNAL_DATABASE_URL: &str = "PTOLEMY_EXTERNAL_DATABASE_URL";

pub struct PgStore {
    pool: PgPool,
    /// Built on first external read, so an unset env var costs nothing and a
    /// bad URL fails the request rather than startup.
    external_pool: tokio::sync::OnceCell<PgPool>,
    analyzer: Analyzer,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self::with_analyze_threshold(pool, Analyzer::threshold_from_env())
    }

    /// Same, with the bulk-write ANALYZE threshold given directly instead of
    /// read from the environment.
    pub fn with_analyze_threshold(pool: PgPool, rows: usize) -> Self {
        Self {
            analyzer: Analyzer::new(pool.clone(), rows),
            pool,
            external_pool: tokio::sync::OnceCell::new(),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Refreshes planner statistics after a bulk write. Handlers that insert
    /// rows outside [`PgStore::commit`] must report their row count to it.
    pub fn analyzer(&self) -> &Analyzer {
        &self.analyzer
    }

    /// Run migrations embedded in this crate.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        // sqlx tracks applied migrations in _sqlx_migrations, so this is
        // idempotent and picks up new migration files automatically
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| StoreError::Db(sqlx::Error::Migrate(Box::new(e))))?;
        Ok(())
    }

    // ─── Dataset CRUD ───────────────────────────────────────────────

    /// Create a dataset. `grant_admin_to` is the verified creator identity when
    /// auth is on: it gets an admin permission row in the same transaction, so a
    /// dataset is never left with content but no owner. `None` (auth off) leaves
    /// the dataset with no rows, which keeps it open to any editor.
    pub async fn create_dataset(
        &self,
        ds: &Dataset,
        grant_admin_to: Option<&str>,
    ) -> Result<(), StoreError> {
        let geom_type = format!("{:?}", ds.geometry_type).to_lowercase();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO datasets (id, name, srid, geometry_type, created_at, created_by,
                                   external_table, external_id_column, external_geometry_column,
                                   visibility)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(ds.id)
        .bind(&ds.name)
        .bind(ds.srid)
        .bind(&geom_type)
        .bind(ds.created_at)
        .bind(&ds.created_by)
        .bind(ds.external.as_ref().map(|e| e.table()))
        .bind(ds.external.as_ref().map(|e| e.id_column()))
        .bind(ds.external.as_ref().map(|e| e.geometry_column()))
        .bind(ds.visibility.as_str())
        .execute(&mut *tx)
        .await?;
        if let Some(creator) = grant_admin_to {
            insert_creator_admin_grant(&mut tx, ds.id, creator).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Register an external dataset: probe the relation, then insert the
    /// dataset and its `main` branch together so list/branch endpoints work
    /// like they do for a versioned dataset. `srid` is taken from the relation
    /// itself, not from the request, because the relation is the truth.
    pub async fn register_external_dataset(
        &self,
        ds: &Dataset,
        grant_admin_to: Option<&str>,
    ) -> Result<Dataset, StoreError> {
        let table = ds
            .external
            .as_ref()
            .ok_or_else(|| StoreError::Conflict("dataset has no external table".into()))?;
        let srid = self.probe_external(table).await?;

        let mut ds = ds.clone();
        ds.srid = srid;
        let geom_type = format!("{:?}", ds.geometry_type).to_lowercase();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO datasets (id, name, srid, geometry_type, created_at, created_by,
                                   external_table, external_id_column, external_geometry_column,
                                   visibility)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(ds.id)
        .bind(&ds.name)
        .bind(ds.srid)
        .bind(&geom_type)
        .bind(ds.created_at)
        .bind(&ds.created_by)
        .bind(table.table())
        .bind(table.id_column())
        .bind(table.geometry_column())
        .bind(ds.visibility.as_str())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO branches (id, dataset_id, name, head, created_at, created_by)
             VALUES ($1, $2, 'main', NULL, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(ds.id)
        .bind(ds.created_at)
        .bind(&ds.created_by)
        .execute(&mut *tx)
        .await?;

        if let Some(creator) = grant_admin_to {
            insert_creator_admin_grant(&mut tx, ds.id, creator).await?;
        }

        tx.commit().await?;
        Ok(ds)
    }

    /// Check that the relation and its two columns exist, that the geometry
    /// column really is PostGIS geometry, and that the pool external reads use
    /// can actually select from it. Returns the relation's SRID.
    ///
    /// The catalog lookup binds the relation name, so it never builds SQL from
    /// it; the `LIMIT 1` select must interpolate the identifiers, which is why
    /// they went through `ExternalTable::parse` first.
    pub async fn probe_external(&self, table: &ExternalTable) -> Result<i32, StoreError> {
        let pool = self.external_pool().await?;
        let relation = table.quoted_relation();

        let oid: i64 = sqlx::query_scalar("SELECT $1::regclass::oid::int8")
            .bind(&relation)
            .fetch_one(pool)
            .await
            .map_err(|_| {
                StoreError::Conflict(format!(
                    "relation {} does not exist or is not readable",
                    table.table()
                ))
            })?;

        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod)
             FROM pg_attribute a
             WHERE a.attrelid = $1::oid AND a.attnum > 0 AND NOT a.attisdropped
               AND a.attname = ANY($2)",
        )
        .bind(oid)
        .bind(vec![
            table.id_column().to_string(),
            table.geometry_column().to_string(),
        ])
        .fetch_all(pool)
        .await?;

        let column_type = |name: &str| columns.iter().find(|(n, _)| n == name).map(|(_, t)| t);
        if column_type(table.id_column()).is_none() {
            return Err(StoreError::Conflict(format!(
                "column {} does not exist on {}",
                table.id_column(),
                table.table()
            )));
        }
        let geom_type = column_type(table.geometry_column()).ok_or_else(|| {
            StoreError::Conflict(format!(
                "column {} does not exist on {}",
                table.geometry_column(),
                table.table()
            ))
        })?;
        if geom_type != "geometry" && !geom_type.starts_with("geometry(") {
            return Err(StoreError::Conflict(format!(
                "column {} on {} is {geom_type}, not PostGIS geometry",
                table.geometry_column(),
                table.table()
            )));
        }

        // reading one row proves SELECT is granted and gives the SRID actually
        // stored, which a plain `geometry` column does not declare
        let id = format!("\"{}\"", table.id_column());
        let geom = format!("\"{}\"", table.geometry_column());
        let srid: Option<i32> = sqlx::query_scalar(&format!(
            "SELECT ST_SRID(t.{geom}) FROM {relation} t WHERE t.{geom} IS NOT NULL LIMIT 1"
        ))
        .fetch_optional(pool)
        .await
        .map_err(|e| StoreError::Conflict(format!("cannot read {}: {e}", table.table())))?
        .flatten();

        sqlx::query(&format!("SELECT t.{id} FROM {relation} t LIMIT 1"))
            .fetch_optional(pool)
            .await
            .map_err(|e| StoreError::Conflict(format!("cannot read {}: {e}", table.table())))?;

        Ok(srid.unwrap_or(4326))
    }

    /// The external read source for a branch, or `None` for an ordinary
    /// versioned dataset. Every read path that supports external datasets
    /// starts here.
    pub async fn external_for_branch(
        &self,
        branch_id: Uuid,
    ) -> Result<Option<ExternalSource>, StoreError> {
        let row = sqlx::query(
            "SELECT d.id, d.srid, d.external_table, d.external_id_column,
                    d.external_geometry_column
             FROM branches b JOIN datasets d ON d.id = b.dataset_id
             WHERE b.id = $1",
        )
        .bind(branch_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(external_source_from_row)
            .transpose()
            .map(Option::flatten)
    }

    /// Same, resolved through the dataset's `main` branch.
    pub async fn external_for_dataset(
        &self,
        dataset_id: Uuid,
    ) -> Result<Option<ExternalSource>, StoreError> {
        let row = sqlx::query(
            "SELECT id, srid, external_table, external_id_column, external_geometry_column
             FROM datasets WHERE id = $1",
        )
        .bind(dataset_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(external_source_from_row)
            .transpose()
            .map(Option::flatten)
    }

    /// Reject any mutation aimed at a dataset that does not exist or is external,
    /// and any writer the dataset's permission rows do not allow. One place, so a
    /// write path is guarded by being routed through it rather than by
    /// remembering to check.
    pub async fn ensure_dataset_writable(
        &self,
        dataset_id: Uuid,
        writer: &Writer,
    ) -> Result<(), StoreError> {
        // a missing dataset has no permission rows, so the ladder below would
        // read it as unenforced and pass the write on to fail on the foreign key
        let external: Option<String> =
            sqlx::query_scalar("SELECT external_table FROM datasets WHERE id = $1")
                .bind(dataset_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| StoreError::NotFound(format!("dataset {dataset_id}")))?;
        if external.is_some() {
            return Err(StoreError::Conflict(EXTERNAL_READ_ONLY.into()));
        }

        let user_id = match writer.check() {
            Check::Skip => return Ok(()),
            Check::Deny => return Err(denied_dataset(dataset_id)),
            Check::Ladder(id) => id,
        };
        let dataset = dataset_scope(&self.pool, dataset_id, user_id).await?;
        if write_allowed(&Scope::open(), &dataset) {
            Ok(())
        } else {
            Err(denied_dataset(dataset_id))
        }
    }

    pub async fn ensure_branch_writable(
        &self,
        branch_id: Uuid,
        writer: &Writer,
    ) -> Result<(), StoreError> {
        let row = sqlx::query(
            "SELECT d.id AS dataset_id, d.external_table
             FROM branches b JOIN datasets d ON d.id = b.dataset_id
             WHERE b.id = $1",
        )
        .bind(branch_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("branch {branch_id}")))?;

        if row.get::<Option<String>, _>("external_table").is_some() {
            return Err(StoreError::Conflict(EXTERNAL_READ_ONLY.into()));
        }
        self.ensure_branch_write_allowed(branch_id, row.get("dataset_id"), writer)
            .await
    }

    /// The write ladder on its own, for callers that already proved the target
    /// is not external.
    async fn ensure_branch_write_allowed(
        &self,
        branch_id: Uuid,
        dataset_id: Uuid,
        writer: &Writer,
    ) -> Result<(), StoreError> {
        let user_id = match writer.check() {
            Check::Skip => return Ok(()),
            Check::Deny => return Err(denied_branch(branch_id)),
            Check::Ladder(id) => id,
        };
        let (branch, dataset) = write_scopes(&self.pool, branch_id, dataset_id, user_id).await?;
        if write_allowed(&branch, &dataset) {
            Ok(())
        } else {
            Err(denied_branch(branch_id))
        }
    }

    /// Pool that external reads and probes run on: a second database when
    /// `PTOLEMY_EXTERNAL_DATABASE_URL` is set (meant to hold a read-only role),
    /// otherwise the primary pool for tables in the same database.
    pub async fn external_pool(&self) -> Result<&PgPool, StoreError> {
        let Ok(url) = std::env::var(EXTERNAL_DATABASE_URL) else {
            return Ok(&self.pool);
        };
        if url.is_empty() {
            return Ok(&self.pool);
        }
        self.external_pool
            .get_or_try_init(|| async { PgPool::connect(&url).await })
            .await
            .map_err(StoreError::Db)
    }

    /// The pool a read should use, given whether it targets an external dataset.
    pub async fn read_pool(
        &self,
        external: Option<&ExternalSource>,
    ) -> Result<&PgPool, StoreError> {
        match external {
            Some(_) => self.external_pool().await,
            None => Ok(&self.pool),
        }
    }

    pub async fn get_dataset(&self, id: Uuid) -> Result<Dataset, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, srid, geometry_type, created_at, created_by,
                    external_table, external_id_column, external_geometry_column, visibility
             FROM datasets WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("dataset {id}")))?;

        dataset_from_row(row)
    }

    /// Datasets this reader may see. A private dataset the reader holds no grant
    /// on is simply absent, as it is from every other listing.
    pub async fn list_datasets(&self, reader: &Reader) -> Result<Vec<Dataset>, StoreError> {
        let visible = visible_datasets_sql("d", 1, 2);
        let rows = sqlx::query(&format!(
            "SELECT d.id, d.name, d.srid, d.geometry_type, d.created_at, d.created_by,
                    d.external_table, d.external_id_column, d.external_geometry_column,
                    d.visibility
             FROM datasets d WHERE {visible} ORDER BY d.name"
        ))
        .bind(reader.bypass)
        .bind(reader.id.as_deref())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(dataset_from_row).collect()
    }

    /// Set a dataset's visibility. Only an instance admin or a holder of an
    /// `admin` permission row on the dataset may call this; the caller enforces
    /// that, this is the write itself.
    pub async fn set_dataset_visibility(
        &self,
        id: Uuid,
        visibility: Visibility,
    ) -> Result<Dataset, StoreError> {
        let affected = sqlx::query("UPDATE datasets SET visibility = $2 WHERE id = $1")
            .bind(id)
            .bind(visibility.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(StoreError::NotFound(format!("dataset {id}")));
        }
        self.get_dataset(id).await
    }

    /// Whether the caller holds an `admin` permission row on a dataset. Used to
    /// decide who may change its visibility.
    pub async fn is_dataset_admin(&self, id: Uuid, user_id: &str) -> Result<bool, StoreError> {
        let perm: Option<String> = sqlx::query_scalar(
            "SELECT permission FROM dataset_permissions WHERE dataset_id = $1 AND user_id = $2",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(perm.as_deref().map(permission_level).unwrap_or(0) >= permission_level("admin"))
    }

    // ─── Visibility enforcement (read paths) ────────────────────────

    /// The private datasets any of `ids` refers to. An id may name a dataset, a
    /// branch, a changeset, a merge request, a feature, a raster catalog or tile,
    /// a point cloud catalog or patch, or an attachment. Public datasets are left
    /// out, so an empty result means nothing to enforce.
    ///
    /// This list is NOT yet every id a route can name: network, route, symbology,
    /// label, domain, subtype, attribute-rule, trajectory, relationship-class and
    /// webhook ids all resolve to nothing here, so the layer passes them. An id
    /// kind missing from this query is an unguarded read of private content, so a
    /// new dataset-owned table belongs here at the same time as its route.
    /// TODO: add the remaining kinds, several of which need a grandparent hop or
    /// carry two owning datasets.
    pub async fn private_datasets_for_ids(&self, ids: &[Uuid]) -> Result<Vec<Uuid>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT d.id FROM datasets d WHERE d.visibility = 'private' AND (
                 d.id = ANY($1)
                 OR EXISTS (SELECT 1 FROM branches b
                             WHERE b.dataset_id = d.id AND b.id = ANY($1))
                 OR EXISTS (SELECT 1 FROM changesets c JOIN branches b ON b.id = c.branch_id
                             WHERE b.dataset_id = d.id AND c.id = ANY($1))
                 OR EXISTS (SELECT 1 FROM merge_requests m
                             WHERE m.dataset_id = d.id AND m.id = ANY($1))
                 OR EXISTS (SELECT 1 FROM feature_versions fv
                             WHERE fv.dataset_id = d.id AND fv.feature_id = ANY($1))
                 OR EXISTS (SELECT 1 FROM raster_catalogs rc
                             WHERE rc.dataset_id = d.id AND rc.id = ANY($1))
                 OR EXISTS (SELECT 1 FROM raster_tiles rt
                              JOIN raster_catalogs rc ON rc.id = rt.catalog_id
                             WHERE rc.dataset_id = d.id AND rt.id = ANY($1))
                 OR EXISTS (SELECT 1 FROM pointcloud_catalogs pc
                             WHERE pc.dataset_id = d.id AND pc.id = ANY($1))
                 OR EXISTS (SELECT 1 FROM pointcloud_patches pp
                              JOIN pointcloud_catalogs pc ON pc.id = pp.catalog_id
                             WHERE pc.dataset_id = d.id AND pp.id = ANY($1))
                 -- an attachment belongs to a dataset directly or to a feature on
                 -- a branch, and the CHECK makes it exactly one of the two
                 OR EXISTS (SELECT 1 FROM attachments a
                        LEFT JOIN branches ab ON ab.id = a.branch_id
                             WHERE COALESCE(a.dataset_id, ab.dataset_id) = d.id
                               AND a.id = ANY($1))
             )",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("id")).collect())
    }

    /// Which of `dataset_ids` this user holds any permission row on, counting a
    /// row on one of the dataset's branches. Org membership does not count: only
    /// an explicit grant opens a private dataset.
    pub async fn readable_datasets(
        &self,
        dataset_ids: &[Uuid],
        user_id: &str,
    ) -> Result<Vec<Uuid>, StoreError> {
        let rows = sqlx::query(
            "SELECT dataset_id FROM dataset_permissions
              WHERE dataset_id = ANY($1) AND user_id = $2
             UNION
             SELECT b.dataset_id FROM branch_permissions bp
               JOIN branches b ON b.id = bp.branch_id
              WHERE b.dataset_id = ANY($1) AND bp.user_id = $2",
        )
        .bind(dataset_ids)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("dataset_id")).collect())
    }

    // ─── Branch CRUD ────────────────────────────────────────────────

    pub async fn create_branch(&self, branch: &Branch, writer: &Writer) -> Result<(), StoreError> {
        self.ensure_dataset_writable(branch.dataset_id, writer)
            .await?;
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
        writer: &Writer,
    ) -> Result<Changeset, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Get current branch head. The external and permission checks ride along
        // on the row lock, so no commit can slip past them.
        let branch_row = sqlx::query(
            "SELECT b.head, b.dataset_id, d.external_table
             FROM branches b JOIN datasets d ON d.id = b.dataset_id
             WHERE b.id = $1 FOR UPDATE OF b",
        )
        .bind(branch_id)
        .fetch_one(&mut *tx)
        .await?;
        if branch_row
            .get::<Option<String>, _>("external_table")
            .is_some()
        {
            return Err(StoreError::Conflict(EXTERNAL_READ_ONLY.into()));
        }
        let parent_id: Option<Uuid> = branch_row.get("head");
        let dataset_id: Uuid = branch_row.get("dataset_id");

        match writer.check() {
            Check::Skip => {}
            Check::Deny => return Err(denied_branch(branch_id)),
            Check::Ladder(user_id) => {
                let (branch, dataset) =
                    write_scopes(&mut *tx, branch_id, dataset_id, user_id).await?;
                if !write_allowed(&branch, &dataset) {
                    return Err(denied_branch(branch_id));
                }
            }
        }

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
                    valid_from,
                    valid_to,
                } => {
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties, valid_from, valid_to)
                         VALUES ($1, $2, $3, 'insert', ST_GeomFromWKB($4, 4326), $5, $6, $7)",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .bind(geometry_wkb)
                    .bind(properties)
                    .bind(valid_from)
                    .bind(valid_to)
                    .execute(&mut *tx)
                    .await?;
                }
                DiffOp::Update {
                    feature_id,
                    geometry_wkb,
                    properties,
                    valid_from,
                    valid_to,
                } => {
                    // fill omitted fields from this branch's own chain, never other branches
                    let geom = if let Some(wkb) = geometry_wkb {
                        wkb.clone()
                    } else {
                        let row = sqlx::query(
                            "WITH RECURSIVE chain AS (
                                SELECT id, parent_id FROM changesets WHERE id = $2
                              UNION ALL
                                SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
                            )
                            SELECT ST_AsBinary(fv.geometry) as geom FROM feature_versions fv
                            JOIN chain ch ON fv.changeset_id = ch.id
                            WHERE fv.feature_id = $1 AND fv.operation != 'delete'
                            ORDER BY fv.id DESC LIMIT 1",
                        )
                        .bind(feature_id)
                        .bind(changeset_id)
                        .fetch_one(&mut *tx)
                        .await?;
                        row.get::<Vec<u8>, _>("geom")
                    };
                    let props = if let Some(p) = properties {
                        p.clone()
                    } else {
                        let row = sqlx::query(
                            "WITH RECURSIVE chain AS (
                                SELECT id, parent_id FROM changesets WHERE id = $2
                              UNION ALL
                                SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
                            )
                            SELECT fv.properties FROM feature_versions fv
                            JOIN chain ch ON fv.changeset_id = ch.id
                            WHERE fv.feature_id = $1 AND fv.operation != 'delete'
                            ORDER BY fv.id DESC LIMIT 1",
                        )
                        .bind(feature_id)
                        .bind(changeset_id)
                        .fetch_one(&mut *tx)
                        .await?;
                        row.get::<serde_json::Value, _>("properties")
                    };
                    let (from, to) = if valid_from.is_none() && valid_to.is_none() {
                        let row = sqlx::query(
                            "WITH RECURSIVE chain AS (
                                SELECT id, parent_id FROM changesets WHERE id = $2
                              UNION ALL
                                SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
                            )
                            SELECT fv.valid_from, fv.valid_to FROM feature_versions fv
                            JOIN chain ch ON fv.changeset_id = ch.id
                            WHERE fv.feature_id = $1 AND fv.operation != 'delete'
                            ORDER BY fv.id DESC LIMIT 1",
                        )
                        .bind(feature_id)
                        .bind(changeset_id)
                        .fetch_one(&mut *tx)
                        .await?;
                        (row.get("valid_from"), row.get("valid_to"))
                    } else {
                        (*valid_from, *valid_to)
                    };
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties, valid_from, valid_to)
                         VALUES ($1, $2, $3, 'update', ST_GeomFromWKB($4, 4326), $5, $6, $7)",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .bind(&geom)
                    .bind(&props)
                    .bind(from)
                    .bind(to)
                    .execute(&mut *tx)
                    .await?;
                }
                DiffOp::Delete { feature_id } => {
                    // no valid time on a delete: there is no version to be true
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
        self.analyzer.after_write(operations.len());

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

    /// Live features on a branch, as SQL every read query can build on: the
    /// prelude declares a `latest` CTE for an ordinary dataset, or is empty and
    /// the source is a derived table over the team's relation for an external
    /// one. Either way the source exposes feature_id, dataset_id, operation,
    /// geometry and properties, and the branch id is `$1`.
    ///
    /// Returns (external source if any, prelude, FROM expression). Handlers that
    /// build their own SQL use this so there is one definition of what a live
    /// feature is.
    pub async fn latest_source(
        &self,
        branch_id: Uuid,
    ) -> Result<(Option<ExternalSource>, String, String), StoreError> {
        self.latest_source_of(branch_id, LATEST_COLUMNS).await
    }

    /// [`Self::latest_source`] with a narrower projection, for queries that do
    /// not need the geometry or properties.
    pub async fn latest_source_of(
        &self,
        branch_id: Uuid,
        columns: &str,
    ) -> Result<(Option<ExternalSource>, String, String), StoreError> {
        self.latest_source_overlapping(branch_id, columns, None)
            .await
    }

    /// [`Self::latest_source_of`] for a read with a spatial restriction.
    ///
    /// `overlaps_4326` is SQL for a geometry in EPSG:4326 that returned rows must
    /// overlap, built by the calling query from its own bind placeholders. An
    /// external source turns it into an index-served pre-filter on the relation's
    /// own geometry column; a versioned dataset ignores it, since its geometry is
    /// already indexed in 4326. The caller keeps its exact predicate either way.
    pub async fn latest_source_overlapping(
        &self,
        branch_id: Uuid,
        columns: &str,
        overlaps_4326: Option<&str>,
    ) -> Result<(Option<ExternalSource>, String, String), StoreError> {
        let external = self.external_for_branch(branch_id).await?;
        match &external {
            None => Ok((None, latest_cte(columns), "latest".to_string())),
            Some(ext) => {
                let source = format!("{} latest", ext.latest_subquery("$1", overlaps_4326));
                Ok((external.clone(), String::new(), source))
            }
        }
    }

    /// What a query should put after FROM where it would say `features`: this
    /// branch's rows from the versioned tables, or a derived table over the
    /// external relation with the same columns. Either way the caller supplies
    /// the alias and binds the branch id as `$1`.
    pub async fn features_source(
        &self,
        branch_id: Uuid,
    ) -> Result<(Option<ExternalSource>, String), StoreError> {
        self.features_source_at(branch_id, "$1").await
    }

    /// [`Self::features_source`] for a query that binds the branch id somewhere
    /// other than `$1`.
    pub async fn features_source_at(
        &self,
        branch_id: Uuid,
        branch_expr: &str,
    ) -> Result<(Option<ExternalSource>, String), StoreError> {
        self.features_source_overlapping(branch_id, branch_expr, None)
            .await
    }

    /// [`Self::features_source_at`] for a read with a spatial restriction; see
    /// [`Self::latest_source_overlapping`] for what `overlaps_4326` must be.
    pub async fn features_source_overlapping(
        &self,
        branch_id: Uuid,
        branch_expr: &str,
        overlaps_4326: Option<&str>,
    ) -> Result<(Option<ExternalSource>, String), StoreError> {
        let external = self.external_for_branch(branch_id).await?;
        let source = match &external {
            None => branch_features_subquery(branch_expr),
            Some(ext) => ext.features_subquery(branch_expr, overlaps_4326),
        };
        Ok((external, source))
    }

    /// Get the current state of all features on a branch (at its head).
    pub async fn list_features_at_head(&self, branch_id: Uuid) -> Result<Vec<Feature>, StoreError> {
        let (external, prelude, source) = self.latest_source(branch_id).await?;
        let rows = sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                   properties, valid_from, valid_to
            FROM {source}
            WHERE operation != 'delete'"
        ))
        .bind(branch_id)
        .fetch_all(self.read_pool(external.as_ref()).await?)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
                valid_from: row.get("valid_from"),
                valid_to: row.get("valid_to"),
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
                   ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties,
                   fv.valid_from, fv.valid_to
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            WHERE fv.feature_id = $1
            ORDER BY fv.created_at DESC, fv.id DESC
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
                valid_from: r.get("valid_from"),
                valid_to: r.get("valid_to"),
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
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties,
                    fv.valid_from, fv.valid_to
                FROM feature_versions fv
                JOIN new_changesets nc ON fv.changeset_id = nc.id
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC",
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
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties,
                    fv.valid_from, fv.valid_to
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC",
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
                        valid_from: row.get("valid_from"),
                        valid_to: row.get("valid_to"),
                    },
                    "update" => DiffOp::Update {
                        feature_id,
                        geometry_wkb: Some(row.get("geometry_wkb")),
                        properties: Some(row.get("properties")),
                        valid_from: row.get("valid_from"),
                        valid_to: row.get("valid_to"),
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
                SELECT id, parent_id, 0 AS depth FROM changesets WHERE id = $1
              UNION ALL
                SELECT c.id, c.parent_id, a.depth + 1 FROM changesets c JOIN ancestors_a a ON a.parent_id = c.id
            ),
            ancestors_b AS (
                SELECT id, parent_id FROM changesets WHERE id = $2
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c JOIN ancestors_b b ON b.parent_id = c.id
            )
            SELECT a.id FROM ancestors_a a
            JOIN ancestors_b b ON a.id = b.id
            ORDER BY a.depth
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
        writer: &Writer,
    ) -> Result<MergeResult, StoreError> {
        self.ensure_branch_writable(target_branch_id, writer)
            .await?;
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
                writer,
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
        writer: &Writer,
    ) -> Result<TopologyMergeResult, StoreError> {
        // First, do the normal merge
        let result = self
            .merge(source_branch_id, target_branch_id, author, writer)
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
                        .auto_repair_topology(target_branch_id, &violations, author, writer)
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
        writer: &Writer,
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
                                valid_from: None,
                                valid_to: None,
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
                                valid_from: None,
                                valid_to: None,
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
                writer,
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

    /// List features with cursor-based pagination, optionally only those valid
    /// at `valid_at`. A row with no valid time recorded always matches, and the
    /// range is half-open: [valid_from, valid_to).
    pub async fn list_features_paginated(
        &self,
        branch_id: Uuid,
        cursor: Option<Uuid>,
        limit: i64,
        valid_at: Option<OffsetDateTime>,
    ) -> Result<Vec<Feature>, StoreError> {
        let (external, prelude, source) = self.latest_source(branch_id).await?;
        let pool = self.read_pool(external.as_ref()).await?;
        let query = if let Some(cursor_id) = cursor {
            sqlx::query(&format!(
                "{prelude}
                SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                       properties, valid_from, valid_to
                FROM {source}
                WHERE operation != 'delete' AND feature_id > $2
                  AND {valid}
                ORDER BY feature_id
                LIMIT $3",
                valid = valid_at_predicate("$4")
            ))
            .bind(branch_id)
            .bind(cursor_id)
            .bind(limit)
            .bind(valid_at)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query(&format!(
                "{prelude}
                SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                       properties, valid_from, valid_to
                FROM {source}
                WHERE operation != 'delete'
                  AND {valid}
                ORDER BY feature_id
                LIMIT $2",
                valid = valid_at_predicate("$3")
            ))
            .bind(branch_id)
            .bind(limit)
            .bind(valid_at)
            .fetch_all(pool)
            .await?
        };

        Ok(query
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
                valid_from: row.get("valid_from"),
                valid_to: row.get("valid_to"),
            })
            .collect())
    }

    /// Search live features whose text property contains the query,
    /// case-insensitive. The filter runs in SQL so the limit applies to
    /// matches, not to an arbitrary prefix of the branch.
    pub async fn search_features_by_property(
        &self,
        branch_id: Uuid,
        key: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Feature>, StoreError> {
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let (external, prelude, source) = self.latest_source(branch_id).await?;
        let rows = sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                   properties, valid_from, valid_to
            FROM {source}
            WHERE operation != 'delete'
              AND properties->>$2 ILIKE '%' || $3 || '%'
            ORDER BY feature_id
            LIMIT $4"
        ))
        .bind(branch_id)
        .bind(key)
        .bind(escaped)
        .bind(limit)
        .fetch_all(self.read_pool(external.as_ref()).await?)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
                valid_from: row.get("valid_from"),
                valid_to: row.get("valid_to"),
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
        let (external, prelude, source) = self
            .latest_source_overlapping(
                branch_id,
                LATEST_COLUMNS,
                Some("ST_MakeEnvelope($2, $3, $4, $5, 4326)"),
            )
            .await?;
        let rows = sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                   properties, valid_from, valid_to
            FROM {source}
            WHERE operation != 'delete'
              AND geometry && ST_MakeEnvelope($2, $3, $4, $5, 4326)
            LIMIT $6"
        ))
        .bind(branch_id)
        .bind(min_x)
        .bind(min_y)
        .bind(max_x)
        .bind(max_y)
        .bind(limit)
        .fetch_all(self.read_pool(external.as_ref()).await?)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
                valid_from: row.get("valid_from"),
                valid_to: row.get("valid_to"),
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
        let (external, prelude, source) = self
            .latest_source_overlapping(branch_id, LATEST_COLUMNS, Some("ST_GeomFromGeoJSON($2)"))
            .await?;
        let rows = sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                   properties, valid_from, valid_to
            FROM {source}
            WHERE operation != 'delete'
              AND ST_Intersects(geometry, ST_GeomFromGeoJSON($2))
            LIMIT $3"
        ))
        .bind(branch_id)
        .bind(geojson_geometry)
        .bind(limit)
        .fetch_all(self.read_pool(external.as_ref()).await?)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
                valid_from: row.get("valid_from"),
                valid_to: row.get("valid_to"),
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
        // ST_Within(geometry, w) implies the row overlaps w, so w is a sound
        // window for the pre-filter here too
        let (external, prelude, source) = self
            .latest_source_overlapping(branch_id, LATEST_COLUMNS, Some("ST_GeomFromGeoJSON($2)"))
            .await?;
        let rows = sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                   properties, valid_from, valid_to
            FROM {source}
            WHERE operation != 'delete'
              AND ST_Within(geometry, ST_GeomFromGeoJSON($2))
            LIMIT $3"
        ))
        .bind(branch_id)
        .bind(geojson_geometry)
        .bind(limit)
        .fetch_all(self.read_pool(external.as_ref()).await?)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Feature {
                id: row.get("feature_id"),
                dataset_id: row.get("dataset_id"),
                geometry_wkb: row.get("geometry_wkb"),
                properties: row.get("properties"),
                valid_from: row.get("valid_from"),
                valid_to: row.get("valid_to"),
            })
            .collect())
    }

    /// Count features at branch head.
    pub async fn count_features_at_head(&self, branch_id: Uuid) -> Result<i64, StoreError> {
        let (external, prelude, source) = self
            .latest_source_of(branch_id, "fv.feature_id, fv.operation")
            .await?;
        let row = sqlx::query(&format!(
            "{prelude}
            SELECT COUNT(*) as cnt
            FROM {source}
            WHERE operation != 'delete'"
        ))
        .bind(branch_id)
        .fetch_one(self.read_pool(external.as_ref()).await?)
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
        let (external, prelude, source) = self
            .latest_source_overlapping(
                branch_id,
                LATEST_COLUMNS,
                Some("ST_Transform(ST_TileEnvelope($2::integer, $3::integer, $4::integer), 4326)"),
            )
            .await?;
        let latest_cte = if prelude.is_empty() {
            format!("WITH latest AS (SELECT * FROM {source}),")
        } else {
            format!("{prelude},")
        };
        let row = sqlx::query(&format!(
            "{latest_cte}
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
            FROM mvtgeom"
        ))
        .bind(branch_id)
        .bind(z as i32)
        .bind(x as i32)
        .bind(y as i32)
        .fetch_one(self.read_pool(external.as_ref()).await?)
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
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
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
        writer: &Writer,
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
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
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
                valid_from: None,
                valid_to: None,
            })
            .collect();

        let count = ops.len();
        let changeset = self
            .commit(
                branch_id,
                &format!("Auto-repair: fixed {} invalid geometries", count),
                author,
                &ops,
                writer,
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
                -- Find the changeset that was head at the given time. The
                -- parentheses are required: postgres rejects ORDER BY / LIMIT in
                -- a bare non-recursive term.
                (SELECT c.id, c.parent_id FROM changesets c
                 WHERE c.branch_id = $1 AND c.created_at <= $2
                 ORDER BY c.created_at DESC LIMIT 1)
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c
                JOIN chain ch ON ch.parent_id = c.id
            ),
            latest AS (
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation,
                    ST_AsGeoJSON(fv.geometry)::jsonb as geojson,
                    fv.properties, fv.valid_from, fv.valid_to
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                WHERE fv.created_at <= $2
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
            )
            SELECT feature_id, geojson, properties, valid_from, valid_to
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
                    valid_from: row.get("valid_from"),
                    valid_to: row.get("valid_to"),
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
        // make_interval's mins is int4, and a lock is short-lived by design, so
        // clamp rather than let a caller's duration overflow the interval
        let mins = duration_minutes.clamp(1, 60 * 24 * 30) as i32;

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
            .bind(mins)
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
            .bind(mins)
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
            "INSERT INTO attachments (id, feature_id, branch_id, dataset_id, name, content_type, size_bytes, data, thumbnail, metadata, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(attachment.id)
        .bind(attachment.feature_id)
        .bind(attachment.branch_id)
        .bind(attachment.dataset_id)
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
            "SELECT id, feature_id, branch_id, dataset_id, name, content_type, size_bytes, metadata, created_by, created_at
             FROM attachments
             WHERE feature_id = $1 AND branch_id = $2
             ORDER BY created_at DESC",
        )
        .bind(feature_id)
        .bind(branch_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(attachment_meta_from_row).collect())
    }

    /// The dataset's own attachments: the ones a style refers to, which belong
    /// to no single feature.
    pub async fn list_dataset_attachments(
        &self,
        dataset_id: Uuid,
    ) -> Result<Vec<AttachmentMeta>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, feature_id, branch_id, dataset_id, name, content_type, size_bytes, metadata, created_by, created_at
             FROM attachments
             WHERE dataset_id = $1
             ORDER BY created_at DESC",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(attachment_meta_from_row).collect())
    }

    pub async fn get_attachment(&self, id: Uuid) -> Result<Attachment, StoreError> {
        let row = sqlx::query(
            "SELECT id, feature_id, branch_id, dataset_id, name, content_type, size_bytes, data, thumbnail, metadata, created_by, created_at
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
            dataset_id: row.get("dataset_id"),
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

    /// Revoking is refused when it would strand the dataset: removing its last
    /// `admin` row leaves nobody able to manage grants, and removing its last row
    /// of any kind drops it back to "no rows means open", quietly handing write
    /// access to every editor. Grant a replacement first. The rule binds instance
    /// admins too, because the second case is a downgrade of the dataset's
    /// protection rather than a question of who is asking.
    pub async fn revoke_dataset_permission(
        &self,
        dataset_id: Uuid,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        // locked, so two concurrent revokes cannot each see the other's row and
        // both conclude a second admin remains
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT user_id, permission FROM dataset_permissions
              WHERE dataset_id = $1 FOR UPDATE",
        )
        .bind(dataset_id)
        .fetch_all(&mut *tx)
        .await?;

        let target_exists = rows.iter().any(|(u, _)| u == user_id);
        if target_exists {
            if rows.len() == 1 {
                return Err(StoreError::Forbidden(format!(
                    "{user_id} holds the only permission row on dataset {dataset_id}: revoking it \
                     would reopen the dataset to every editor. Grant someone else first."
                )));
            }
            let admins = rows.iter().filter(|(_, p)| p == "admin").count();
            let target_is_admin = rows.iter().any(|(u, p)| u == user_id && p == "admin");
            if target_is_admin && admins == 1 {
                return Err(StoreError::Forbidden(format!(
                    "{user_id} is the only admin of dataset {dataset_id}: revoking it would leave \
                     nobody able to manage its permissions. Grant another admin first."
                )));
            }
        }

        sqlx::query("DELETE FROM dataset_permissions WHERE dataset_id = $1 AND user_id = $2")
            .bind(dataset_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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
        writer: &Writer,
    ) -> Result<CompactionResult, StoreError> {
        self.ensure_branch_writable(branch_id, writer).await?;
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
                    ROW_NUMBER() OVER (PARTITION BY fv.feature_id ORDER BY fv.created_at DESC, fv.id DESC) as rn
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
        self.analyzer.after_write(removed as usize);

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

/// Owned by either a feature on a branch or a dataset, never both and never
/// neither; the `attachments_one_owner` CHECK is the authority.
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub id: Uuid,
    pub feature_id: Option<Uuid>,
    pub branch_id: Option<Uuid>,
    pub dataset_id: Option<Uuid>,
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
    pub feature_id: Option<Uuid>,
    pub branch_id: Option<Uuid>,
    pub dataset_id: Option<Uuid>,
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
/// Give the creator an admin row so a dataset created with auth on always has an
/// owner who can grant to others.
async fn insert_creator_admin_grant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dataset_id: Uuid,
    creator: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO dataset_permissions (id, dataset_id, user_id, permission, granted_by)
         VALUES ($1, $2, $3, 'admin', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(dataset_id)
    .bind(creator)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The branch and dataset permission scopes for one writer, in one round trip.
async fn write_scopes<'e, E>(
    exec: E,
    branch_id: Uuid,
    dataset_id: Uuid,
    user_id: &str,
) -> Result<(Scope, Scope), StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query(
        "SELECT
            (SELECT count(*) FROM branch_permissions WHERE branch_id = $1) AS branch_total,
            (SELECT permission FROM branch_permissions
              WHERE branch_id = $1 AND user_id = $3) AS branch_mine,
            (SELECT count(*) FROM dataset_permissions WHERE dataset_id = $2) AS dataset_total,
            (SELECT permission FROM dataset_permissions
              WHERE dataset_id = $2 AND user_id = $3) AS dataset_mine",
    )
    .bind(branch_id)
    .bind(dataset_id)
    .bind(user_id)
    .fetch_one(exec)
    .await?;

    Ok((
        Scope {
            enforced: row.get::<i64, _>("branch_total") > 0,
            mine: row.get("branch_mine"),
        },
        Scope {
            enforced: row.get::<i64, _>("dataset_total") > 0,
            mine: row.get("dataset_mine"),
        },
    ))
}

async fn dataset_scope<'e, E>(exec: E, dataset_id: Uuid, user_id: &str) -> Result<Scope, StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query(
        "SELECT
            (SELECT count(*) FROM dataset_permissions WHERE dataset_id = $1) AS total,
            (SELECT permission FROM dataset_permissions
              WHERE dataset_id = $1 AND user_id = $2) AS mine",
    )
    .bind(dataset_id)
    .bind(user_id)
    .fetch_one(exec)
    .await?;

    Ok(Scope {
        enforced: row.get::<i64, _>("total") > 0,
        mine: row.get("mine"),
    })
}

fn denied_branch(branch_id: Uuid) -> StoreError {
    StoreError::Forbidden(format!(
        "no write permission on branch {branch_id}: ask an admin of the dataset for a write or \
         admin grant"
    ))
}

fn denied_dataset(dataset_id: Uuid) -> StoreError {
    StoreError::Forbidden(format!(
        "no write permission on dataset {dataset_id}: ask an admin of the dataset for a write or \
         admin grant"
    ))
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
                valid_from: vfa,
                valid_to: vta,
            },
            DiffOp::Insert {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
                valid_from: vfb,
                valid_to: vtb,
            },
        ) => fa == fb && ga == gb && pa == pb && vfa == vfb && vta == vtb,
        (
            DiffOp::Update {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
                valid_from: vfa,
                valid_to: vta,
            },
            DiffOp::Update {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
                valid_from: vfb,
                valid_to: vtb,
            },
        ) => fa == fb && ga == gb && pa == pb && vfa == vfb && vta == vtb,
        (DiffOp::Delete { feature_id: fa }, DiffOp::Delete { feature_id: fb }) => fa == fb,
        _ => false,
    }
}

/// Rebuild the validated [`ExternalTable`] from a `datasets` row. Names in the
/// table passed validation when they were written, so a row that fails it now
/// was tampered with outside the API and must not reach a query.
fn attachment_meta_from_row(row: sqlx::postgres::PgRow) -> AttachmentMeta {
    AttachmentMeta {
        id: row.get("id"),
        feature_id: row.get("feature_id"),
        branch_id: row.get("branch_id"),
        dataset_id: row.get("dataset_id"),
        name: row.get("name"),
        content_type: row.get("content_type"),
        size_bytes: row.get("size_bytes"),
        metadata: row.get("metadata"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
    }
}

fn external_table_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<ExternalTable>, StoreError> {
    let Some(table) = row.get::<Option<String>, _>("external_table") else {
        return Ok(None);
    };
    let id_column: Option<String> = row.get("external_id_column");
    let geometry_column: Option<String> = row.get("external_geometry_column");
    let (Some(id_column), Some(geometry_column)) = (id_column, geometry_column) else {
        return Err(StoreError::Conflict(
            "external dataset row is missing its column names".into(),
        ));
    };
    ExternalTable::parse(&table, &id_column, &geometry_column)
        .map(Some)
        .map_err(|e| StoreError::Conflict(e.to_string()))
}

fn external_source_from_row(
    row: sqlx::postgres::PgRow,
) -> Result<Option<ExternalSource>, StoreError> {
    Ok(external_table_from_row(&row)?.map(|table| ExternalSource {
        dataset_id: row.get("id"),
        srid: row.get("srid"),
        table,
    }))
}

fn dataset_from_row(row: sqlx::postgres::PgRow) -> Result<Dataset, StoreError> {
    let visibility = row.get::<String, _>("visibility");
    Ok(Dataset {
        external: external_table_from_row(&row)?,
        id: row.get("id"),
        name: row.get("name"),
        srid: row.get("srid"),
        geometry_type: parse_geometry_type(row.get::<String, _>("geometry_type")),
        created_at: row.get("created_at"),
        created_by: row.get("created_by"),
        // the column has a CHECK constraint, so an unknown value cannot be stored
        visibility: Visibility::parse(&visibility).unwrap_or_default(),
    })
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
        "geometry" => GeometryType::Geometry,
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
