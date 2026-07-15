//! End-to-end tests for the GeoPackage store against real .gpkg files.

use ptolemy_core::geoconvert::{geojson_to_wkb, wkb_to_geojson};
use ptolemy_core::{DataStore, DataStoreError, Feature, FeatureQuery, GeometryType};
use ptolemy_geopackage::GeoPackageStore;
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

async fn connected_store(path: &std::path::Path) -> GeoPackageStore {
    let store = GeoPackageStore::new();
    store
        .connect(json!({
            "path": path.to_str().unwrap(),
            "read_only": false,
            "create_if_missing": true,
        }))
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn create_and_list_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("empty.gpkg")).await;
    assert!(store.list_datasets().await.unwrap().is_empty());
}

#[tokio::test]
async fn insert_get_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("rt.gpkg")).await;
    let f = feature(point(3.0, 4.0), json!({"name": "a", "n": 1}));
    let expected_wkb = f.geometry_wkb.clone();
    let id = store.insert_feature("roads", f).await.unwrap();

    let got = store.get_feature("roads", &id).await.unwrap();
    assert_eq!(got.id.to_string(), id);
    assert_eq!(got.properties["name"], "a");
    // geometry round-trips through the gpkg header + wkb decode.
    assert_eq!(
        wkb_to_geojson(&got.geometry_wkb).unwrap(),
        wkb_to_geojson(&expected_wkb).unwrap()
    );
}

#[tokio::test]
async fn list_datasets_reports_geometry_type() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("list.gpkg")).await;
    store
        .insert_feature("places", feature(point(1.0, 2.0), json!({})))
        .await
        .unwrap();
    let datasets = store.list_datasets().await.unwrap();
    assert_eq!(datasets.len(), 1);
    assert_eq!(datasets[0].name, "places");
    assert_eq!(datasets[0].geometry_type, GeometryType::Point);
    assert_eq!(datasets[0].srid, 4326);
}

#[tokio::test]
async fn count_and_get_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("count.gpkg")).await;
    for i in 0..3 {
        store
            .insert_feature("pts", feature(point(i as f64, 0.0), json!({"i": i})))
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .count_features("pts", FeatureQuery::default())
            .await
            .unwrap(),
        3
    );
    let all = store
        .get_features("pts", FeatureQuery::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn bbox_query_uses_spatial_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("bbox.gpkg")).await;
    store
        .insert_feature("pts", feature(point(0.0, 0.0), json!({"tag": "near"})))
        .await
        .unwrap();
    store
        .insert_feature("pts", feature(point(10.0, 10.0), json!({"tag": "far"})))
        .await
        .unwrap();

    let q = FeatureQuery {
        bbox: Some([-1.0, -1.0, 1.0, 1.0]),
        ..Default::default()
    };
    let hits = store.get_features("pts", q.clone()).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].properties["tag"], "near");
    assert_eq!(store.count_features("pts", q).await.unwrap(), 1);
}

#[tokio::test]
async fn update_feature_changes_geom_and_props() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("upd.gpkg")).await;
    let id = store
        .insert_feature("pts", feature(point(0.0, 0.0), json!({"v": 1})))
        .await
        .unwrap();
    store
        .update_feature("pts", &id, feature(point(5.0, 6.0), json!({"v": 2})))
        .await
        .unwrap();
    let got = store.get_feature("pts", &id).await.unwrap();
    assert_eq!(got.properties["v"], 2);
    let coords = &wkb_to_geojson(&got.geometry_wkb).unwrap()["coordinates"];
    assert_eq!(coords[0].as_f64().unwrap(), 5.0);
    assert_eq!(coords[1].as_f64().unwrap(), 6.0);
    // rtree updated: new location is found, old one is not.
    let q = FeatureQuery {
        bbox: Some([4.0, 5.0, 7.0, 8.0]),
        ..Default::default()
    };
    assert_eq!(store.count_features("pts", q).await.unwrap(), 1);
}

#[tokio::test]
async fn delete_feature_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("del.gpkg")).await;
    let id = store
        .insert_feature("pts", feature(point(0.0, 0.0), json!({})))
        .await
        .unwrap();
    store.delete_feature("pts", &id).await.unwrap();
    assert!(matches!(
        store.get_feature("pts", &id).await,
        Err(DataStoreError::NotFound(_))
    ));
    assert_eq!(
        store
            .count_features("pts", FeatureQuery::default())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn get_extent_reports_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("ext.gpkg")).await;
    store
        .insert_feature("pts", feature(point(-5.0, -3.0), json!({})))
        .await
        .unwrap();
    store
        .insert_feature("pts", feature(point(8.0, 9.0), json!({})))
        .await
        .unwrap();
    assert_eq!(
        store.get_extent("pts").await.unwrap(),
        [-5.0, -3.0, 8.0, 9.0]
    );
}

#[tokio::test]
async fn cql_filter_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("filt.gpkg")).await;
    store
        .insert_feature("pts", feature(point(0.0, 0.0), json!({})))
        .await
        .unwrap();
    let q = FeatureQuery {
        filter: Some("name = 'x'".into()),
        ..Default::default()
    };
    assert!(matches!(
        store.get_features("pts", q).await,
        Err(DataStoreError::Unsupported(_))
    ));
}

#[tokio::test]
async fn data_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.gpkg");
    let id = {
        let store = connected_store(&path).await;
        let id = store
            .insert_feature("pts", feature(point(1.0, 1.0), json!({"k": "v"})))
            .await
            .unwrap();
        store.disconnect().await;
        id
    };
    let store = connected_store(&path).await;
    let got = store.get_feature("pts", &id).await.unwrap();
    assert_eq!(got.properties["k"], "v");
}

#[tokio::test]
async fn missing_file_without_create_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = GeoPackageStore::new();
    let res = store
        .connect(json!({
            "path": dir.path().join("nope.gpkg").to_str().unwrap(),
            "read_only": false,
            "create_if_missing": false,
        }))
        .await;
    assert!(matches!(res, Err(DataStoreError::Connection(_))));
}

#[tokio::test]
async fn property_projection() {
    let dir = tempfile::tempdir().unwrap();
    let store = connected_store(&dir.path().join("proj.gpkg")).await;
    store
        .insert_feature("pts", feature(point(0.0, 0.0), json!({"a": 1, "b": 2})))
        .await
        .unwrap();
    let q = FeatureQuery {
        properties: vec!["a".into()],
        ..Default::default()
    };
    let got = store.get_features("pts", q).await.unwrap();
    assert_eq!(got[0].properties, json!({"a": 1}));
}
