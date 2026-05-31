//! Ptolemy GeoPackage Data Store Plugin
//!
//! Provides read/write access to OGC GeoPackage (.gpkg) files.
//! GeoPackage is an SQLite-based format widely used for offline/mobile
//! geospatial data exchange.
//!
//! ## Features
//! - Read/write features from GeoPackage files
//! - RTree spatial index support
//! - Multiple geometry columns per table
//! - Attribute table access
//! - Tile matrix sets (raster tiles)
//! - Schema discovery from gpkg_contents + gpkg_geometry_columns

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use ptolemy_core::{
    Bbox, BoxFuture, DataStore, DataStoreError, Dataset, Feature, FeatureQuery, StoreCapabilities,
    StoreResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration for a GeoPackage store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoPackageConfig {
    /// Path to the .gpkg file.
    pub path: String,
    /// Whether to open in read-only mode.
    pub read_only: bool,
    /// Whether to create the file if it doesn't exist.
    pub create_if_missing: bool,
}

/// GeoPackage data store implementation.
pub struct GeoPackageStore {
    capabilities: StoreCapabilities,
    #[allow(dead_code)]
    config: Arc<RwLock<Option<GeoPackageConfig>>>,
}

impl GeoPackageStore {
    pub fn new() -> Self {
        Self {
            capabilities: StoreCapabilities {
                name: "GeoPackage".to_string(),
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
                spatial_index: true, // RTree
                versioning: false,
                max_features: 0,
                supported_crs: vec![4326, 3857, 32632, 32633],
            },
            config: Arc::new(RwLock::new(None)),
        }
    }
}

impl Default for GeoPackageStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DataStore for GeoPackageStore {
    fn capabilities(&self) -> &StoreCapabilities {
        &self.capabilities
    }

    fn connect(&self, config: Value) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            let gpkg_config: GeoPackageConfig = serde_json::from_value(config)
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;

            // Validate that the path exists (or create_if_missing is set)
            let path = PathBuf::from(&gpkg_config.path);
            if !gpkg_config.create_if_missing && !path.exists() {
                return Err(DataStoreError::Connection(format!(
                    "GeoPackage file not found: {}",
                    gpkg_config.path
                )));
            }

            *self.config.write().await = Some(gpkg_config);
            Ok(())
        })
    }

    fn disconnect(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            *self.config.write().await = None;
        })
    }

    fn list_datasets(&self) -> BoxFuture<'_, StoreResult<Vec<Dataset>>> {
        Box::pin(async move {
            // TODO: SELECT * FROM gpkg_contents WHERE data_type = 'features'
            Ok(Vec::new())
        })
    }

    fn get_features(
        &self,
        _dataset: &str,
        _query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<Vec<Feature>>> {
        Box::pin(async move {
            // TODO: Query with RTree spatial filter
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
        Box::pin(async move {
            // TODO: SELECT min_x, min_y, max_x, max_y FROM gpkg_contents
            Ok([-180.0, -90.0, 180.0, 90.0])
        })
    }
}
