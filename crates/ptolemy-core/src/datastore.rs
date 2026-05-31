//! Plugin system for Ptolemy data store backends.
//!
//! Ptolemy's storage layer is pluggable — each backend implements the
//! `DataStore` trait to provide feature CRUD, spatial queries, and schema
//! management against different storage engines.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::{Dataset, Feature};
use serde_json::Value;

/// Result type for data store operations.
pub type StoreResult<T> = Result<T, DataStoreError>;

/// Errors from data store plugins.
#[derive(Debug, thiserror::Error)]
pub enum DataStoreError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("feature not found: {0}")]
    NotFound(String),
    #[error("schema mismatch: {0}")]
    SchemaMismatch(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Bounding box for spatial queries [west, south, east, north].
pub type Bbox = [f64; 4];

/// Boxed async future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Query parameters for feature retrieval.
#[derive(Debug, Clone, Default)]
pub struct FeatureQuery {
    /// Spatial bounding box filter.
    pub bbox: Option<Bbox>,
    /// CQL/attribute filter expression.
    pub filter: Option<String>,
    /// Maximum features to return.
    pub limit: Option<u32>,
    /// Offset for pagination.
    pub offset: Option<u32>,
    /// Property names to include (empty = all).
    pub properties: Vec<String>,
    /// Sort by field.
    pub sort_by: Option<String>,
    /// Sort ascending.
    pub sort_asc: bool,
    /// SRID for the spatial filter.
    pub srid: u32,
}

/// Metadata about a data store backend's capabilities.
#[derive(Debug, Clone)]
pub struct StoreCapabilities {
    /// Human-readable name.
    pub name: String,
    /// Supported geometry types.
    pub geometry_types: Vec<String>,
    /// Whether the store supports transactions.
    pub transactions: bool,
    /// Whether the store supports spatial indexing.
    pub spatial_index: bool,
    /// Whether the store supports versioning/branching natively.
    pub versioning: bool,
    /// Maximum features per query (0 = unlimited).
    pub max_features: u32,
    /// Supported CRS codes.
    pub supported_crs: Vec<u32>,
}

/// Core trait for pluggable data store backends.
///
/// Each implementation provides feature storage and retrieval
/// against a specific backend (PostgreSQL, Elasticsearch, MongoDB,
/// GeoPackage, Parquet, etc.).
pub trait DataStore: Send + Sync + 'static {
    /// Returns the store's capabilities and metadata.
    fn capabilities(&self) -> &StoreCapabilities;

    /// Initialize/connect to the data store with the given config.
    fn connect(&self, config: Value) -> BoxFuture<'_, StoreResult<()>>;

    /// Disconnect and clean up resources.
    fn disconnect(&self) -> BoxFuture<'_, ()>;

    /// List available datasets/collections in this store.
    fn list_datasets(&self) -> BoxFuture<'_, StoreResult<Vec<Dataset>>>;

    /// Get features matching a query.
    fn get_features(
        &self,
        dataset: &str,
        query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<Vec<Feature>>>;

    /// Get a single feature by ID.
    fn get_feature(&self, dataset: &str, id: &str) -> BoxFuture<'_, StoreResult<Feature>>;

    /// Count features matching a query.
    fn count_features(&self, dataset: &str, query: FeatureQuery)
    -> BoxFuture<'_, StoreResult<u64>>;

    /// Insert a new feature. Returns the assigned ID.
    fn insert_feature(&self, dataset: &str, feature: Feature)
    -> BoxFuture<'_, StoreResult<String>>;

    /// Update an existing feature.
    fn update_feature(
        &self,
        dataset: &str,
        id: &str,
        feature: Feature,
    ) -> BoxFuture<'_, StoreResult<()>>;

    /// Delete a feature by ID.
    fn delete_feature(&self, dataset: &str, id: &str) -> BoxFuture<'_, StoreResult<()>>;

    /// Get the bounding box of all features in a dataset.
    fn get_extent(&self, dataset: &str) -> BoxFuture<'_, StoreResult<Bbox>>;
}

/// Registry for managing multiple data store backends.
pub struct DataStoreRegistry {
    stores: HashMap<String, Box<dyn DataStore>>,
}

impl DataStoreRegistry {
    pub fn new() -> Self {
        Self {
            stores: HashMap::new(),
        }
    }

    /// Register a named data store backend.
    pub async fn register(
        &mut self,
        name: String,
        store: Box<dyn DataStore>,
        config: Value,
    ) -> StoreResult<()> {
        store.connect(config).await?;
        self.stores.insert(name, store);
        Ok(())
    }

    /// Get a data store by name.
    pub fn get(&self, name: &str) -> Option<&dyn DataStore> {
        self.stores.get(name).map(|s| s.as_ref())
    }

    /// List all registered store names.
    pub fn list(&self) -> Vec<&str> {
        self.stores.keys().map(|s| s.as_str()).collect()
    }

    /// Disconnect and remove all stores.
    pub async fn disconnect_all(&mut self) {
        for (_, store) in self.stores.drain() {
            store.disconnect().await;
        }
    }
}

impl Default for DataStoreRegistry {
    fn default() -> Self {
        Self::new()
    }
}
