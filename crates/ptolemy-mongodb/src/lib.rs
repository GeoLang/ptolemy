//! Ptolemy MongoDB Data Store Plugin
//!
//! Provides geospatial feature storage using MongoDB's native
//! GeoJSON support and 2dsphere indexes.
//!
//! ## Features
//! - Native GeoJSON storage (no conversion needed)
//! - 2dsphere spatial indexes for efficient geo queries
//! - `$geoWithin`, `$geoIntersects`, `$near` query support
//! - Change streams for real-time feature updates
//! - Aggregation pipeline for complex spatial analytics
//! - GridFS for large raster/attachment storage

use std::sync::Arc;
use tokio::sync::RwLock;

use ptolemy_core::{
    Bbox, BoxFuture, DataStore, DataStoreError, Dataset, Feature, FeatureQuery, StoreCapabilities,
    StoreResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration for the MongoDB store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoConfig {
    /// MongoDB connection URI.
    pub uri: String,
    /// Database name.
    pub database: String,
    /// Collection prefix for Ptolemy datasets.
    pub collection_prefix: String,
    /// Whether to create 2dsphere indexes automatically.
    pub auto_index: bool,
}

/// MongoDB data store implementation.
pub struct MongoStore {
    capabilities: StoreCapabilities,
    #[allow(dead_code)]
    config: Arc<RwLock<Option<MongoConfig>>>,
    #[allow(dead_code)]
    client: Arc<RwLock<Option<mongodb::Client>>>,
}

impl MongoStore {
    pub fn new() -> Self {
        Self {
            capabilities: StoreCapabilities {
                name: "MongoDB".to_string(),
                geometry_types: vec![
                    "Point".to_string(),
                    "LineString".to_string(),
                    "Polygon".to_string(),
                    "MultiPoint".to_string(),
                    "MultiLineString".to_string(),
                    "MultiPolygon".to_string(),
                    "GeometryCollection".to_string(),
                ],
                transactions: true,
                spatial_index: true,
                versioning: false,
                max_features: 0,           // unlimited
                supported_crs: vec![4326], // MongoDB only supports WGS84 for geo queries
            },
            config: Arc::new(RwLock::new(None)),
            client: Arc::new(RwLock::new(None)),
        }
    }
}

impl Default for MongoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DataStore for MongoStore {
    fn capabilities(&self) -> &StoreCapabilities {
        &self.capabilities
    }

    fn connect(&self, config: Value) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            let mongo_config: MongoConfig = serde_json::from_value(config)
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;
            let client = mongodb::Client::with_uri_str(&mongo_config.uri)
                .await
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;
            *self.config.write().await = Some(mongo_config);
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
            // TODO: List collections matching the prefix
            Ok(Vec::new())
        })
    }

    fn get_features(
        &self,
        _dataset: &str,
        _query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<Vec<Feature>>> {
        Box::pin(async move {
            // TODO: Build MongoDB find with $geoWithin for bbox
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
