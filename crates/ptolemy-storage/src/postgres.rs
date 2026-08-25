// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! PostgreSQL/PostGIS backend for the versioned feature store.

use crate::analyze::Analyzer;
use crate::grant::WriteGrant;
use crate::permission::{
    Check, Reader, Scope, Writer, permission_level, stronger_permission, visible_datasets_sql,
    write_allowed,
};
use crate::workspace::{effective_project_role_sql, parse_effective_role};
use ptolemy_core::Feature;
use ptolemy_core::branch::Branch;
use ptolemy_core::changeset::Changeset;
use ptolemy_core::dataset::{Dataset, GeometryType, Visibility};
use ptolemy_core::diff::{Diff, DiffOp, NativeGeometry};
use ptolemy_core::event::{Event, EventType, Webhook};
use ptolemy_core::external::{ExternalSource, ExternalTable};
use ptolemy_core::review::{MergeRequest, MergeRequestStatus, ReviewComment};
use ptolemy_core::schema::{
    DatasetSchema, FieldDef, GeometryRules, QualityReport, QualityStatistics,
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

/// Tile-space grid an MVT geometry is quantized onto, and the MVT default.
pub const MVT_TILE_EXTENT: i32 = 4096;

/// Width of the web mercator plane in metres, the span one tile covers at zoom 0.
const WEB_MERCATOR_SPAN_METRES: f64 = 40_075_016.685_578_5;

/// Douglas-Peucker tolerance for a tile at this zoom, in web mercator metres.
///
/// Half a tile unit, so a simplified vertex lands on the same grid cell it would
/// have without this or the one beside it. ST_AsMVTGeom transforms and clips but
/// never drops a vertex, so without simplifying first every vertex of a dense
/// line reaches the tile and rounds onto a coordinate its neighbour already
/// holds.
pub fn mvt_simplify_tolerance(z: i32) -> f64 {
    WEB_MERCATOR_SPAN_METRES / (f64::from(MVT_TILE_EXTENT) * 2f64.powi(z) * 2.0)
}

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

/// A recursive CTE named `name` holding the changeset bound to `start` and every
/// changeset it can reach, following both parents so a merge commit reaches the
/// source branch it brought in. The caller puts it after `WITH RECURSIVE`.
///
/// `UNION` rather than `UNION ALL`: history is a DAG, and a shared ancestor
/// below two merged lines would otherwise be walked once per path through it.
///
/// This answers whether one changeset is reachable from another, which is not
/// the same question as which versions make up a branch's current state. The
/// state walks stay on `parent_id`: a merge commit records what it brought in,
/// so the source's own versions would only add older duplicates.
fn ancestors_cte(name: &str, start: &str) -> String {
    format!(
        "{name} AS (
            SELECT id, parent_id, merge_parent_id FROM changesets WHERE id = {start}
          UNION
            SELECT c.id, c.parent_id, c.merge_parent_id FROM changesets c
            JOIN {name} a ON c.id = a.parent_id OR c.id = a.merge_parent_id
        )"
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

/// The owner a write has to be allowed on, resolved from whatever id the request
/// named. See [`PgStore::write_targets_for_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteTarget {
    Branch(Uuid),
    Dataset(Uuid),
}

pub struct PgStore {
    /// `pub(crate)` so the guarded writes in [`crate::writes`] can reach it.
    /// Outside this crate the only handles are [`PgStore::read_pool`] and
    /// [`PgStore::unguarded_pool`].
    pub(crate) pool: PgPool,
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

    /// The pool a caller runs reads on. Every `SELECT` in `ptolemy-api` uses
    /// this.
    ///
    /// It is the same handle a write would run on, so the name is a convention
    /// and not a barrier: nothing in the type system stops an `INSERT` here.
    /// What actually keeps write SQL out of `ptolemy-api` is
    /// `ci/no-raw-writes.sh`; what makes the guarded path the easy one is that
    /// every write already has a [`WriteGrant`]-taking method in
    /// [`crate::writes`].
    pub fn read_pool(&self) -> &PgPool {
        &self.pool
    }

    /// The pool for callers with no request and no write ladder behind them:
    /// the CLI's admin commands and test fixtures. `ci/no-raw-writes.sh`
    /// refuses this name anywhere in `ptolemy-api`.
    pub fn unguarded_pool(&self) -> &PgPool {
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
    /// the dataset with no rows, and with auth on later such a dataset is
    /// writable by instance admins only until one of them grants.
    pub async fn create_dataset(
        &self,
        ds: &Dataset,
        grant_admin_to: Option<&str>,
    ) -> Result<(), StoreError> {
        let geom_type = format!("{:?}", ds.geometry_type).to_lowercase();
        let mut tx = self.pool.begin().await?;
        // `project_id` is deliberately absent: a create that wrote it would attach
        // a dataset to a project without the editor-or-owner check the attach
        // endpoint makes, so the column is left NULL and only attach sets it
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

        // no branch_created event: the dataset is being created here, so it has
        // no subscriptions yet and nobody could be told
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
        // resolving the dataset first means a missing one is a 404, not the 403
        // the ladder below would answer for it
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
        if write_allowed(&Scope::empty(), &dataset) {
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

    /// The same ladder for an existing attachment, whose owner is either a branch
    /// or a dataset. One query reads the two owner columns, so a delete does not
    /// have to load the blob to find out who guards it.
    ///
    /// A tombstoned attachment is absent, so a second delete is refused with the
    /// same not-found the first one after it was hard deleted used to give.
    pub async fn ensure_attachment_writable(
        &self,
        attachment_id: Uuid,
        writer: &Writer,
    ) -> Result<(), StoreError> {
        let row = sqlx::query(
            "SELECT branch_id, dataset_id, project_id FROM attachments
              WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(attachment_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("attachment {attachment_id}")))?;

        match (
            row.get::<Option<Uuid>, _>("branch_id"),
            row.get::<Option<Uuid>, _>("dataset_id"),
            row.get::<Option<Uuid>, _>("project_id"),
        ) {
            (Some(branch_id), _, _) => self.ensure_branch_writable(branch_id, writer).await,
            (None, Some(dataset_id), _) => self.ensure_dataset_writable(dataset_id, writer).await,
            // a project attachment has no dataset for the ladder to run against.
            // It is absent here, so `/attachments/{id}` cannot reach it and
            // `/projects/{id}/attachments/{id}`, which checks the project role,
            // is the only way to delete one.
            (None, None, _) => Err(StoreError::NotFound(format!("attachment {attachment_id}"))),
        }
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
    /// Distinct from [`PgStore::read_pool`], which is the primary handle on its
    /// own: this one picks between the primary and the external database.
    pub async fn source_pool(
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
                    external_table, external_id_column, external_geometry_column, visibility,
                    project_id
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
                    d.visibility, d.project_id
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

    /// Whether the caller administers a dataset: an `admin` permission row on it,
    /// or the owner role on the project it is attached to. Used to decide who may
    /// change its visibility, manage its grants, and attach or detach it.
    pub async fn is_dataset_admin(&self, id: Uuid, user_id: &str) -> Result<bool, StoreError> {
        let scope = dataset_scope(&self.pool, id, user_id).await?;
        let level = scope.mine.as_deref().map(permission_level).unwrap_or(0);
        Ok(level >= permission_level("admin"))
    }

    /// The project a dataset is attached to, `None` when it is attached to none.
    /// The outer `None` is a dataset that does not exist.
    pub async fn dataset_project(&self, dataset_id: Uuid) -> Result<Option<Uuid>, StoreError> {
        sqlx::query_scalar::<_, Option<Uuid>>("SELECT project_id FROM datasets WHERE id = $1")
            .bind(dataset_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("dataset {dataset_id}")))
    }

    /// Attach a dataset to a project and make it private, in one transaction, so
    /// the dataset is never attached while still readable by anyone who asks.
    ///
    /// The caller enforces who may do this: an admin on the dataset who can also
    /// edit the target project.
    pub async fn attach_dataset_to_project(
        &self,
        dataset_id: Uuid,
        project_id: Uuid,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        // lock the dataset row first, so a concurrent attach or visibility flip
        // cannot land between the project link and the private flag
        sqlx::query("SELECT id FROM datasets WHERE id = $1 FOR UPDATE")
            .bind(dataset_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("dataset {dataset_id}")))?;
        // the foreign key would refuse an unknown project as a database error,
        // and a missing project is a 404. FOR KEY SHARE is the lock the key
        // check below takes anyway, and holding it from here means a project
        // deleted in between cannot turn the 404 into a 500.
        sqlx::query("SELECT id FROM projects WHERE id = $1 FOR KEY SHARE")
            .bind(project_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("project {project_id}")))?;
        sqlx::query("UPDATE datasets SET project_id = $2, visibility = 'private' WHERE id = $1")
            .bind(dataset_id)
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drop a dataset's project link. Visibility is left alone: the dataset was
    /// made private when it was attached and stays private until an admin
    /// publishes it, so detaching can only ever close access.
    ///
    /// `expected_project_id` is the project the caller was authorized against. A
    /// dataset that moved in the meantime is refused rather than detached from a
    /// project nobody checked.
    pub async fn detach_dataset_from_project(
        &self,
        dataset_id: Uuid,
        expected_project_id: Uuid,
    ) -> Result<(), StoreError> {
        let affected =
            sqlx::query("UPDATE datasets SET project_id = NULL WHERE id = $1 AND project_id = $2")
                .bind(dataset_id)
                .bind(expected_project_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        if affected == 0 {
            return Err(StoreError::Conflict(format!(
                "dataset {dataset_id} is no longer attached to project {expected_project_id}"
            )));
        }
        Ok(())
    }

    // ─── Visibility enforcement (read paths) ────────────────────────

    /// The private datasets any of `ids` refers to. Every kind of id a route can
    /// name resolves here: a dataset, a branch, a changeset, a merge request, a
    /// feature, a raster catalog or tile, a point cloud catalog or patch, an
    /// attachment, a network, an LRS route, a symbology or label rule, a domain,
    /// a subtype, an attribute rule, a trajectory, a webhook, a relationship
    /// class or a relationship record. Public datasets are left out, so an empty
    /// result means nothing to enforce.
    ///
    /// An id kind missing from this query is an unguarded read of private
    /// content, so a new dataset-owned table belongs here at the same time as its
    /// route. Ids that deliberately resolve to nothing: replication peers,
    /// which are not dataset content.
    ///
    /// Shape matters here, because this runs on every request that names a uuid.
    /// Each branch resolves an id to its owning dataset by primary key, and the
    /// visibility filter is then a lookup of those few dataset ids, so the cost
    /// does not grow with the number of private datasets. Asking the question the
    /// other way round (which private datasets own one of these ids) re-runs
    /// every branch for every private dataset in the instance: measured at 5k
    /// private datasets that is 639ms against 0.6ms.
    pub async fn private_datasets_for_ids(&self, ids: &[Uuid]) -> Result<Vec<Uuid>, StoreError> {
        let rows = sqlx::query(
            "WITH owners AS (
                 SELECT id AS dataset_id FROM datasets WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM branches WHERE id = ANY($1)
                 UNION ALL SELECT b.dataset_id FROM changesets c
                             JOIN branches b ON b.id = c.branch_id
                            WHERE c.id = ANY($1)
                 UNION ALL SELECT dataset_id FROM merge_requests WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM feature_versions WHERE feature_id = ANY($1)
                 UNION ALL SELECT dataset_id FROM raster_catalogs WHERE id = ANY($1)
                 UNION ALL SELECT rc.dataset_id FROM raster_tiles rt
                             JOIN raster_catalogs rc ON rc.id = rt.catalog_id
                            WHERE rt.id = ANY($1)
                 UNION ALL SELECT dataset_id FROM pointcloud_catalogs WHERE id = ANY($1)
                 UNION ALL SELECT pc.dataset_id FROM pointcloud_patches pp
                             JOIN pointcloud_catalogs pc ON pc.id = pp.catalog_id
                            WHERE pp.id = ANY($1)
                 -- an attachment belongs to a dataset directly, to a feature on a
                 -- branch, or to a project, and the CHECK makes it exactly one.
                 -- A project one names no dataset and is read through routes
                 -- that check the project role.
                 UNION ALL SELECT COALESCE(a.dataset_id, ab.dataset_id) FROM attachments a
                        LEFT JOIN branches ab ON ab.id = a.branch_id
                            WHERE a.id = ANY($1) AND a.project_id IS NULL
                 UNION ALL SELECT dataset_id FROM networks WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM routes WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM symbology_rules WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM label_rules WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM domains WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM subtypes WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM attribute_rules WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM trajectories WHERE id = ANY($1)
                 UNION ALL SELECT dataset_id FROM webhooks WHERE id = ANY($1)
                 -- a relationship class spans two datasets, and both of them have
                 -- to be readable, so both go in the result
                 UNION ALL SELECT unnest(ARRAY[origin_dataset_id, destination_dataset_id])
                             FROM relationship_classes WHERE id = ANY($1)
                 UNION ALL SELECT unnest(ARRAY[rc.origin_dataset_id, rc.destination_dataset_id])
                             FROM relationship_records rr
                             JOIN relationship_classes rc ON rc.id = rr.relationship_class_id
                            WHERE rr.id = ANY($1)
             )
             -- ANY(ARRAY(...)) rather than a join to owners: the join makes the
             -- planner seq-scan every dataset and hash-join, 16ms against 1.3ms
             SELECT id FROM datasets
              WHERE visibility = 'private'
                AND id = ANY(ARRAY(SELECT dataset_id FROM owners))",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("id")).collect())
    }

    /// Which of `dataset_ids` this user may read: those they hold a permission row
    /// on, counting a row on one of the dataset's branches, plus those attached to
    /// a project they hold any effective role on.
    pub async fn readable_datasets(
        &self,
        dataset_ids: &[Uuid],
        user_id: &str,
    ) -> Result<Vec<Uuid>, StoreError> {
        let project_role = effective_project_role_sql("d.project_id", 2);
        let rows = sqlx::query(&format!(
            "SELECT dataset_id FROM dataset_permissions
              WHERE dataset_id = ANY($1) AND user_id = $2
             UNION
             SELECT b.dataset_id FROM branch_permissions bp
               JOIN branches b ON b.id = bp.branch_id
              WHERE b.dataset_id = ANY($1) AND bp.user_id = $2
             UNION
             SELECT d.id AS dataset_id FROM datasets d
              WHERE d.id = ANY($1) AND {project_role} IS NOT NULL"
        ))
        .bind(dataset_ids)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("dataset_id")).collect())
    }

    /// What a mutation aimed at `id` writes to, so the write gate can run the
    /// ladder without knowing which route it is guarding. The mirror of
    /// [`PgStore::private_datasets_for_ids`] for writes: the same set of tables,
    /// resolved to the owner the ladder takes rather than to a visibility flag.
    ///
    /// A branch is returned as a branch, not as its dataset, so
    /// [`PgStore::ensure_branch_writable`] can apply the branch scope. A
    /// relationship class spans two datasets and yields both, and the caller has
    /// to be allowed on each.
    ///
    /// Empty means the id names nothing this instance owns. There is nothing to
    /// check, and the write fails on its own foreign key.
    pub async fn write_targets_for_id(&self, id: Uuid) -> Result<Vec<WriteTarget>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT branch_id, dataset_id FROM (
                 SELECT NULL::uuid AS branch_id, id AS dataset_id FROM datasets WHERE id = $1
                 UNION ALL SELECT id, dataset_id FROM branches WHERE id = $1
                 UNION ALL SELECT c.branch_id, b.dataset_id FROM changesets c
                             JOIN branches b ON b.id = c.branch_id WHERE c.id = $1
                 -- a review is approved, closed and commented on against the
                 -- branch it would land on, so a branch grantee keeps working
                 UNION ALL SELECT target_branch_id, dataset_id FROM merge_requests WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM feature_versions WHERE feature_id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM raster_catalogs WHERE id = $1
                 UNION ALL SELECT NULL::uuid, rc.dataset_id FROM raster_tiles rt
                             JOIN raster_catalogs rc ON rc.id = rt.catalog_id WHERE rt.id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM pointcloud_catalogs WHERE id = $1
                 UNION ALL SELECT NULL::uuid, pc.dataset_id FROM pointcloud_patches pp
                             JOIN pointcloud_catalogs pc ON pc.id = pp.catalog_id WHERE pp.id = $1
                 -- a project attachment resolves to neither, and is guarded by
                 -- the project role its own routes check instead
                 UNION ALL SELECT a.branch_id, COALESCE(a.dataset_id, ab.dataset_id) FROM attachments a
                        LEFT JOIN branches ab ON ab.id = a.branch_id
                        WHERE a.id = $1 AND a.project_id IS NULL
                 UNION ALL SELECT NULL::uuid, dataset_id FROM networks WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM routes WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM symbology_rules WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM label_rules WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM domains WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM subtypes WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM attribute_rules WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM trajectories WHERE id = $1
                 UNION ALL SELECT NULL::uuid, dataset_id FROM webhooks WHERE id = $1
                 UNION ALL SELECT NULL::uuid, unnest(ARRAY[origin_dataset_id, destination_dataset_id])
                             FROM relationship_classes WHERE id = $1
                 UNION ALL SELECT NULL::uuid, unnest(ARRAY[rc.origin_dataset_id, rc.destination_dataset_id])
                             FROM relationship_records rr
                             JOIN relationship_classes rc ON rc.id = rr.relationship_class_id
                            WHERE rr.id = $1
             ) owners",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| match r.get::<Option<Uuid>, _>("branch_id") {
                Some(branch_id) => WriteTarget::Branch(branch_id),
                None => WriteTarget::Dataset(r.get("dataset_id")),
            })
            .collect())
    }

    /// Run the ladder for every target [`PgStore::write_targets_for_id`] found.
    ///
    /// The [`WriteGrant`] it returns is the proof a guarded write demands, and
    /// this is the only thing that mints one. It carries `id`, so a write that
    /// scopes itself by the grant is aimed at exactly what was checked here.
    pub async fn ensure_id_writable(
        &self,
        id: Uuid,
        writer: &Writer,
    ) -> Result<WriteGrant, StoreError> {
        for target in self.write_targets_for_id(id).await? {
            match target {
                WriteTarget::Branch(branch_id) => {
                    self.ensure_branch_writable(branch_id, writer).await?
                }
                WriteTarget::Dataset(dataset_id) => {
                    self.ensure_dataset_writable(dataset_id, writer).await?
                }
            }
        }
        Ok(WriteGrant::issue(id))
    }

    // ─── Branch CRUD ────────────────────────────────────────────────

    pub async fn create_branch(&self, branch: &Branch, writer: &Writer) -> Result<(), StoreError> {
        self.ensure_dataset_writable(branch.dataset_id, writer)
            .await?;
        let mut tx = self.pool.begin().await?;
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
        .execute(&mut *tx)
        .await?;
        queue_event(
            &mut tx,
            branch.dataset_id,
            EventType::BranchCreated,
            &serde_json::json!({
                "dataset_id": branch.dataset_id,
                "branch_id": branch.id,
                "name": branch.name,
                "head": branch.head,
                "created_by": branch.created_by,
            }),
        )
        .await?;
        tx.commit().await?;
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
        self.commit_merge(branch_id, message, author, operations, None, writer)
            .await
    }

    /// A commit that also records `merge_parent`, the source branch head it
    /// brought in. Every later merge base walk sees that head as reached, so
    /// merging the same branch again starts from it instead of from the fork.
    pub async fn commit_merge(
        &self,
        branch_id: Uuid,
        message: &str,
        author: &str,
        operations: &[DiffOp],
        merge_parent: Option<Uuid>,
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
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("branch {branch_id}")))?;
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

        // Create changeset.
        //
        // The time comes from the database rather than from this process, and is
        // read back so the answer says what was stored. Every other timestamp a
        // commit writes already came from there: feature_versions.created_at and
        // change_feed.created_at are column defaults, and so is an attachment's.
        // The ArcGIS facade's change tracking compares a changeset against those
        // attachment timestamps, so two clocks there put its window boundary out
        // by whatever they disagreed by.
        let changeset_id = Uuid::now_v7();
        let now: OffsetDateTime = sqlx::query(
            "INSERT INTO changesets (id, branch_id, parent_id, merge_parent_id, message, author, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, now())
             RETURNING created_at",
        )
        .bind(changeset_id)
        .bind(branch_id)
        .bind(parent_id)
        .bind(merge_parent)
        .bind(message)
        .bind(author)
        .fetch_one(&mut *tx)
        .await?
        .get("created_at");

        // Apply operations as feature_versions
        for op in operations {
            match op {
                DiffOp::Insert {
                    feature_id,
                    geometry_wkb,
                    properties,
                    native,
                    valid_from,
                    valid_to,
                } => {
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties, valid_from, valid_to, native_geometry, native_crs_wkt)
                         VALUES ($1, $2, $3, 'insert', ST_GeomFromWKB($4, 4326), $5, $6, $7, ST_GeomFromWKB($8, $9), $10)",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .bind(geometry_wkb)
                    .bind(properties)
                    .bind(valid_from)
                    .bind(valid_to)
                    .bind(native.as_ref().map(|n| n.wkb()))
                    .bind(native.as_ref().map(|n| n.srid().unwrap_or(0)))
                    .bind(native.as_ref().and_then(|n| n.crs_wkt()))
                    .execute(&mut *tx)
                    .await?;
                }
                DiffOp::Update {
                    feature_id,
                    geometry_wkb,
                    properties,
                    native,
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
                    // native is not inherited the way the fields above are: an
                    // omitted one means the new version has no original
                    sqlx::query(
                        "INSERT INTO feature_versions (feature_id, dataset_id, changeset_id, operation, geometry, properties, valid_from, valid_to, native_geometry, native_crs_wkt)
                         VALUES ($1, $2, $3, 'update', ST_GeomFromWKB($4, 4326), $5, $6, $7, ST_GeomFromWKB($8, $9), $10)",
                    )
                    .bind(feature_id)
                    .bind(dataset_id)
                    .bind(changeset_id)
                    .bind(&geom)
                    .bind(&props)
                    .bind(from)
                    .bind(to)
                    .bind(native.as_ref().map(|n| n.wkb()))
                    .bind(native.as_ref().map(|n| n.srid().unwrap_or(0)))
                    .bind(native.as_ref().and_then(|n| n.crs_wkt()))
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

        // `merge_parent` is what tells the two apart: `merge` is the only caller
        // that passes one, and it passes the source head it brought in.
        let event_type = match merge_parent {
            Some(_) => EventType::Merge,
            None => EventType::Commit,
        };
        queue_event(
            &mut tx,
            dataset_id,
            event_type,
            &serde_json::json!({
                "dataset_id": dataset_id,
                "branch_id": branch_id,
                "changeset_id": changeset_id,
                "parent_id": parent_id,
                "merge_parent_id": merge_parent,
                "message": message,
                "author": author,
                "operations": operations.len(),
            }),
        )
        .await?;

        tx.commit().await?;
        self.analyzer.after_write(operations.len());

        Ok(Changeset {
            id: changeset_id,
            branch_id,
            parent_id,
            merge_parent_id: merge_parent,
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
        .fetch_all(self.source_pool(external.as_ref()).await?)
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

    /// The pre-reprojection original of one live feature, or None when its
    /// current version has no distinct original. An external dataset is a view
    /// over someone else's table, nothing was reprojected on the way in, so
    /// every feature there answers None without a per-feature lookup.
    pub async fn native_geometry(
        &self,
        branch_id: Uuid,
        feature_id: Uuid,
    ) -> Result<Option<NativeGeometry>, StoreError> {
        if self.external_for_branch(branch_id).await?.is_some() {
            return Ok(None);
        }
        let prelude =
            latest_cte("fv.feature_id, fv.operation, fv.native_geometry, fv.native_crs_wkt");
        let row = sqlx::query(&format!(
            "{prelude}
            SELECT ST_AsBinary(native_geometry) as wkb, ST_SRID(native_geometry) as srid,
                   native_crs_wkt
            FROM latest
            WHERE feature_id = $2 AND operation != 'delete'"
        ))
        .bind(branch_id)
        .bind(feature_id)
        .fetch_optional(self.read_pool())
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!(
                "feature {feature_id} on branch {branch_id}"
            )));
        };
        Ok(native_from_row(&row, "wkb", "srid", "native_crs_wkt"))
    }

    /// One live feature on a branch, geometry and properties together.
    /// `list_features_paginated` is the alternative and scans the branch for one row.
    pub async fn feature_on_branch(
        &self,
        branch_id: Uuid,
        feature_id: Uuid,
    ) -> Result<Feature, StoreError> {
        let (external, prelude, source) = self.latest_source(branch_id).await?;
        let pool = self.source_pool(external.as_ref()).await?;
        let row = sqlx::query(&format!(
            "{prelude}
            SELECT feature_id, dataset_id, ST_AsBinary(geometry) as geometry_wkb,
                   properties, valid_from, valid_to
            FROM {source}
            WHERE feature_id = $2 AND operation != 'delete'"
        ))
        .bind(branch_id)
        .bind(feature_id)
        .fetch_optional(pool)
        .await?;
        let Some(row) = row else {
            return Err(StoreError::NotFound(format!(
                "feature {feature_id} on branch {branch_id}"
            )));
        };
        Ok(Feature {
            id: row.get("feature_id"),
            dataset_id: row.get("dataset_id"),
            geometry_wkb: row.get("geometry_wkb"),
            properties: row.get("properties"),
            valid_from: row.get("valid_from"),
            valid_to: row.get("valid_to"),
        })
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
            sqlx::query(&format!(
                "WITH RECURSIVE
                {to_chain},
                {from_chain},
                new_changesets AS (
                    SELECT id FROM to_chain EXCEPT SELECT id FROM from_chain
                )
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation,
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties,
                    fv.valid_from, fv.valid_to,
                    ST_AsBinary(fv.native_geometry) as native_wkb,
                    ST_SRID(fv.native_geometry) as native_srid,
                    fv.native_crs_wkt
                FROM feature_versions fv
                JOIN new_changesets nc ON fv.changeset_id = nc.id
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC",
                to_chain = ancestors_cte("to_chain", "$2"),
                from_chain = ancestors_cte("from_chain", "$1"),
            ))
            .bind(from_id)
            .bind(to_changeset)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(&format!(
                "WITH RECURSIVE {chain}
                SELECT DISTINCT ON (fv.feature_id)
                    fv.feature_id, fv.operation,
                    ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties,
                    fv.valid_from, fv.valid_to,
                    ST_AsBinary(fv.native_geometry) as native_wkb,
                    ST_SRID(fv.native_geometry) as native_srid,
                    fv.native_crs_wkt
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC",
                chain = ancestors_cte("chain", "$1"),
            ))
            .bind(to_changeset)
            .fetch_all(&self.pool)
            .await?
        };

        let operations = rows
            .into_iter()
            .map(|row| {
                let op: String = row.get("operation");
                let feature_id: Uuid = row.get("feature_id");
                // carried so replaying this diff (merge) keeps the original
                let native = native_from_row(&row, "native_wkb", "native_srid", "native_crs_wkt");
                match op.as_str() {
                    "insert" => DiffOp::Insert {
                        feature_id,
                        geometry_wkb: row.get("geometry_wkb"),
                        properties: row.get("properties"),
                        native,
                        valid_from: row.get("valid_from"),
                        valid_to: row.get("valid_to"),
                    },
                    "update" => DiffOp::Update {
                        feature_id,
                        geometry_wkb: Some(row.get("geometry_wkb")),
                        properties: Some(row.get("properties")),
                        native,
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
    ///
    /// The newest common ancestor rather than the shallowest: a commit is always
    /// younger than both its parents, so ordering by time picks a common
    /// ancestor no other common ancestor descends from, and it stays right where
    /// merge parents make the two sides different distances apart.
    pub async fn find_merge_base(
        &self,
        changeset_a: Uuid,
        changeset_b: Uuid,
    ) -> Result<Option<Uuid>, StoreError> {
        let row = sqlx::query(&format!(
            "WITH RECURSIVE
            {ancestors_a},
            {ancestors_b}
            SELECT c.id FROM ancestors_a a
            JOIN ancestors_b b ON a.id = b.id
            JOIN changesets c ON c.id = a.id
            ORDER BY c.created_at DESC, c.id DESC
            LIMIT 1",
            ancestors_a = ancestors_cte("ancestors_a", "$1"),
            ancestors_b = ancestors_cte("ancestors_b", "$2"),
        ))
        .bind(changeset_a)
        .bind(changeset_b)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.get("id")))
    }

    /// Whether `changeset` is `head` itself or one of its ancestors, merge
    /// parents included. This is what makes a merge already up to date.
    pub async fn is_reachable_from(&self, changeset: Uuid, head: Uuid) -> Result<bool, StoreError> {
        let reachable: bool = sqlx::query_scalar(&format!(
            "WITH RECURSIVE {ancestors}
             SELECT EXISTS (SELECT 1 FROM ancestors WHERE id = $2)",
            ancestors = ancestors_cte("ancestors", "$1"),
        ))
        .bind(head)
        .bind(changeset)
        .fetch_one(&self.pool)
        .await?;
        Ok(reachable)
    }

    /// What each of `feature_ids` held at `changeset`, absent where the feature
    /// did not exist there or was deleted. Feed it to [`merge_choice`] with the
    /// merge base as `changeset`, so a side that only carries the base forward
    /// is told apart from one that edited the feature.
    pub async fn contents_at(
        &self,
        changeset: Uuid,
        feature_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, VersionContent>, StoreError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS (
                SELECT id, parent_id FROM changesets WHERE id = $1
              UNION ALL
                SELECT c.id, c.parent_id FROM changesets c JOIN chain ch ON ch.parent_id = c.id
            )
            SELECT DISTINCT ON (fv.feature_id)
                fv.feature_id, fv.operation,
                ST_AsBinary(fv.geometry) as geometry_wkb, fv.properties,
                fv.valid_from, fv.valid_to,
                ST_AsBinary(fv.native_geometry) as native_wkb,
                ST_SRID(fv.native_geometry) as native_srid,
                fv.native_crs_wkt
            FROM feature_versions fv
            JOIN chain ch ON fv.changeset_id = ch.id
            WHERE fv.feature_id = ANY($2::uuid[])
            ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC",
        )
        .bind(changeset)
        .bind(feature_ids)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter(|row| row.get::<String, _>("operation") != "delete")
            .map(|row| {
                let content = VersionContent {
                    geometry_wkb: row.get("geometry_wkb"),
                    properties: row.get("properties"),
                    native: native_from_row(&row, "native_wkb", "native_srid", "native_crs_wkt"),
                    valid_from: row.get("valid_from"),
                    valid_to: row.get("valid_to"),
                };
                (row.get("feature_id"), content)
            })
            .collect())
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

        // Everything the source has is already on the target, so there is
        // nothing to merge and nothing to record.
        if self.is_reachable_from(source_head, target_head).await? {
            return Ok(MergeResult::AlreadyUpToDate);
        }

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

        // What the base held for those features. A side whose version still
        // matches it did not change the feature, however the diff reports it:
        // an earlier merge's own copy of the other branch's work reads as a
        // change against the fork point but is not one.
        let base_contents = match base {
            Some(base_id) => {
                let ids: Vec<Uuid> = all_features.iter().copied().collect();
                self.contents_at(base_id, &ids).await?
            }
            None => std::collections::HashMap::new(),
        };

        for fid in all_features {
            let choice = merge_choice(
                ours_map.get(&fid).copied(),
                theirs_map.get(&fid).copied(),
                base_contents.get(&fid),
            );
            match choice {
                MergeChoice::Nothing => {}
                MergeChoice::Apply(op) => merged_ops.push(op.clone()),
                MergeChoice::ApplyMerged(op) => merged_ops.push(op),
                MergeChoice::Conflict { ours, theirs } => conflicts.push(ConflictInfo {
                    feature_id: fid,
                    ours: ours.clone(),
                    theirs: theirs.clone(),
                }),
            }
        }

        if !conflicts.is_empty() {
            return Ok(MergeResult::Conflicts(conflicts));
        }

        // No conflicts — create merge commit on target branch
        let changeset = self
            .commit_merge(
                target_branch_id,
                &format!("Merge branch '{}' into '{}'", source.name, target.name),
                author,
                &merged_ops,
                Some(source_head),
                writer,
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
            SELECT id, branch_id, parent_id, merge_parent_id, message, author, created_at
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
                merge_parent_id: row.get("merge_parent_id"),
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
        let pool = self.source_pool(external.as_ref()).await?;
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
        .fetch_all(self.source_pool(external.as_ref()).await?)
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
        .fetch_all(self.source_pool(external.as_ref()).await?)
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
        .fetch_all(self.source_pool(external.as_ref()).await?)
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
        .fetch_all(self.source_pool(external.as_ref()).await?)
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
        .fetch_one(self.source_pool(external.as_ref()).await?)
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
        let zoom = i32::try_from(z).unwrap_or(i32::MAX);
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
                    ST_Simplify(ST_Transform(l.geometry, 3857), $5::double precision),
                    b.geom,
                    {MVT_TILE_EXTENT}, 64, true
                ) AS geom,
                l.feature_id,
                l.properties
                FROM latest l, bounds b
                WHERE l.operation != 'delete'
                  AND l.geometry IS NOT NULL
                  AND ST_Intersects(l.geometry, ST_Transform(b.geom, 4326))
            )
            SELECT COALESCE(ST_AsMVT(mvtgeom.*, 'features', {MVT_TILE_EXTENT}, 'geom'), ''::bytea) AS tile
            FROM mvtgeom
            WHERE geom IS NOT NULL"
        ))
        .bind(branch_id)
        .bind(zoom)
        .bind(x as i32)
        .bind(y as i32)
        .bind(mvt_simplify_tolerance(zoom))
        .fetch_one(self.source_pool(external.as_ref()).await?)
        .await?;

        Ok(row.get::<Vec<u8>, _>("tile"))
    }

    // ─── Merge Requests (Reviews) ───────────────────────────────────

    /// The review's target branch is taken from `grant`, not from `mr`, so the
    /// branch it would eventually land on is the one the ladder checked.
    pub async fn create_merge_request(
        &self,
        grant: &WriteGrant,
        mr: &MergeRequest,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO merge_requests (id, dataset_id, source_branch_id, target_branch_id, title, description, author, status, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(mr.id)
        .bind(mr.dataset_id)
        .bind(mr.source_branch_id)
        .bind(grant.id())
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

    // ─── Schema ─────────────────────────────────────────────────────

    pub async fn set_dataset_schema(&self, schema: &DatasetSchema) -> Result<(), StoreError> {
        let fields_json = serde_json::to_value(&schema.fields).unwrap();
        let rules_json = serde_json::to_value(&schema.geometry_rules).unwrap();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO dataset_schemas (dataset_id, fields, geometry_rules)
             VALUES ($1, $2, $3)
             ON CONFLICT (dataset_id) DO UPDATE SET fields = $2, geometry_rules = $3",
        )
        .bind(schema.dataset_id)
        .bind(&fields_json)
        .bind(&rules_json)
        .execute(&mut *tx)
        .await?;
        // field names only: a subscriber learns the shape changed without the
        // payload carrying the whole schema to an arbitrary url
        let field_names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
        queue_event(
            &mut tx,
            schema.dataset_id,
            EventType::SchemaChanged,
            &serde_json::json!({
                "dataset_id": schema.dataset_id,
                "fields": field_names,
            }),
        )
        .await?;
        tx.commit().await?;
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
                native: None,
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
    //
    // The writes are in `writes` behind a grant, and the events a change raises
    // are queued by `queue_event` inside the transaction that made the change.

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

    // ─── Feature Attachments ────────────────────────────────────────────
    //
    // A delete is a soft delete: it sets `deleted_at` and the row stays, so the
    // ArcGIS facade's change files can report what went. Every read here filters
    // `deleted_at IS NULL`, which is what makes a tombstone invisible to
    // everything but that one window query. The two id-resolution queries the
    // permission layer runs, `private_datasets_for_ids` and
    // `write_targets_for_id`, deliberately do not filter: a tombstoned
    // attachment still belongs to its dataset, and its visibility and its ladder
    // must not change because the row was deleted.

    pub async fn create_attachment(&self, attachment: &Attachment) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO attachments (id, feature_id, branch_id, dataset_id, project_id, name, content_type, size_bytes, data, thumbnail, metadata, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(attachment.id)
        .bind(attachment.feature_id)
        .bind(attachment.branch_id)
        .bind(attachment.dataset_id)
        .bind(attachment.project_id)
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
            "SELECT id, feature_id, branch_id, dataset_id, project_id, name, content_type, size_bytes, metadata, created_by, created_at
             FROM attachments
             WHERE feature_id = $1 AND branch_id = $2 AND deleted_at IS NULL
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
            "SELECT id, feature_id, branch_id, dataset_id, project_id, name, content_type, size_bytes, metadata, created_by, created_at
             FROM attachments
             WHERE dataset_id = $1 AND deleted_at IS NULL
             ORDER BY created_at DESC",
        )
        .bind(dataset_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(attachment_meta_from_row).collect())
    }

    /// A project's own attachments: the overlay bitmaps its map names, which
    /// belong to no dataset.
    pub async fn list_project_attachments(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<AttachmentMeta>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, feature_id, branch_id, dataset_id, project_id, name, content_type, size_bytes, metadata, created_by, created_at
             FROM attachments
             WHERE project_id = $1 AND deleted_at IS NULL
             ORDER BY created_at DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(attachment_meta_from_row).collect())
    }

    /// The same as [`PgStore::get_attachment`], refusing one that belongs to
    /// another project. The project-scoped routes check the caller's role
    /// against the project in the path, so an attachment reached through the
    /// wrong path would skip that check.
    pub async fn get_project_attachment(
        &self,
        project_id: Uuid,
        id: Uuid,
    ) -> Result<Attachment, StoreError> {
        let attachment = self.get_attachment(id).await?;
        if attachment.project_id != Some(project_id) {
            return Err(StoreError::NotFound(format!("attachment {id}")));
        }
        Ok(attachment)
    }

    pub async fn get_attachment(&self, id: Uuid) -> Result<Attachment, StoreError> {
        let row = sqlx::query(
            "SELECT id, feature_id, branch_id, dataset_id, project_id, name, content_type, size_bytes, data, thumbnail, metadata, created_by, created_at
             FROM attachments WHERE id = $1 AND deleted_at IS NULL",
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
            project_id: row.get("project_id"),
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

    /// The same soft delete, refusing an attachment that belongs to another
    /// project. Scoped in the statement rather than by reading the row first, so
    /// a megabyte bitmap is never loaded to answer who owns it.
    pub async fn delete_project_attachment(
        &self,
        project_id: Uuid,
        id: Uuid,
    ) -> Result<(), StoreError> {
        let deleted = sqlx::query(
            "UPDATE attachments SET deleted_at = now()
              WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(project_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if deleted == 0 {
            return Err(StoreError::NotFound(format!("attachment {id}")));
        }
        Ok(())
    }

    /// Soft delete: the row keeps its bytes and gains a `deleted_at`, so a change
    /// file can report it gone. Already tombstoned is left alone rather than
    /// re-stamped, so the time a delete is reported under is the time it happened.
    pub async fn delete_attachment(&self, id: Uuid) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE attachments SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
        )
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

    /// A live API key for this SHA-256 hex. Revoked and expired rows are a miss
    /// the same way an unknown hash is, so the auth layer can treat them as 401.
    pub async fn active_api_key(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyIdentity>, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, role FROM api_keys
             WHERE key_hash = $1
               AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > NOW())",
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| ApiKeyIdentity {
            id: row.get("id"),
            name: row.get("name"),
            role: row.get("role"),
        }))
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

    /// Revoking the dataset's last `admin` row is refused: it leaves nobody able
    /// to manage grants. Grant a replacement first. The rule binds instance
    /// admins too, so stepping down is always grant-then-revoke.
    ///
    /// Removing the last row of any other kind is allowed. It leaves the dataset
    /// with no rows, which now denies every enforced writer rather than opening
    /// it, so it is a tightening and an instance admin can still grant.
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

        let admins = rows.iter().filter(|(_, p)| p == "admin").count();
        let target_is_admin = rows.iter().any(|(u, p)| u == user_id && p == "admin");
        if target_is_admin && admins == 1 {
            return Err(StoreError::Forbidden(format!(
                "{user_id} is the only admin of dataset {dataset_id}: revoking it would leave \
                 nobody able to manage its permissions. Grant another admin first."
            )));
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
        // the same scope the write ladder reads, so a project member is not told
        // they cannot do what they can. [`PgStore::check_branch_permission`] does
        // the same through `write_scopes`.
        let scope = dataset_scope(&self.pool, dataset_id, user_id).await?;
        match scope.mine {
            Some(perm) => Ok(permission_level(&perm) >= permission_level(required)),
            None => Ok(false),
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

    /// Whether a user holds at least `required` on a branch.
    ///
    /// `write` and `admin` read the scopes the write ladder reads and pick
    /// between them the same way, so a branch that has rows of its own decides
    /// and a dataset grant does not reach into it. Answering otherwise would
    /// promise a write that [`PgStore::ensure_branch_writable`] then refuses.
    ///
    /// `read` is dataset visibility, which a grant anywhere on the dataset
    /// satisfies, so there the dataset scope still stands in.
    pub async fn check_branch_permission(
        &self,
        branch_id: Uuid,
        user_id: &str,
        required: &str,
    ) -> Result<bool, StoreError> {
        let dataset_id: Uuid = sqlx::query_scalar("SELECT dataset_id FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("branch {branch_id}")))?;

        let (branch, dataset) = write_scopes(&self.pool, branch_id, dataset_id, user_id).await?;
        let mine = match required {
            "read" => branch.mine.or(dataset.mine),
            _ if branch.enforced => branch.mine,
            _ => dataset.mine,
        };
        Ok(mine.as_deref().map(permission_level).unwrap_or(0) >= permission_level(required))
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

// ─── Merge types ────────────────────────────────────────────────────]
#[derive(Debug)]
pub enum MergeResult {
    Success(Changeset),
    Conflicts(Vec<ConflictInfo>),
    /// The source head was already on the target. No changeset was written.
    AlreadyUpToDate,
}

/// One feature version's content, for comparing a merge side against the base.
/// Read it with [`PgStore::contents_at`].
pub struct VersionContent {
    geometry_wkb: Option<Vec<u8>>,
    properties: serde_json::Value,
    native: Option<ptolemy_core::diff::NativeGeometry>,
    valid_from: Option<OffsetDateTime>,
    valid_to: Option<OffsetDateTime>,
}

impl VersionContent {
    /// For a conflict report, which shows what the base held alongside the two
    /// sides.
    pub fn properties(&self) -> &serde_json::Value {
        &self.properties
    }
}

/// What a three-way merge does with one feature.
#[derive(Debug, Clone)]
pub enum MergeChoice<'a> {
    /// Nothing to write: the target's own chain already holds this content.
    Nothing,
    /// Write this op on the target.
    Apply(&'a DiffOp),
    /// Write a newly combined op (disjoint attribute edits).
    ApplyMerged(DiffOp),
    /// Both sides moved the feature away from the base.
    Conflict {
        ours: &'a DiffOp,
        theirs: &'a DiffOp,
    },
}

/// The merge decision for one feature: each side's op as its diff from the base
/// reports it, `None` where that side did not touch the feature, and `at_base`
/// as the base held it.
///
/// The listing and preview routes decide with this too, so what they call a
/// conflict is what [`PgStore::merge`] calls one. A side whose version still
/// matches the base did not change the feature and so cannot conflict: that is
/// what an earlier merge's own copy of the other branch's work looks like.
pub fn merge_choice<'a>(
    ours: Option<&'a DiffOp>,
    theirs: Option<&'a DiffOp>,
    at_base: Option<&VersionContent>,
) -> MergeChoice<'a> {
    match (ours, theirs) {
        (Some(ours), None) if op_matches_content(ours, at_base) => MergeChoice::Nothing,
        (Some(ours), None) => MergeChoice::Apply(ours),
        (None, Some(theirs)) => MergeChoice::Apply(theirs),
        (Some(ours), Some(theirs)) => {
            if ops_equal(ours, theirs) || op_matches_content(theirs, at_base) {
                MergeChoice::Apply(ours)
            } else if op_matches_content(ours, at_base) {
                MergeChoice::Apply(theirs)
            } else if let Some(merged) = merge_disjoint_updates(ours, theirs, at_base) {
                MergeChoice::ApplyMerged(merged)
            } else {
                MergeChoice::Conflict { ours, theirs }
            }
        }
        (None, None) => MergeChoice::Nothing,
    }
}

/// Whether `op` leaves the feature exactly as `content` had it, `None` meaning
/// the feature was not there. A side that matches has nothing to contribute.
fn op_matches_content(op: &DiffOp, content: Option<&VersionContent>) -> bool {
    match (op, content) {
        (DiffOp::Delete { .. }, None) => true,
        (DiffOp::Delete { .. }, Some(_)) | (_, None) => false,
        (
            DiffOp::Insert {
                geometry_wkb,
                properties,
                native,
                valid_from,
                valid_to,
                ..
            },
            Some(base),
        ) => {
            Some(geometry_wkb) == base.geometry_wkb.as_ref()
                && *properties == base.properties
                && *native == base.native
                && *valid_from == base.valid_from
                && *valid_to == base.valid_to
        }
        (
            DiffOp::Update {
                geometry_wkb,
                properties,
                native,
                valid_from,
                valid_to,
                ..
            },
            Some(base),
        ) => {
            geometry_wkb.as_ref() == base.geometry_wkb.as_ref()
                && properties.as_ref() == Some(&base.properties)
                && *native == base.native
                && *valid_from == base.valid_from
                && *valid_to == base.valid_to
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub feature_id: Uuid,
    pub ours: DiffOp,
    pub theirs: DiffOp,
}

// ─── Attachment types ───────────────────────────────────────────────

/// Owned by a feature on a branch, a dataset, or a project, never more than one
/// and never none; the `attachments_one_owner` CHECK is the authority.
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub id: Uuid,
    pub feature_id: Option<Uuid>,
    pub branch_id: Option<Uuid>,
    pub dataset_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
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
    pub project_id: Option<Uuid>,
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

/// The fields auth needs from a live `api_keys` row.
#[derive(Debug, Clone)]
pub struct ApiKeyIdentity {
    pub id: Uuid,
    pub name: String,
    pub role: String,
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

/// Record an event and queue it to every active webhook that subscribes to its
/// type, inside the caller's transaction.
///
/// Being in the same transaction as the change is the point: an event exists
/// exactly when the commit it describes does, so no restart or failed commit can
/// leave a subscriber told about something that did not happen, or not told about
/// something that did.
///
/// A subscription with an empty `events` array takes every type. Returns the
/// event id.
async fn queue_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dataset_id: Uuid,
    event_type: EventType,
    payload: &serde_json::Value,
) -> Result<Uuid, StoreError> {
    queue_named_event(tx, dataset_id, &event_type.to_string(), payload).await
}

/// The same, for the one event type that is not one of ours: what the
/// caller-facing emit route was given. See [`PgStore::emit_event`].
pub(crate) async fn queue_named_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dataset_id: Uuid,
    name: &str,
    payload: &serde_json::Value,
) -> Result<Uuid, StoreError> {
    let event_id = Uuid::now_v7();
    sqlx::query("INSERT INTO events (id, dataset_id, event_type, payload) VALUES ($1, $2, $3, $4)")
        .bind(event_id)
        .bind(dataset_id)
        .bind(name)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO webhook_deliveries (webhook_id, event_id)
         SELECT w.id, $1 FROM webhooks w
          WHERE w.dataset_id = $2 AND w.active
            AND (cardinality(w.events) = 0 OR $3 = ANY(w.events))",
    )
    .bind(event_id)
    .bind(dataset_id)
    .bind(name)
    .execute(&mut **tx)
    .await?;
    Ok(event_id)
}

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

/// SQL for the caller's effective role on the project a dataset is attached to,
/// with the dataset named by a bind. No role when the dataset has no project.
fn project_role_of_dataset_sql(dataset_param: usize, caller_param: usize) -> String {
    effective_project_role_sql(
        &format!("(SELECT project_id FROM datasets WHERE id = ${dataset_param})"),
        caller_param,
    )
}

/// The branch and dataset permission scopes for one writer, in one round trip.
///
/// The dataset scope carries the stronger of the writer's explicit grant and the
/// permission their project role maps to. The branch scope is explicit rows only:
/// a project role is dataset-wide, and letting it reach into a branch would
/// override the rule that branch rows decide.
async fn write_scopes<'e, E>(
    exec: E,
    branch_id: Uuid,
    dataset_id: Uuid,
    user_id: &str,
) -> Result<(Scope, Scope), StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let project_role = project_role_of_dataset_sql(2, 3);
    let row = sqlx::query(&format!(
        "SELECT
            (SELECT count(*) FROM branch_permissions WHERE branch_id = $1) AS branch_total,
            (SELECT permission FROM branch_permissions
              WHERE branch_id = $1 AND user_id = $3) AS branch_mine,
            (SELECT count(*) FROM dataset_permissions WHERE dataset_id = $2) AS dataset_total,
            (SELECT permission FROM dataset_permissions
              WHERE dataset_id = $2 AND user_id = $3) AS dataset_mine,
            {project_role} AS project_role"
    ))
    .bind(branch_id)
    .bind(dataset_id)
    .bind(user_id)
    .fetch_one(exec)
    .await?;

    let project_role = parse_effective_role(row.get("project_role"))?;
    Ok((
        Scope {
            enforced: row.get::<i64, _>("branch_total") > 0,
            mine: row.get("branch_mine"),
        },
        Scope {
            enforced: row.get::<i64, _>("dataset_total") > 0,
            mine: stronger_permission(row.get("dataset_mine"), project_role),
        },
    ))
}

async fn dataset_scope<'e, E>(exec: E, dataset_id: Uuid, user_id: &str) -> Result<Scope, StoreError>
where
    E: sqlx::PgExecutor<'e>,
{
    let project_role = project_role_of_dataset_sql(1, 2);
    let row = sqlx::query(&format!(
        "SELECT
            (SELECT count(*) FROM dataset_permissions WHERE dataset_id = $1) AS total,
            (SELECT permission FROM dataset_permissions
              WHERE dataset_id = $1 AND user_id = $2) AS mine,
            {project_role} AS project_role"
    ))
    .bind(dataset_id)
    .bind(user_id)
    .fetch_one(exec)
    .await?;

    let project_role = parse_effective_role(row.get("project_role"))?;
    Ok(Scope {
        enforced: row.get::<i64, _>("total") > 0,
        mine: stronger_permission(row.get("mine"), project_role),
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

fn op_feature_id(op: &DiffOp) -> Uuid {
    match op {
        DiffOp::Insert { feature_id, .. }
        | DiffOp::Update { feature_id, .. }
        | DiffOp::Delete { feature_id } => *feature_id,
    }
}

/// Rebuild a version's original from its three stored parts. A WKT wins when
/// present (its geometry is stamped srid 0), a nonzero srid names a code, and
/// anything else is no distinct original.
fn native_from_row(
    row: &sqlx::postgres::PgRow,
    wkb_col: &str,
    srid_col: &str,
    wkt_col: &str,
) -> Option<NativeGeometry> {
    let wkb: Option<Vec<u8>> = row.get(wkb_col);
    let wkb = wkb?;
    match (
        row.get::<Option<String>, _>(wkt_col),
        row.get::<Option<i32>, _>(srid_col),
    ) {
        (Some(wkt), _) => NativeGeometry::wkt(wkb, wkt),
        (None, Some(srid)) if srid != 0 => NativeGeometry::epsg(wkb, srid),
        _ => None,
    }
}

/// Combine two Updates that touched different attributes, or None if they
/// both changed the same field, the geometry, or something else that cannot
/// be auto-merged.
fn merge_disjoint_updates(
    ours: &DiffOp,
    theirs: &DiffOp,
    at_base: Option<&VersionContent>,
) -> Option<DiffOp> {
    let (
        DiffOp::Update {
            feature_id,
            geometry_wkb: ours_g,
            properties: ours_p,
            native: ours_n,
            valid_from: ours_vf,
            valid_to: ours_vt,
        },
        DiffOp::Update {
            geometry_wkb: theirs_g,
            properties: theirs_p,
            native: theirs_n,
            valid_from: theirs_vf,
            valid_to: theirs_vt,
            ..
        },
    ) = (ours, theirs)
    else {
        return None;
    };

    let base_g = at_base.and_then(|b| b.geometry_wkb.as_ref());
    let geometry_wkb = pick_side(ours_g.as_ref(), theirs_g.as_ref(), base_g)?.cloned();
    let native = pick_side(
        ours_n.as_ref(),
        theirs_n.as_ref(),
        at_base.and_then(|b| b.native.as_ref()),
    )?
    .cloned();
    let valid_from = pick_side(
        ours_vf.as_ref(),
        theirs_vf.as_ref(),
        at_base.and_then(|b| b.valid_from.as_ref()),
    )?
    .copied();
    let valid_to = pick_side(
        ours_vt.as_ref(),
        theirs_vt.as_ref(),
        at_base.and_then(|b| b.valid_to.as_ref()),
    )?
    .copied();

    let properties = merge_properties(
        ours_p.as_ref(),
        theirs_p.as_ref(),
        at_base.map(|b| &b.properties),
    )?;

    Some(DiffOp::Update {
        feature_id: *feature_id,
        geometry_wkb,
        properties: Some(properties),
        native,
        valid_from,
        valid_to,
    })
}

/// Prefer the side that moved away from `base`. Both moving to different
/// values is a conflict.
fn pick_side<'a, T: PartialEq>(
    ours: Option<&'a T>,
    theirs: Option<&'a T>,
    base: Option<&T>,
) -> Option<Option<&'a T>> {
    match (ours, theirs) {
        (a, b) if a == b => Some(a),
        (a, b) if a == base => Some(b),
        (a, b) if b == base => Some(a),
        _ => None,
    }
}

fn merge_properties(
    ours: Option<&serde_json::Value>,
    theirs: Option<&serde_json::Value>,
    base: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let ours_obj = ours.and_then(|v| v.as_object());
    let theirs_obj = theirs.and_then(|v| v.as_object());
    let base_obj = base.and_then(|v| v.as_object());
    match (ours_obj, theirs_obj) {
        (None, None) => Some(base.cloned().unwrap_or(serde_json::json!({}))),
        (Some(a), None) => Some(serde_json::Value::Object(a.clone())),
        (None, Some(b)) => Some(serde_json::Value::Object(b.clone())),
        (Some(a), Some(b)) => {
            let mut keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            if let Some(base_obj) = base_obj {
                keys.extend(base_obj.keys());
            }
            let mut out = serde_json::Map::new();
            for key in keys {
                let ov = a.get(key);
                let tv = b.get(key);
                let bv = base_obj.and_then(|o| o.get(key));
                let chosen = match (ov, tv) {
                    (a, b) if a == b => ov.or(tv),
                    (_, b) if ov == bv => b,
                    (a, _) if tv == bv => a,
                    _ => return None,
                };
                if let Some(v) = chosen {
                    out.insert(key.clone(), v.clone());
                }
            }
            Some(serde_json::Value::Object(out))
        }
    }
}

fn ops_equal(a: &DiffOp, b: &DiffOp) -> bool {
    match (a, b) {
        (
            DiffOp::Insert {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
                native: na,
                valid_from: vfa,
                valid_to: vta,
            },
            DiffOp::Insert {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
                native: nb,
                valid_from: vfb,
                valid_to: vtb,
            },
        ) => fa == fb && ga == gb && pa == pb && na == nb && vfa == vfb && vta == vtb,
        (
            DiffOp::Update {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
                native: na,
                valid_from: vfa,
                valid_to: vta,
            },
            DiffOp::Update {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
                native: nb,
                valid_from: vfb,
                valid_to: vtb,
            },
        ) => fa == fb && ga == gb && pa == pb && na == nb && vfa == vfb && vta == vtb,
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
        project_id: row.get("project_id"),
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
        project_id: row.get("project_id"),
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

#[cfg(test)]
mod merge_attribute_tests {
    use super::*;
    use ptolemy_core::diff::DiffOp;
    use serde_json::json;
    use uuid::Uuid;

    fn upd(props: serde_json::Value) -> DiffOp {
        DiffOp::Update {
            feature_id: Uuid::nil(),
            geometry_wkb: None,
            properties: Some(props),
            native: None,
            valid_from: None,
            valid_to: None,
        }
    }

    #[test]
    fn disjoint_keys_merge() {
        let base = VersionContent {
            geometry_wkb: None,
            properties: json!({"name": "Park", "capacity": 100}),
            native: None,
            valid_from: None,
            valid_to: None,
        };
        let ours = upd(json!({"name": "Central Park", "capacity": 100}));
        let theirs = upd(json!({"name": "Park", "capacity": 250}));
        let merged = merge_disjoint_updates(&ours, &theirs, Some(&base)).unwrap();
        let DiffOp::Update { properties, .. } = merged else {
            panic!("expected update");
        };
        let props = properties.unwrap();
        assert_eq!(props["name"], "Central Park");
        assert_eq!(props["capacity"], 250);
    }

    #[test]
    fn same_key_conflict() {
        let base = VersionContent {
            geometry_wkb: None,
            properties: json!({"name": "Park"}),
            native: None,
            valid_from: None,
            valid_to: None,
        };
        let ours = upd(json!({"name": "A"}));
        let theirs = upd(json!({"name": "B"}));
        assert!(merge_disjoint_updates(&ours, &theirs, Some(&base)).is_none());
    }
}

#[cfg(test)]
mod mvt_tolerance_tests {
    use super::*;

    /// One tile unit in metres at this zoom, the grid ST_AsMVTGeom rounds onto.
    fn tile_unit_metres(z: i32) -> f64 {
        WEB_MERCATOR_SPAN_METRES / (f64::from(MVT_TILE_EXTENT) * 2f64.powi(z))
    }

    #[test]
    fn the_tolerance_is_half_a_tile_unit_at_every_zoom() {
        for z in 0..=22 {
            let expected = tile_unit_metres(z) / 2.0;
            let actual = mvt_simplify_tolerance(z);
            assert!(
                (actual - expected).abs() < f64::EPSILON * expected.max(1.0),
                "zoom {z}: {actual} is not half a tile unit {expected}"
            );
            assert!(
                actual < tile_unit_metres(z),
                "zoom {z} would move a vertex a whole cell"
            );
        }
    }

    #[test]
    fn each_zoom_halves_the_tolerance_of_the_one_above_it() {
        for z in 0..22 {
            let coarser = mvt_simplify_tolerance(z);
            let finer = mvt_simplify_tolerance(z + 1);
            assert!((coarser - finer * 2.0).abs() < f64::EPSILON * coarser);
        }
    }

    /// The whole point is that a tile at low zoom drops detail a viewer could
    /// never see, so the tolerance has to be a real distance, not a rounding
    /// error, until the zoom is deep enough for it not to be.
    #[test]
    fn the_tolerance_spans_metres_when_zoomed_out_and_centimetres_when_zoomed_in() {
        assert!(mvt_simplify_tolerance(0) > 1000.0);
        assert!(mvt_simplify_tolerance(10) > 1.0);
        assert!(mvt_simplify_tolerance(18) < 0.1);
    }
}
