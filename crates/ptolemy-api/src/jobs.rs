// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Simple background job scheduler for periodic tasks.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info};

use ptolemy_storage::PgStore;

/// A scheduled background job.
pub struct BackgroundJobs {
    shutdown_tx: watch::Sender<bool>,
}

impl BackgroundJobs {
    /// Spawn all background jobs. Returns a handle to stop them.
    pub fn spawn(store: Arc<PgStore>) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Job 1: Clean expired feature locks every 5 minutes
        {
            let store = store.clone();
            let mut rx = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(300));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = cleanup_expired_locks(&store).await {
                                error!("Lock cleanup failed: {e}");
                            }
                        }
                        _ = rx.changed() => break,
                    }
                }
                info!("Lock cleanup job stopped");
            });
        }

        // Job 2: Quality check alerts every 15 minutes
        {
            let store = store.clone();
            let mut rx = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(900));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = quality_check_alert(&store).await {
                                error!("Quality check alert failed: {e}");
                            }
                        }
                        _ = rx.changed() => break,
                    }
                }
                info!("Quality check job stopped");
            });
        }

        // Job 3: Clean old events (retain 30 days) daily
        {
            let store = store.clone();
            let mut rx = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(86400));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = cleanup_old_events(&store).await {
                                error!("Event cleanup failed: {e}");
                            }
                        }
                        _ = rx.changed() => break,
                    }
                }
                info!("Event cleanup job stopped");
            });
        }

        Self { shutdown_tx }
    }

    /// Stop all background jobs.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

async fn cleanup_expired_locks(store: &PgStore) -> Result<(), sqlx::Error> {
    let result = sqlx::query("DELETE FROM feature_locks WHERE expires_at < now()")
        .execute(store.pool())
        .await?;
    let count = result.rows_affected();
    if count > 0 {
        info!(count, "Cleaned up expired feature locks");
    }
    Ok(())
}

async fn quality_check_alert(store: &PgStore) -> Result<(), sqlx::Error> {
    // Check for branches with >5% invalid geometries
    let rows = sqlx::query(
        "SELECT b.id, b.name, b.dataset_id,
            (SELECT COUNT(*) FROM feature_versions fv
             JOIN changesets c ON c.id = fv.changeset_id
             WHERE c.branch_id = b.id AND fv.geometry IS NOT NULL
               AND NOT ST_IsValid(fv.geometry)) as invalid_count
         FROM branches b
         HAVING (SELECT COUNT(*) FROM feature_versions fv
                 JOIN changesets c ON c.id = fv.changeset_id
                 WHERE c.branch_id = b.id AND fv.geometry IS NOT NULL
                   AND NOT ST_IsValid(fv.geometry)) > 0",
    )
    .fetch_all(store.pool())
    .await?;

    for row in rows {
        use sqlx::Row;
        let branch_name: String = row.get("name");
        let invalid: i64 = row.get("invalid_count");
        info!(branch = %branch_name, invalid_geometries = invalid, "Quality alert: branch has invalid geometries");
    }
    Ok(())
}

async fn cleanup_old_events(store: &PgStore) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM events WHERE created_at < now() - interval '30 days'",
    )
    .execute(store.pool())
    .await?;
    let count = result.rows_affected();
    if count > 0 {
        info!(count, "Cleaned up old events");
    }
    Ok(())
}
