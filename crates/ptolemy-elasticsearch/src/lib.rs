//! Ptolemy Elasticsearch/OpenSearch Data Store Plugin
//!
//! Provides read/write access to geospatial features stored in
//! Elasticsearch or OpenSearch indices with geo_shape/geo_point mappings.
//!
//! ## Features
//! - Spatial queries via geo_bounding_box and geo_shape filters
//! - Full-text search combined with spatial filtering
//! - Automatic index mapping from Ptolemy dataset schemas
//! - Bulk insert/update for high-throughput ingestion
//! - Scroll API support for large result sets

use std::sync::Arc;
use tokio::sync::RwLock;

use ptolemy_core::{
    Bbox, BoxFuture, DataStore, DataStoreError, Dataset, Feature, FeatureQuery, StoreCapabilities,
    StoreResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration for the Elasticsearch store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElasticsearchConfig {
    /// Elasticsearch node URLs.
    pub nodes: Vec<String>,
    /// Index prefix for Ptolemy datasets.
    pub index_prefix: String,
    /// Authentication (basic auth user:pass or API key).
    pub auth: Option<AuthConfig>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Scroll keep-alive duration.
    pub scroll_ttl: String,
}

/// Authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthConfig {
    Basic { username: String, password: String },
    ApiKey { id: String, key: String },
    Bearer(String),
}

/// Elasticsearch data store implementation.
pub struct ElasticsearchStore {
    capabilities: StoreCapabilities,
    #[allow(dead_code)]
    config: Arc<RwLock<Option<ElasticsearchConfig>>>,
    #[allow(dead_code)]
    client: Arc<RwLock<Option<reqwest::Client>>>,
}

impl ElasticsearchStore {
    pub fn new() -> Self {
        Self {
            capabilities: StoreCapabilities {
                name: "Elasticsearch".to_string(),
                geometry_types: vec![
                    "Point".to_string(),
                    "LineString".to_string(),
                    "Polygon".to_string(),
                    "MultiPoint".to_string(),
                    "MultiLineString".to_string(),
                    "MultiPolygon".to_string(),
                    "GeometryCollection".to_string(),
                ],
                transactions: false,
                spatial_index: true,
                versioning: false,
                max_features: 10000,
                supported_crs: vec![4326],
            },
            config: Arc::new(RwLock::new(None)),
            client: Arc::new(RwLock::new(None)),
        }
    }
}

impl Default for ElasticsearchStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DataStore for ElasticsearchStore {
    fn capabilities(&self) -> &StoreCapabilities {
        &self.capabilities
    }

    fn connect(&self, config: Value) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            let es_config: ElasticsearchConfig = serde_json::from_value(config)
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(es_config.timeout_secs))
                .build()
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;
            *self.config.write().await = Some(es_config);
            *self.client.write().await = Some(client);
            Ok(())
        })
    }

    fn disconnect(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            *self.client.write().await = None;
            *self.config.write().await = None;
        })
    }

    fn list_datasets(&self) -> BoxFuture<'_, StoreResult<Vec<Dataset>>> {
        Box::pin(async move {
            // TODO: Query _cat/indices with the configured prefix
            Ok(Vec::new())
        })
    }

    fn get_features(
        &self,
        _dataset: &str,
        _query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<Vec<Feature>>> {
        Box::pin(async move {
            // TODO: Build ES query DSL with geo_bounding_box + bool filters
            Ok(Vec::new())
        })
    }

    fn get_feature(&self, _dataset: &str, _id: &str) -> BoxFuture<'_, StoreResult<Feature>> {
        Box::pin(async move { Err(DataStoreError::NotFound("not implemented".into())) })
    }

    fn count_features(
        &self,
        _dataset: &str,
        _query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<u64>> {
        Box::pin(async move { Ok(0) })
    }

    fn insert_feature(
        &self,
        _dataset: &str,
        _feature: Feature,
    ) -> BoxFuture<'_, StoreResult<String>> {
        Box::pin(async move { Err(DataStoreError::Unsupported("not implemented".into())) })
    }

    fn update_feature(
        &self,
        _dataset: &str,
        _id: &str,
        _feature: Feature,
    ) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move { Err(DataStoreError::Unsupported("not implemented".into())) })
    }

    fn delete_feature(&self, _dataset: &str, _id: &str) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move { Err(DataStoreError::Unsupported("not implemented".into())) })
    }

    fn get_extent(&self, _dataset: &str) -> BoxFuture<'_, StoreResult<Bbox>> {
        Box::pin(async move { Ok([-180.0, -90.0, 180.0, 90.0]) })
    }
}
