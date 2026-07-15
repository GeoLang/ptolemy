//! Ptolemy Elasticsearch Data Store Plugin
//!
//! Stores features as documents with a `geo_shape` geometry mapping, one
//! index per dataset (index_prefix + dataset name).
//!
//! ## Features
//! - GeoJSON geometry stored as geo_shape
//! - bbox queries via a geo_shape envelope filter (relation intersects)
//! - CRUD keyed by the feature uuid as the document _id

use std::sync::Arc;
use tokio::sync::RwLock;

use elasticsearch::auth::Credentials;
use elasticsearch::cat::CatIndicesParts;
use elasticsearch::http::Url;
use elasticsearch::http::transport::{SingleNodeConnectionPool, TransportBuilder};
use elasticsearch::indices::{IndicesCreateParts, IndicesExistsParts};
use elasticsearch::params::Refresh;
use elasticsearch::{CountParts, DeleteParts, Elasticsearch, GetParts, IndexParts, SearchParts};
use ptolemy_core::geoconvert::{geojson_to_wkb, wkb_to_geojson};
use ptolemy_core::{
    Bbox, BoxFuture, DataStore, DataStoreError, Dataset, Feature, FeatureQuery, GeometryType,
    StoreCapabilities, StoreResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

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
    config: Arc<RwLock<Option<ElasticsearchConfig>>>,
    client: Arc<RwLock<Option<Elasticsearch>>>,
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

    async fn ctx(&self) -> StoreResult<(Elasticsearch, ElasticsearchConfig)> {
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

    async fn index_name(&self, dataset: &str) -> StoreResult<String> {
        let (_, config) = self.ctx().await?;
        Ok(format!("{}{}", config.index_prefix, dataset).to_lowercase())
    }

    async fn ensure_index(&self, client: &Elasticsearch, index: &str) -> StoreResult<()> {
        let exists = client
            .indices()
            .exists(IndicesExistsParts::Index(&[index]))
            .send()
            .await
            .map_err(es_err)?;
        if exists.status_code().as_u16() == 200 {
            return Ok(());
        }
        let resp = client
            .indices()
            .create(IndicesCreateParts::Index(index))
            .body(json!({
                "mappings": {
                    "properties": {
                        "geometry": { "type": "geo_shape" },
                        "properties": { "type": "object", "enabled": true }
                    }
                }
            }))
            .send()
            .await
            .map_err(es_err)?;
        if !resp.status_code().is_success() {
            let body = resp.json::<Value>().await.map_err(es_err)?;
            return Err(DataStoreError::Query(format!(
                "create index {index}: {body}"
            )));
        }
        Ok(())
    }
}

impl Default for ElasticsearchStore {
    fn default() -> Self {
        Self::new()
    }
}

fn es_err(e: elasticsearch::Error) -> DataStoreError {
    DataStoreError::Query(e.to_string())
}

/// Derive a stable dataset id from an index/dataset name.
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
        _ => GeometryType::GeometryCollection,
    }
}

/// Build a Feature from a document _source (geometry is geo_shape geojson).
fn source_to_feature(dataset: &str, id: &str, source: &Value) -> StoreResult<Feature> {
    let geojson = source
        .get("geometry")
        .ok_or_else(|| DataStoreError::Internal("missing geometry".into()))?;
    let wkb = geojson_to_wkb(geojson)?;
    let properties = source
        .get("properties")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Feature {
        id: Uuid::parse_str(id)
            .map_err(|e| DataStoreError::Internal(format!("bad feature id {id}: {e}")))?,
        dataset_id: dataset_uuid(dataset),
        geometry_wkb: wkb,
        properties,
    })
}

/// bbox to a geo_shape envelope query, or a match_all.
fn build_query(query: &FeatureQuery) -> StoreResult<Value> {
    if query.filter.is_some() {
        return Err(DataStoreError::Unsupported(
            "attribute/cql filter not supported by elasticsearch store".into(),
        ));
    }
    match query.bbox {
        None => Ok(json!({ "match_all": {} })),
        // geo_shape envelope coordinates are [[minLon, maxLat], [maxLon, minLat]].
        Some([w, s, e, n]) => Ok(json!({
            "geo_shape": {
                "geometry": {
                    "shape": { "type": "envelope", "coordinates": [[w, n], [e, s]] },
                    "relation": "intersects"
                }
            }
        })),
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

impl DataStore for ElasticsearchStore {
    fn capabilities(&self) -> &StoreCapabilities {
        &self.capabilities
    }

    fn connect(&self, config: Value) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            let es_config: ElasticsearchConfig = serde_json::from_value(config)
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;
            let node = es_config
                .nodes
                .first()
                .ok_or_else(|| DataStoreError::Connection("no nodes configured".into()))?;
            let url = Url::parse(node).map_err(|e| DataStoreError::Connection(e.to_string()))?;
            let pool = SingleNodeConnectionPool::new(url);
            let mut builder = TransportBuilder::new(pool)
                .timeout(std::time::Duration::from_secs(es_config.timeout_secs));
            if let Some(auth) = &es_config.auth {
                let cred = match auth {
                    AuthConfig::Basic { username, password } => {
                        Credentials::Basic(username.clone(), password.clone())
                    }
                    AuthConfig::ApiKey { id, key } => Credentials::ApiKey(id.clone(), key.clone()),
                    AuthConfig::Bearer(token) => Credentials::Bearer(token.clone()),
                };
                builder = builder.auth(cred);
            }
            let transport = builder
                .build()
                .map_err(|e| DataStoreError::Connection(e.to_string()))?;
            *self.config.write().await = Some(es_config);
            *self.client.write().await = Some(Elasticsearch::new(transport));
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
            let prefix = config.index_prefix.to_lowercase();
            let pattern = format!("{prefix}*");
            let resp = client
                .cat()
                .indices(CatIndicesParts::Index(&[&pattern]))
                .format("json")
                .send()
                .await
                .map_err(es_err)?;
            let body = resp.json::<Value>().await.map_err(es_err)?;
            let mut out = Vec::new();
            for entry in body.as_array().cloned().unwrap_or_default() {
                let index = entry.get("index").and_then(Value::as_str).unwrap_or("");
                if index.is_empty() {
                    continue;
                }
                let name = index.strip_prefix(&prefix).unwrap_or(index).to_string();
                // sample one doc for the geometry type
                let sample = client
                    .search(SearchParts::Index(&[index]))
                    .body(json!({ "size": 1 }))
                    .send()
                    .await
                    .map_err(es_err)?
                    .json::<Value>()
                    .await
                    .map_err(es_err)?;
                let geometry_type = sample["hits"]["hits"]
                    .get(0)
                    .and_then(|h| h["_source"].get("geometry"))
                    .map(geometry_type_from_geojson)
                    .unwrap_or(GeometryType::GeometryCollection);
                out.push(Dataset {
                    id: dataset_uuid(&name),
                    name,
                    srid: 4326,
                    geometry_type,
                    created_at: OffsetDateTime::now_utc(),
                    created_by: "elasticsearch".into(),
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
            let q = build_query(&query)?;
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            let mut body = json!({
                "query": q,
                "size": query.limit.unwrap_or(self.capabilities.max_features),
                "from": query.offset.unwrap_or(0),
            });
            if let Some(sort) = &query.sort_by {
                let dir = if query.sort_asc { "asc" } else { "desc" };
                body["sort"] = json!([{ format!("properties.{sort}"): { "order": dir } }]);
            }
            let resp = client
                .search(SearchParts::Index(&[&index]))
                .body(body)
                .send()
                .await
                .map_err(es_err)?;
            if resp.status_code().as_u16() == 404 {
                return Ok(Vec::new());
            }
            let body = resp.json::<Value>().await.map_err(es_err)?;
            let mut out = Vec::new();
            for hit in body["hits"]["hits"].as_array().cloned().unwrap_or_default() {
                let id = hit["_id"].as_str().unwrap_or_default();
                let mut feature = source_to_feature(&dataset, id, &hit["_source"])?;
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
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            let resp = client
                .get(GetParts::IndexId(&index, &id))
                .send()
                .await
                .map_err(es_err)?;
            if resp.status_code().as_u16() == 404 {
                return Err(DataStoreError::NotFound(format!("feature {id}")));
            }
            let body = resp.json::<Value>().await.map_err(es_err)?;
            if !body["found"].as_bool().unwrap_or(false) {
                return Err(DataStoreError::NotFound(format!("feature {id}")));
            }
            source_to_feature(&dataset, &id, &body["_source"])
        })
    }

    fn count_features(
        &self,
        dataset: &str,
        query: FeatureQuery,
    ) -> BoxFuture<'_, StoreResult<u64>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let q = build_query(&query)?;
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            let resp = client
                .count(CountParts::Index(&[&index]))
                .body(json!({ "query": q }))
                .send()
                .await
                .map_err(es_err)?;
            if resp.status_code().as_u16() == 404 {
                return Ok(0);
            }
            let body = resp.json::<Value>().await.map_err(es_err)?;
            Ok(body["count"].as_u64().unwrap_or(0))
        })
    }

    fn insert_feature(
        &self,
        dataset: &str,
        feature: Feature,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            self.ensure_index(&client, &index).await?;
            let id = feature.id.to_string();
            let geojson = wkb_to_geojson(&feature.geometry_wkb)?;
            let resp = client
                .index(IndexParts::IndexId(&index, &id))
                .refresh(Refresh::True)
                .body(json!({ "geometry": geojson, "properties": feature.properties }))
                .send()
                .await
                .map_err(es_err)?;
            if !resp.status_code().is_success() {
                let body = resp.json::<Value>().await.map_err(es_err)?;
                return Err(DataStoreError::Query(format!("index doc: {body}")));
            }
            Ok(id)
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
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            let exists = client
                .get(GetParts::IndexId(&index, &id))
                .send()
                .await
                .map_err(es_err)?;
            if exists.status_code().as_u16() == 404 {
                return Err(DataStoreError::NotFound(format!("feature {id}")));
            }
            let geojson = wkb_to_geojson(&feature.geometry_wkb)?;
            let resp = client
                .index(IndexParts::IndexId(&index, &id))
                .refresh(Refresh::True)
                .body(json!({ "geometry": geojson, "properties": feature.properties }))
                .send()
                .await
                .map_err(es_err)?;
            if !resp.status_code().is_success() {
                let body = resp.json::<Value>().await.map_err(es_err)?;
                return Err(DataStoreError::Query(format!("update doc: {body}")));
            }
            Ok(())
        })
    }

    fn delete_feature(&self, dataset: &str, id: &str) -> BoxFuture<'_, StoreResult<()>> {
        let dataset = dataset.to_string();
        let id = id.to_string();
        Box::pin(async move {
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            let resp = client
                .delete(DeleteParts::IndexId(&index, &id))
                .refresh(Refresh::True)
                .send()
                .await
                .map_err(es_err)?;
            if resp.status_code().as_u16() == 404 {
                return Err(DataStoreError::NotFound(format!("feature {id}")));
            }
            if !resp.status_code().is_success() {
                let body = resp.json::<Value>().await.map_err(es_err)?;
                return Err(DataStoreError::Query(format!("delete doc: {body}")));
            }
            Ok(())
        })
    }

    fn get_extent(&self, dataset: &str) -> BoxFuture<'_, StoreResult<Bbox>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let (client, _) = self.ctx().await?;
            let index = self.index_name(&dataset).await?;
            let resp = client
                .search(SearchParts::Index(&[&index]))
                .body(json!({
                    "size": 0,
                    "aggs": { "bb": { "geo_bounds": { "field": "geometry" } } }
                }))
                .send()
                .await
                .map_err(es_err)?;
            if resp.status_code().as_u16() == 404 {
                return Err(DataStoreError::NotFound(format!("dataset {dataset}")));
            }
            let body = resp.json::<Value>().await.map_err(es_err)?;
            let bounds = &body["aggregations"]["bb"]["bounds"];
            match (
                bounds["top_left"]["lon"].as_f64(),
                bounds["top_left"]["lat"].as_f64(),
                bounds["bottom_right"]["lon"].as_f64(),
                bounds["bottom_right"]["lat"].as_f64(),
            ) {
                (Some(w), Some(n), Some(e), Some(s)) => Ok([w, s, e, n]),
                // no geometries indexed yet
                _ => Ok([0.0, 0.0, 0.0, 0.0]),
            }
        })
    }
}
