//! Integration tests against a real MongoDB. Ignored by default; run with a
//! throwaway container:
//!   docker run --rm -d --name ptolemy-mongo-test -p 27019:27017 mongo:7
//!   cargo test -p ptolemy-mongodb -- --ignored
//! Override the uri with PTOLEMY_MONGO_URI if needed.

use ptolemy_core::geoconvert::{geojson_to_wkb, wkb_to_geojson};
use ptolemy_core::{DataStore, DataStoreError, Feature, FeatureQuery, GeometryType};
use ptolemy_mongodb::MongoStore;
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
        valid_from: None,
        valid_to: None,
    }
}

fn uri() -> String {
    std::env::var("PTOLEMY_MONGO_URI").unwrap_or_else(|_| "mongodb://localhost:27019".into())
}

/// Fresh store against a uniquely named database so runs don't collide.
async fn store(db: &str) -> MongoStore {
    let s = MongoStore::new();
    s.connect(json!({
        "uri": uri(),
        "database": db,
        "collection_prefix": "ds_",
        "auto_index": true,
    }))
    .await
    .unwrap();
    s
}

fn db_name(tag: &str) -> String {
    format!("ptolemy_test_{tag}_{}", Uuid::new_v4().simple())
}

#[tokio::test]
#[ignore = "needs mongodb"]
async fn crud_roundtrip() {
    let s = store(&db_name("crud")).await;
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

    // update
    s.update_feature("roads", &id, feature(point(7.0, 8.0), json!({"name": "b"})))
        .await
        .unwrap();
    let got = s.get_feature("roads", &id).await.unwrap();
    assert_eq!(got.properties["name"], "b");

    // delete
    s.delete_feature("roads", &id).await.unwrap();
    assert!(matches!(
        s.get_feature("roads", &id).await,
        Err(DataStoreError::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "needs mongodb"]
async fn bbox_query_and_count() {
    let s = store(&db_name("bbox")).await;
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
#[ignore = "needs mongodb"]
async fn list_datasets_and_extent() {
    let s = store(&db_name("list")).await;
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
    assert_eq!(
        s.get_extent("places").await.unwrap(),
        [-5.0, -3.0, 8.0, 9.0]
    );
}

#[tokio::test]
#[ignore = "needs mongodb"]
async fn cql_filter_unsupported() {
    let s = store(&db_name("filter")).await;
    let q = FeatureQuery {
        filter: Some("x = 1".into()),
        ..Default::default()
    };
    assert!(matches!(
        s.get_features("pts", q).await,
        Err(DataStoreError::Unsupported(_))
    ));
}
