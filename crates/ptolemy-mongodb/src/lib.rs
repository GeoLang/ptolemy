//! Ptolemy MongoDB Data Store Plugin
//!
//! Stores features as documents with a GeoJSON `geometry` field and a
//! 2dsphere index, using MongoDB's native geospatial queries.
//!
//! ## Features
//! - GeoJSON geometry storage with a 2dsphere spatial index
//! - bbox queries via `$geoIntersects` with a polygon
//! - one collection per dataset (prefix + dataset name)

use std::sync::Arc;
use tokio::sync::RwLock;

use futures_util::TryStreamExt;
use mongodb::bson::{Document, doc};
use mongodb::{Client, Collection, IndexModel};
use ptolemy_core::geoconvert::{geojson_to_wkb, wkb_bbox, wkb_to_geojson};
use ptolemy_core::{
    Bbox, BoxFuture, DataStore, DataStoreError, Dataset, Feature, FeatureQuery, GeometryType,
    StoreCapabilities, StoreResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

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
    config: Arc<RwLock<Option<MongoConfig>>>,
    client: Arc<RwLock<Option<Client>>>,
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

    /// Fetch the client and config, erroring if disconnected.
    async fn ctx(&self) -> StoreResult<(Client, MongoConfig)> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or_else(|| DataStoreError::Connection("not connected".into()))?;
        let config = self
            .config
            .read()
            .await
            .clone()
            .ok_or_else(|| DataStoreError::Connection("not connected".into()))?;
        Ok((client, config))
    }

    async fn collection(&self, dataset: &str) -> StoreResult<Collection<Document>> {
        let (client, config) = self.ctx().await?;
        let name = format!("{}{}", config.collection_prefix, dataset);
        Ok(client.database(&config.database).collection(&name))
    }
}

impl Default for MongoStore {
    fn default() -> Self {
        Self::new()
    }
}

fn mongo_err(e: mongodb::error::Error) -> DataStoreError {
    DataStoreError::Query(e.to_string())
}

/// Derive a stable dataset id from a collection/dataset name.
fn dataset_uuid(name: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes())
}

fn geometry_type_from_geojson(geom: &Value) -> GeometryType {
    match geom.get("type").and_then(Value::as_str).unwrap_or("") {
        "Point" => GeometryType::Point,
        "LineString" => GeometryType::LineString,
        "Polygon" => GeometryType::Polygon,
        "MultiPoint" => GeometryType::MultiPoint,
        "MultiLineString" => GeometryType::MultiLineString,
        "MultiPolygon" => GeometryType::MultiPolygon,
        "GeometryCollection" => GeometryType::GeometryCollection,
        // no usable type on the sample, so nothing narrower can be claimed
        _ => GeometryType::Geometry,
    }
}

/// Build the stored document for a feature (geometry as GeoJSON).
fn feature_to_doc(feature: &Feature) -> StoreResult<Document> {
    let geojson = wkb_to_geojson(&feature.geometry_wkb)?;
    let geom = mongodb::bson::to_bson(&geojson)
        .map_err(|e| DataStoreError::Internal(format!("geometry to bson: {e}")))?;
    let props = mongodb::bson::to_bson(&feature.properties)
        .map_err(|e| DataStoreError::Internal(format!("properties to bson: {e}")))?;
    Ok(doc! {
        "_id": feature.id.to_string(),
        "geometry": geom,
        "properties": props,
    })
}

/// Rebuild a Feature from a stored document, decoding GeoJSON back to WKB.
fn doc_to_feature(dataset: &str, doc: &Document) -> StoreResult<Feature> {
    let id = doc
        .get_str("_id")
        .map_err(|e| DataStoreError::Internal(format!("missing _id: {e}")))?;
    let geom_bson = doc
        .get("geometry")
        .cloned()
        .ok_or_else(|| DataStoreError::Internal("missing geometry".into()))?;
    let geojson: Value = mongodb::bson::from_bson(geom_bson)
        .map_err(|e| DataStoreError::Internal(format!("geometry from bson: {e}")))?;
    let wkb = geojson_to_wkb(&geojson)?;
    let properties: Value = match doc.get("properties").cloned() {
        Some(b) => mongodb::bson::from_bson(b)
            .map_err(|e| DataStoreError::Internal(format!("properties from bson: {e}")))?,
        None => json!({}),
    };
    Ok(Feature {
        id: Uuid::parse_str(id)
            .map_err(|e| DataStoreError::Internal(format!("bad feature id {id}: {e}")))?,
        dataset_id: dataset_uuid(dataset),
        geometry_wkb: wkb,
        properties,
        valid_from: None,
        valid_to: None,
    })
}

/// bbox to a `$geoIntersects` polygon filter, or an empty filter.
fn bbox_filter(query: &FeatureQuery) -> StoreResult<Document> {
    if query.filter.is_some() {
        return Err(DataStoreError::Unsupported(
            "attribute/cql filter not supported by mongodb store".into(),
        ));
    }
    match query.bbox {
        None => Ok(doc! {}),
        Some([w, s, e, n]) => {
            let poly = json!({
                "type": "Polygon",
                "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]],
            });
            let geom = mongodb::bson::to_bson(&poly)
                .map_err(|e| DataStoreError::Internal(format!("bbox to bson: {e}")))?;
            Ok(doc! { "geometry": { "$geoIntersects": { "$geometry": geom } } })
        }
    }
}

fn project_properties(props: &mut Value, keep: &[String]) {
    if keep.is_empty() {
        return;
    }
    if let Value::Object(map) = props {
        map.retain(|k, _| keep.iter().any(|w| w == k));
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
            let client = Client::with_uri_str(&mongo_config.uri)
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
            let (client, config) = self.ctx().await?;
            let db = client.database(&config.database);
            let names = db.list_collection_names().await.map_err(mongo_err)?;
            let mut out = Vec::new();
            for name in names {
                if !config.collection_prefix.is_empty()
                    && !name.starts_with(&config.collection_prefix)
                {
                    continue;
                }
                let dataset = name
                    .strip_prefix(&config.collection_prefix)
                    .unwrap_or(&name)
                    .to_string();
                let coll: Collection<Document> = db.collection(&name);
                let sample = coll.find_one(doc! {}).await.map_err(mongo_err)?;
                let geometry_type = sample
                    .as_ref()
                    .and_then(|d| d.get("geometry").cloned())
                    .and_then(|b| mongodb::bson::from_bson::<Value>(b).ok())
                    .map(|g| geometry_type_from_geojson(&g))
                    .unwrap_or(GeometryType::GeometryCollection);
                out.push(Dataset {
                    id: dataset_uuid(&dataset),
                    name: dataset,
                    srid: 4326,
                    geometry_type,
                    created_at: OffsetDateTime::now_utc(),
                    created_by: "mongodb".into(),
                    external: None,
                    visibility: Default::default(),
                    project_id: None,
                });
            }
            Ok(out)
        })
    }

    fn get_features(
        &self,
        dataset: &str,
        query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<Vec<Feature>>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let filter = bbox_filter(&query)?;
            let coll = self.collection(&dataset).await?;
            let mut find = coll.find(filter);
            if let Some(limit) = query.limit {
                find = find.limit(limit as i64);
            }
            if let Some(offset) = query.offset {
                find = find.skip(offset as u64);
            }
            if let Some(sort) = &query.sort_by {
                let dir = if query.sort_asc { 1 } else { -1 };
                find = find.sort(doc! { format!("properties.{sort}"): dir });
            }
            let docs: Vec<Document> = find
                .await
                .map_err(mongo_err)?
                .try_collect()
                .await
                .map_err(mongo_err)?;
            let mut out = Vec::with_capacity(docs.len());
            for d in &docs {
                let mut feature = doc_to_feature(&dataset, d)?;
                project_properties(&mut feature.properties, &query.properties);
                out.push(feature);
            }
            Ok(out)
        })
    }

    fn get_feature(&self, dataset: &str, id: &str) -> BoxFuture<'_, StoreResult<Feature>> {
        let dataset = dataset.to_string();
        let id = id.to_string();
        Box::pin(async move {
            let coll = self.collection(&dataset).await?;
            let doc = coll
                .find_one(doc! { "_id": &id })
                .await
                .map_err(mongo_err)?
                .ok_or_else(|| DataStoreError::NotFound(format!("feature {id}")))?;
            doc_to_feature(&dataset, &doc)
        })
    }

    fn count_features(
        &self,
        dataset: &str,
        query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<u64>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let filter = bbox_filter(&query)?;
            let coll = self.collection(&dataset).await?;
            coll.count_documents(filter).await.map_err(mongo_err)
        })
    }

    fn insert_feature(
        &self,
        dataset: &str,
        feature: Feature,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let (_, config) = self.ctx().await?;
            let coll = self.collection(&dataset).await?;
            let doc = feature_to_doc(&feature)?;
            if config.auto_index {
                let index = IndexModel::builder()
                    .keys(doc! { "geometry": "2dsphere" })
                    .build();
                coll.create_index(index).await.map_err(mongo_err)?;
            }
            coll.insert_one(&doc).await.map_err(mongo_err)?;
            Ok(feature.id.to_string())
        })
    }

    fn update_feature(
        &self,
        dataset: &str,
        id: &str,
        feature: Feature,
    ) -> BoxFuture<'_, StoreResult<()>> {
        let dataset = dataset.to_string();
        let id = id.to_string();
        Box::pin(async move {
            let coll = self.collection(&dataset).await?;
            let mut doc = feature_to_doc(&feature)?;
            doc.insert("_id", &id);
            let res = coll
                .replace_one(doc! { "_id": &id }, &doc)
                .await
                .map_err(mongo_err)?;
            if res.matched_count == 0 {
                return Err(DataStoreError::NotFound(format!("feature {id}")));
            }
            Ok(())
        })
    }

    fn delete_feature(&self, dataset: &str, id: &str) -> BoxFuture<'_, StoreResult<()>> {
        let dataset = dataset.to_string();
        let id = id.to_string();
        Box::pin(async move {
            let coll = self.collection(&dataset).await?;
            let res = coll
                .delete_one(doc! { "_id": &id })
                .await
                .map_err(mongo_err)?;
            if res.deleted_count == 0 {
                return Err(DataStoreError::NotFound(format!("feature {id}")));
            }
            Ok(())
        })
    }

    fn get_extent(&self, dataset: &str) -> BoxFuture<'_, StoreResult<Bbox>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let coll = self.collection(&dataset).await?;
            let docs: Vec<Document> = coll
                .find(doc! {})
                .await
                .map_err(mongo_err)?
                .try_collect()
                .await
                .map_err(mongo_err)?;
            let mut ext: Option<Bbox> = None;
            for d in &docs {
                let feature = doc_to_feature(&dataset, d)?;
                let [minx, miny, maxx, maxy] = wkb_bbox(&feature.geometry_wkb)?;
                ext = Some(match ext {
                    None => [minx, miny, maxx, maxy],
                    Some([w, s, e, n]) => [w.min(minx), s.min(miny), e.max(maxx), n.max(maxy)],
                });
            }
            Ok(ext.unwrap_or([0.0, 0.0, 0.0, 0.0]))
        })
    }
}
