//! Integration tests against a real Elasticsearch. Ignored by default; run with
//! a throwaway container:
//!   docker run --rm -d --name ptolemy-es-test -p 9209:9200 \
//!     -e discovery.type=single-node -e xpack.security.enabled=false \
//!     docker.elastic.co/elasticsearch/elasticsearch:8.15.0
//!   cargo test -p ptolemy-elasticsearch -- --ignored
//! Override the node with PTOLEMY_ES_URL if needed.

use ptolemy_core::geoconvert::{geojson_to_wkb, wkb_to_geojson};
use ptolemy_core::{DataStore, DataStoreError, Feature, FeatureQuery, GeometryType};
use ptolemy_elasticsearch::ElasticsearchStore;
use serde_json::json;
use uuid::Uuid;

fn point(x: f64, y: f64) -> Vec<u8> {
    geojson_to_wkb(&json!({"type": "Point", "coordinates": [x, y]})).unwrap()
}

fn feature(wkb: Vec<u8>, props: serde_json::Value) -> Feature {
    Feature {
        id: Uuid::new_v4(),
        dataset_id: Uuid::new_v4(),
        geometry_wkb: wkb,
        properties: props,
    }
}

fn node() -> String {
    std::env::var("PTOLEMY_ES_URL").unwrap_or_else(|_| "http://localhost:9209".into())
}

/// Fresh store with a unique index prefix so runs don't collide.
async fn store(tag: &str) -> ElasticsearchStore {
    let prefix = format!("ptolemy_test_{tag}_{}_", Uuid::new_v4().simple());
    let s = ElasticsearchStore::new();
    s.connect(json!({
        "nodes": [node()],
        "index_prefix": prefix,
        "auth": null,
        "timeout_secs": 30,
        "scroll_ttl": "1m",
    }))
    .await
    .unwrap();
    s
}

#[tokio::test]
#[ignore = "needs elasticsearch"]
async fn crud_roundtrip() {
    let s = store("crud").await;
    let f = feature(point(3.0, 4.0), json!({"name": "a"}));
    let expected = f.geometry_wkb.clone();
    let id = s.insert_feature("roads", f).await.unwrap();

    let got = s.get_feature("roads", &id).await.unwrap();
    assert_eq!(got.id.to_string(), id);
    assert_eq!(got.properties["name"], "a");
    assert_eq!(
        wkb_to_geojson(&got.geometry_wkb).unwrap(),
        wkb_to_geojson(&expected).unwrap()
    );

    s.update_feature("roads", &id, feature(point(7.0, 8.0), json!({"name": "b"})))
        .await
        .unwrap();
    assert_eq!(
        s.get_feature("roads", &id).await.unwrap().properties["name"],
        "b"
    );

    s.delete_feature("roads", &id).await.unwrap();
    assert!(matches!(
        s.get_feature("roads", &id).await,
        Err(DataStoreError::NotFound(_))
    ));
    assert!(matches!(
        s.delete_feature("roads", &id).await,
        Err(DataStoreError::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "needs elasticsearch"]
async fn bbox_query_and_count() {
    let s = store("bbox").await;
    s.insert_feature("pts", feature(point(0.0, 0.0), json!({"tag": "near"})))
        .await
        .unwrap();
    s.insert_feature("pts", feature(point(20.0, 20.0), json!({"tag": "far"})))
        .await
        .unwrap();

    assert_eq!(
        s.count_features("pts", FeatureQuery::default())
            .await
            .unwrap(),
        2
    );
    let q = FeatureQuery {
        bbox: Some([-1.0, -1.0, 1.0, 1.0]),
        ..Default::default()
    };
    let hits = s.get_features("pts", q.clone()).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].properties["tag"], "near");
    assert_eq!(s.count_features("pts", q).await.unwrap(), 1);
}

#[tokio::test]
#[ignore = "needs elasticsearch"]
async fn list_datasets_and_extent() {
    let s = store("list").await;
    s.insert_feature("places", feature(point(-5.0, -3.0), json!({})))
        .await
        .unwrap();
    s.insert_feature("places", feature(point(8.0, 9.0), json!({})))
        .await
        .unwrap();

    let datasets = s.list_datasets().await.unwrap();
    assert_eq!(datasets.len(), 1);
    assert_eq!(datasets[0].name, "places");
    assert_eq!(datasets[0].geometry_type, GeometryType::Point);

    let ext = s.get_extent("places").await.unwrap();
    assert!((ext[0] - -5.0).abs() < 1e-6);
    assert!((ext[1] - -3.0).abs() < 1e-6);
    assert!((ext[2] - 8.0).abs() < 1e-6);
    assert!((ext[3] - 9.0).abs() < 1e-6);
}

#[tokio::test]
#[ignore = "needs elasticsearch"]
async fn cql_filter_unsupported() {
    let s = store("filter").await;
    let q = FeatureQuery {
        filter: Some("x = 1".into()),
        ..Default::default()
    };
    assert!(matches!(
        s.get_features("pts", q).await,
        Err(DataStoreError::Unsupported(_))
    ));
}
