// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! gRPC service for high-throughput bulk operations.
//!
//! Provides a tonic-based gRPC server for operations that benefit from
//! binary protocol efficiency (bulk feature import/export, streaming).

use prost::Message;
use std::sync::Arc;
use ptolemy_storage::PgStore;

/// gRPC service state.
pub struct GrpcService {
    pub store: Arc<PgStore>,
}

/// Feature message for bulk transfer (proto-like struct using prost derive).
#[derive(Clone, Message)]
pub struct FeatureMessage {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(bytes = "vec", tag = "2")]
    pub geometry_wkb: Vec<u8>,
    #[prost(string, tag = "3")]
    pub properties_json: String,
}

/// Bulk import request.
#[derive(Clone, Message)]
pub struct BulkImportRequest {
    #[prost(string, tag = "1")]
    pub branch_id: String,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(string, tag = "3")]
    pub author: String,
    #[prost(message, repeated, tag = "4")]
    pub features: Vec<FeatureMessage>,
}

/// Bulk import response.
#[derive(Clone, Message)]
pub struct BulkImportResponse {
    #[prost(string, tag = "1")]
    pub changeset_id: String,
    #[prost(uint64, tag = "2")]
    pub features_imported: u64,
}

/// Bulk export request.
#[derive(Clone, Message)]
pub struct BulkExportRequest {
    #[prost(string, tag = "1")]
    pub branch_id: String,
    #[prost(uint64, tag = "2")]
    pub limit: u64,
    #[prost(uint64, tag = "3")]
    pub offset: u64,
}

/// Bulk export response.
#[derive(Clone, Message)]
pub struct BulkExportResponse {
    #[prost(message, repeated, tag = "1")]
    pub features: Vec<FeatureMessage>,
    #[prost(uint64, tag = "2")]
    pub total: u64,
}

impl GrpcService {
    pub fn new(store: Arc<PgStore>) -> Self {
        Self { store }
    }

    /// Handle bulk import via binary protocol.
    pub async fn bulk_import(
        &self,
        request: BulkImportRequest,
    ) -> Result<BulkImportResponse, String> {
        use ptolemy_core::diff::DiffOp;
        use uuid::Uuid;

        let branch_id: Uuid = request
            .branch_id
            .parse()
            .map_err(|e| format!("invalid branch_id: {e}"))?;

        let ops: Vec<DiffOp> = request
            .features
            .into_iter()
            .map(|f| {
                let fid = f.id.parse::<Uuid>().unwrap_or_else(|_| Uuid::now_v7());
                DiffOp::Insert {
                    feature_id: fid,
                    geometry_wkb: f.geometry_wkb,
                    properties: serde_json::from_str(&f.properties_json)
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                }
            })
            .collect();

        let count = ops.len() as u64;
        let changeset = self
            .store
            .commit(branch_id, &request.message, &request.author, &ops)
            .await
            .map_err(|e| format!("commit failed: {e}"))?;

        Ok(BulkImportResponse {
            changeset_id: changeset.id.to_string(),
            features_imported: count,
        })
    }

    /// Handle bulk export via binary protocol.
    pub async fn bulk_export(
        &self,
        request: BulkExportRequest,
    ) -> Result<BulkExportResponse, String> {
        use sqlx::Row;
        use uuid::Uuid;

        let branch_id: Uuid = request
            .branch_id
            .parse()
            .map_err(|e| format!("invalid branch_id: {e}"))?;

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
                    fv.feature_id, fv.operation,
                    ST_AsBinary(fv.geometry) as geom,
                    fv.properties
                FROM feature_versions fv
                JOIN chain ch ON fv.changeset_id = ch.id
                ORDER BY fv.feature_id, fv.created_at DESC
            )
            SELECT feature_id, geom, properties
            FROM latest
            WHERE operation != 'delete'
            LIMIT $2 OFFSET $3",
        )
        .bind(branch_id)
        .bind(request.limit as i64)
        .bind(request.offset as i64)
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| format!("query failed: {e}"))?;

        let features: Vec<FeatureMessage> = rows
            .into_iter()
            .map(|row| {
                let fid: Uuid = row.get("feature_id");
                let geom: Option<Vec<u8>> = row.get("geom");
                let props: serde_json::Value = row.get("properties");
                FeatureMessage {
                    id: fid.to_string(),
                    geometry_wkb: geom.unwrap_or_default(),
                    properties_json: props.to_string(),
                }
            })
            .collect();

        let total = features.len() as u64;
        Ok(BulkExportResponse { features, total })
    }
}
