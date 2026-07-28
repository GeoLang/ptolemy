// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Planner statistics refresh after a bulk write.
//!
//! Every branch read walks the changeset ancestor chain with a recursive CTE
//! (see `latest_cte` in [`crate::postgres`]). Its plan is chosen from the
//! statistics postgres holds for `changesets`, `feature_versions` and
//! `branches`. Right after an import those statistics still describe an empty
//! or tiny database, so the planner picks nested loops over what is now a large
//! table and each read costs tens of milliseconds instead of about one, until
//! autoanalyze catches up minutes later. A write that inserts enough rows
//! schedules a targeted ANALYZE to close that window.

use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Semaphore;

/// Row count at or above which a write refreshes planner statistics. `0`
/// disables the refresh and leaves it to autoanalyze.
pub const ANALYZE_ROW_THRESHOLD: &str = "PTOLEMY_ANALYZE_ROW_THRESHOLD";

pub const DEFAULT_ANALYZE_ROW_THRESHOLD: usize = 1000;

/// Exactly the tables the branch read path walks. `branches` is small but it is
/// the CTE's anchor, so its plan matters as much as the other two.
const ANALYZE_SQL: &str = "ANALYZE feature_versions, changesets, branches";

/// Schedules the post-write ANALYZE. Cloned pool rather than a borrow so the
/// work outlives the request that triggered it.
#[derive(Clone)]
pub struct Analyzer {
    pool: PgPool,
    threshold: usize,
    /// One permit: holding it means an ANALYZE is running, so concurrent bulk
    /// writes collapse into the one already in flight instead of queueing.
    slot: Arc<Semaphore>,
    scheduled: Arc<AtomicU64>,
}

impl Analyzer {
    pub fn new(pool: PgPool, threshold: usize) -> Self {
        Self {
            pool,
            threshold,
            slot: Arc::new(Semaphore::new(1)),
            scheduled: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn threshold_from_env() -> usize {
        Self::resolve_threshold(std::env::var(ANALYZE_ROW_THRESHOLD).ok().as_deref())
    }

    /// A garbage value falls back to the default: stale statistics are a
    /// performance problem, not a reason to refuse to start.
    pub fn resolve_threshold(raw: Option<&str>) -> usize {
        match raw.map(str::trim) {
            None | Some("") => DEFAULT_ANALYZE_ROW_THRESHOLD,
            Some(v) => v.parse().unwrap_or_else(|_| {
                tracing::warn!(
                    value = v,
                    "{ANALYZE_ROW_THRESHOLD} is not a row count, using the default"
                );
                DEFAULT_ANALYZE_ROW_THRESHOLD
            }),
        }
    }

    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// How many ANALYZEs this process has started. Incremented before the task
    /// is spawned, so a caller can assert on it the moment the write returns.
    pub fn scheduled(&self) -> u64 {
        self.scheduled.load(Ordering::Relaxed)
    }

    /// Call once the write has committed, with the number of rows it inserted
    /// or removed. Never blocks and never fails: a write that succeeded stays
    /// succeeded whatever the database says about ANALYZE.
    pub fn after_write(&self, rows: usize) {
        if self.threshold == 0 || rows < self.threshold {
            return;
        }
        let Ok(slot) = self.slot.clone().try_acquire_owned() else {
            return;
        };
        self.scheduled.fetch_add(1, Ordering::Relaxed);
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let _slot = slot;
            match sqlx::query(ANALYZE_SQL).execute(&pool).await {
                Ok(_) => tracing::debug!(rows, "refreshed planner statistics after bulk write"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "post-write ANALYZE failed, reads keep stale planner statistics until autoanalyze"
                ),
            }
        });
    }

    /// Resolves once no ANALYZE is running. For callers that need to observe
    /// the effect, such as tests and shutdown.
    pub async fn wait_idle(&self) {
        let _ = self.slot.acquire().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that never connects: `after_write` must decide and return without
    /// touching the database.
    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
            .unwrap()
    }

    #[test]
    fn threshold_defaults_when_unset_or_junk() {
        assert_eq!(
            Analyzer::resolve_threshold(None),
            DEFAULT_ANALYZE_ROW_THRESHOLD
        );
        assert_eq!(
            Analyzer::resolve_threshold(Some("  ")),
            DEFAULT_ANALYZE_ROW_THRESHOLD
        );
        assert_eq!(
            Analyzer::resolve_threshold(Some("many")),
            DEFAULT_ANALYZE_ROW_THRESHOLD
        );
        assert_eq!(Analyzer::resolve_threshold(Some(" 50 ")), 50);
        assert_eq!(Analyzer::resolve_threshold(Some("0")), 0);
    }

    #[tokio::test]
    async fn schedules_only_at_or_above_the_threshold() {
        let a = Analyzer::new(lazy_pool(), 10);
        a.after_write(9);
        assert_eq!(a.scheduled(), 0);
        a.after_write(10);
        assert_eq!(a.scheduled(), 1);
    }

    #[tokio::test]
    async fn zero_threshold_disables_it() {
        let a = Analyzer::new(lazy_pool(), 0);
        a.after_write(usize::MAX);
        assert_eq!(a.scheduled(), 0);
    }

    /// Both calls happen before the runtime can poll the spawned task, so the
    /// second one sees the slot taken.
    #[tokio::test]
    async fn concurrent_bulk_writes_share_one_analyze() {
        let a = Analyzer::new(lazy_pool(), 1);
        a.after_write(5);
        a.after_write(5);
        assert_eq!(a.scheduled(), 1);
    }

    #[tokio::test]
    async fn a_failing_analyze_releases_the_slot() {
        let a = Analyzer::new(lazy_pool(), 1);
        a.after_write(5);
        a.wait_idle().await;
        a.after_write(5);
        assert_eq!(a.scheduled(), 2);
    }
}
