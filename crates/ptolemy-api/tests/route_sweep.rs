//! Every mounted route, called once against a migrated database.
//!
//! What this catches: a handler that queries a column or a table the migrations
//! never create. Four feature families shipped with one, three of them found by
//! accident, because every query in this crate is a runtime `sqlx::query` and
//! nothing checks it against the schema until it runs.
//!
//! How it catches it: the route list is read off the router, every route is
//! called with fixture data, and the log is watched for SQLSTATE 42703
//! (undefined column) and 42P01 (undefined table). The log rather than the
//! response, because a handler flattens a database error to `internal error`
//! with a 500 and the arcgis facade answers 200 with the failure in the body.
//! `errors::log_db_error` is the one place they all go through.
//!
//! A 404 or a 400 is a pass: the sweep cannot build a plausible fixture for
//! every route, and a route that answers "no such thing" still ran its query.
//! What is not a pass is an extractor refusing the request, since then the
//! handler never ran at all. Those fail with the route named, and the fix is an
//! entry in [`BODY`] or [`QUERY`], or a line in [`SKIPPED`] saying why not.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ptolemy_api::errors::{UNDEFINED_COLUMN, UNDEFINED_TABLE};
use ptolemy_api::{AppState, AuthConfig, app_with_auth};
use ptolemy_storage::postgres::PgStore;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════════
// What the sweep does not call
// ═══════════════════════════════════════════════════════════════════════

/// One line per route, and the reason. Nothing else may be left out.
const SKIPPED: &[(&str, &str)] = &[
    (
        "GET /api/v1/events/stream",
        "an sse stream that never ends, and the handler runs no sql",
    ),
    (
        "GET /ws/branches/{branch_id}",
        "needs a websocket handshake, which oneshot cannot do: api_integration.rs drives a real one",
    ),
    (
        "GET /ws/rooms/{room_id}",
        "needs a websocket handshake, same as above",
    ),
    (
        "GET /auth/oidc/login",
        "redirects to the configured provider, so calling it is a network call, not a query",
    ),
    (
        "GET /auth/oidc/callback",
        "exchanges a code with the provider, so calling it is a network call, not a query",
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/{layer}/{oid}/addAttachment",
        "takes a multipart upload, which api_integration.rs covers",
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/{layer}/{oid}/updateAttachment",
        "takes a multipart upload, same as above",
    ),
];

// ═══════════════════════════════════════════════════════════════════════
// Fixture data
// ═══════════════════════════════════════════════════════════════════════

/// A path parameter is filled from the fixture named by the segment in front of
/// it, so `/api/v1/rasters/{id}` gets the raster the sweep created. This names
/// the parameters where that rule does not hold, keyed by the path up to and
/// including the parameter.
const PARAM_BY_PATH: &[(&str, &str)] = &[
    // an ogc collection is a dataset, a stac collection is a raster catalog
    ("/api/v1/ogc/collections/{id}", "datasets"),
    ("/api/v1/stac/collections/{id}", "rasters"),
    ("/api/v1/stac/collections/{id}/items/{item_id}", "tiles"),
    // merging a branch into itself is not a merge, so the source is the second one
    (
        "/api/v1/branches/{target_id}/merge/{source_id}",
        "other_branches",
    ),
];

/// Same, by parameter name wherever it appears.
const PARAM_BY_NAME: &[(&str, &str)] = &[
    ("dataset_id", "datasets"),
    ("branch_id", "branches"),
    ("target_id", "branches"),
    ("from_id", "branches"),
    ("to_id", "other_branches"),
    ("source_id", "other_branches"),
    ("feature_id", "features"),
    ("fid", "features"),
    ("rule_id", "topology"),
    ("item_id", "tiles"),
];

/// A `POST` that creates something registers what it made under the last static
/// segment of its path, so `POST /api/v1/datasets/{id}/rasters` fills the `{id}`
/// of `/api/v1/rasters/{id}`. This names the ones read back under another word.
const LEARN_AS: &[(&str, &str)] = &[
    ("relationships", "relationship-classes"),
    ("records", "relationship-records"),
];

/// Query strings, keyed by `METHOD path`. Only where the handler requires one,
/// or where a second one reaches a query the first does not.
const QUERY: &[(&str, &str)] = &[
    (
        "GET /api/v1/branches/{id}/features/bbox",
        "min_x=-1&min_y=-1&max_x=1&max_y=1",
    ),
    (
        "GET /api/v1/branches/{id}/features/at",
        "at=2020-01-01T00:00:00Z",
    ),
    (
        "GET /api/v1/branches/{id}/analytics/buffer",
        "feature_id={{features}}&distance=10",
    ),
    ("GET /api/v1/branches/{id}/h3/hexagons", "resolution=7"),
    ("GET /api/v1/branches/{id}/h3/aggregate", "resolution=7"),
    (
        "GET /api/v1/branches/{id}/h3/neighbors",
        "cell=8928308280fffff",
    ),
    ("GET /api/v1/h3/cell", "lat=0&lng=0&resolution=7"),
    ("GET /api/v1/h3/boundary", "cell=8928308280fffff"),
    (
        "GET /api/v1/trajectories/{id}/at",
        "timestamp=2020-01-01T00:00:00Z",
    ),
    ("GET /api/v1/rasters/{id}/value", "lat=0&lng=0"),
    ("GET /api/v1/rasters/{id}/tiles", "z=0&x=0&y=0"),
    ("GET /api/v1/routes/{id}/locate", "lat=0&lng=0"),
    (
        "GET /api/v1/routes/{id}/subline",
        "from_measure=0&to_measure=1",
    ),
    ("GET /api/v1/catalog/search", "q=sweep"),
    // the tag filter is a second query in the same handler
    ("GET /api/v1/catalog/search", "q=sweep&tag=sweep"),
    ("GET /api/v1/crs/search", "q=4326"),
    (
        "GET /api/v1/comps/search",
        "branch_id={{branches}}&lat=0&lng=0",
    ),
    (
        "GET /api/v1/parcels/search",
        "branch_id={{branches}}&type=owner&q=sweep",
    ),
    ("GET /api/v1/networks/{id}/connectivity", "source=1"),
    (
        "GET /api/v1/branches/{id}/similarity/duplicates",
        "threshold=0.9",
    ),
    ("GET /api/v1/fields", "branch_id={{branches}}"),
    (
        "GET /api/v1/fields/ndvi",
        "branch_id={{branches}}&field_id={{features}}",
    ),
    ("GET /api/v1/incidents", "branch_id={{branches}}"),
    ("GET /api/v1/sensors", "branch_id={{branches}}"),
    (
        "GET /api/v1/sensors/readings",
        "branch_id={{branches}}&sensor_id={{features}}",
    ),
    ("GET /api/v1/towers", "branch_id={{branches}}"),
    (
        "GET /api/v1/construction/milestones",
        "branch_id={{branches}}",
    ),
    ("GET /api/v1/construction/surveys", "branch_id={{branches}}"),
    ("GET /api/v1/sync/pull", "branch_id={{branches}}"),
    ("GET /api/v1/sync/status", "branch_id={{branches}}"),
    ("GET /api/v1/reviews", "dataset_id={{datasets}}"),
    (
        "GET /arcgis/rest/services/{service}/FeatureServer/{layer}/queryAttachments",
        "f=json&objectIds=1",
    ),
];

/// Request bodies, keyed by `METHOD path`. `{{name}}` is replaced with the
/// fixture of that name. A route not named here is called with `{}`, which is
/// all a handler whose fields all have defaults needs. A body that is not a
/// JSON object is sent form encoded, which is how the geoservices facade takes
/// its parameters.
const BODY: &[(&str, &str)] = &[
    (
        "POST /api/v1/branches/{branch_id}/features/{feature_id}/attachments",
        r#"{"name":"sweep","data":"c3dlZXA=","created_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/extrude",
        r#"{"feature_id":"{{features}}","height":1.0}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/intersection",
        r#"{"feature_a":"{{features}}","feature_b":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/minkowski-sum",
        r#"{"feature_id":"{{features}}","buffer_geometry_wkb_hex":"0101000000000000000000F03F0000000000000040"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/straight-skeleton",
        r#"{"feature_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/tesselate",
        r#"{"feature_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/visibility",
        r#"{"observer_x":1.0,"observer_y":1.0,"observer_z":1.0,"feature_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/3d/volume",
        r#"{"feature_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/batch",
        r#"{"message":"sweep","author":"sweep","operations":[{"type":"insert","geometry_wkb_hex":"0101000000000000000000F03F0000000000000040","properties":{"name":"sweep"}}]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/commit",
        r#"{"message":"sweep","author":"sweep","operations":[{"type":"insert","geometry_wkb_hex":"0101000000000000000000F03F0000000000000040","properties":{"name":"sweep"}}]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/features/filter",
        r#"{"filter":{}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/features/intersects",
        r#"{"geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,3],[0,3],[0,0]]]}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/features/within",
        r#"{"geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,3],[0,3],[0,0]]]}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/clip",
        r#"{"clip_geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,3],[0,3],[0,0]]]}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/contour",
        r#"{"value_property":"sweep","interval":1.0}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/densify",
        r#"{"max_segment_length":1.0}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/difference",
        r#"{"subtract_geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,3],[0,3],[0,0]]]}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/dissolve",
        r#"{"group_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/distance-matrix",
        r#"{"feature_ids":["{{features}}"]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/intersect",
        r#"{"overlay_geometry":{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,3],[0,3],[0,0]]]}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/merge",
        r#"{"feature_ids":["{{features}}"]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/nearest-neighbor",
        r#"{"feature_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/simplify",
        r#"{"tolerance":1.0}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/spatial-join",
        r#"{"target_ids":["{{features}}"],"predicate":"sweep","copy_properties":["sweep"]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/geoprocessing/split",
        r#"{"feature_id":"{{features}}","split_line":{"type":"LineString","coordinates":[[0,0],[3,3]]}}"#,
    ),
    (
        "POST /api/v1/branches/{id}/h3/compact",
        r#"{"cells":["sweep"]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/import/csv",
        r#"{"csv":"sweep"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/import/geojson",
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]},"properties":{"name":"sweep"}}]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/locks",
        r#"{"feature_id":"{{features}}","locked_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/permissions",
        r#"{"user_id":"sweep","permission":"read","granted_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/branches/{id}/reproject",
        r#"{"target_srid":3857}"#,
    ),
    (
        "POST /api/v1/branches/{id}/similarity/embed",
        r#"{"fields":["sweep"]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/similarity/search",
        r#"{"embedding":[1.0]}"#,
    ),
    (
        "POST /api/v1/branches/{id}/transform",
        r#"{"from_srid":4326,"to_srid":3857,"geometry_wkb_hex":"0101000000000000000000F03F0000000000000040"}"#,
    ),
    (
        "POST /api/v1/branches/{target_id}/merge/{source_id}/resolve",
        r#"{"resolutions":[{"feature_id":"{{features}}","strategy":"sweep"}],"author":"sweep"}"#,
    ),
    (
        "POST /api/v1/coverage/simulate",
        r#"{"tower_lat":1.0,"tower_lng":1.0,"height_m":1.0,"frequency_mhz":1.0,"power_dbm":1.0}"#,
    ),
    (
        "POST /api/v1/datasets",
        r#"{"name":"sweep_{{unique}}","created_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{dataset_id}/attachments",
        r#"{"name":"sweep","data":"c3dlZXA=","created_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{dataset_id}/branches",
        r#"{"name":"sweep_{{unique}}","created_by":"sweep"}"#,
    ),
    ("PATCH /api/v1/datasets/{id}", r#"{"visibility":"public"}"#),
    (
        "POST /api/v1/datasets/{id}/attribute-rules",
        r#"{"name":"sweep","rule_type":"sweep","trigger_event":"sweep","expression":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/domains",
        r#"{"name":"sweep","domain_type":"sweep","field_type":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/events",
        r#"{"event_type":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/labels",
        r#"{"name":"sweep","field_expression":"sweep"}"#,
    ),
    (
        "PUT /api/v1/datasets/{id}/metadata",
        r#"{"description":"sweep"}"#,
    ),
    ("POST /api/v1/datasets/{id}/networks", r#"{"name":"sweep"}"#),
    (
        "POST /api/v1/datasets/{id}/permissions",
        r#"{"user_id":"sweep","permission":"read","granted_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/pointclouds",
        r#"{"name":"sweep"}"#,
    ),
    ("POST /api/v1/datasets/{id}/rasters", r#"{"name":"sweep"}"#),
    (
        "POST /api/v1/datasets/{id}/relationships",
        r#"{"name":"sweep","origin_dataset_id":"{{datasets}}","destination_dataset_id":"{{datasets}}","origin_foreign_key":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/routes",
        r#"{"name":"sweep","geometry_wkb_hex":"01020000000200000000000000000000000000000000000000000000000000f03f000000000000f03f"}"#,
    ),
    (
        "PUT /api/v1/datasets/{id}/schema",
        r#"{"fields":[{"name":"sweep","field_type":"string","required":false}]}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/schema/migrations",
        r#"{"description":"sweep","migration_type":"sweep","applied_by":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/subtypes",
        r#"{"subtype_field":"sweep","name":"sweep","code":1}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/symbology",
        r#"{"name":"sweep","symbol":{}}"#,
    ),
    ("POST /api/v1/datasets/{id}/tags", r#"{"tag":"sweep"}"#),
    (
        "POST /api/v1/datasets/{id}/topologies",
        r#"{"name":"sweep_{{unique}}","srid":4326}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/topology",
        r#"{"rule_type":"must_not_overlap","name":"sweep"}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/trajectories",
        r#"{"name":"sweep","points":[{"lng":1.0,"lat":1.0,"timestamp":"2020-01-01T00:00:00Z"},{"lng":2.0,"lat":2.0,"timestamp":"2020-01-01T01:00:00Z"}]}"#,
    ),
    (
        "POST /api/v1/datasets/{id}/trajectories/nearest",
        r#"{"trajectory_a":"{{features}}","trajectory_b":"{{features}}"}"#,
    ),
    ("POST /api/v1/datasets/{id}/webhooks", r#"{"url":"sweep"}"#),
    (
        "POST /api/v1/incidents",
        r#"{"branch_id":"{{branches}}","incident_type":"sweep","severity":"sweep","lat":1.0,"lng":1.0,"description":"sweep","author":"sweep"}"#,
    ),
    (
        "POST /api/v1/incidents/evacuate",
        r#"{"incident_lat":1.0,"incident_lng":1.0,"radius_m":1.0,"assembly_points":[{"id":"sweep","lat":1.0,"lng":1.0,"capacity":1}]}"#,
    ),
    (
        "POST /api/v1/networks/{id}/astar",
        r#"{"from_junction":"{{features}}","to_junction":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/networks/{id}/edges",
        r#"{"feature_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/networks/{id}/isochrone",
        r#"{"start_junction":"{{features}}","max_cost":1.0}"#,
    ),
    (
        "POST /api/v1/networks/{id}/junctions",
        r#"{"lng":1.0,"lat":1.0}"#,
    ),
    (
        "POST /api/v1/networks/{id}/shortest-path",
        r#"{"from_junction":"{{features}}","to_junction":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/networks/{id}/trace",
        r#"{"start_junction":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/networks/{id}/tsp",
        r#"{"junction_ids":["{{features}}"]}"#,
    ),
    (
        "POST /api/v1/parcels/merge",
        r#"{"branch_id":"{{branches}}","feature_ids":["{{features}}"],"author":"sweep"}"#,
    ),
    (
        "POST /api/v1/parcels/split",
        r#"{"branch_id":"{{branches}}","feature_id":"{{features}}","line":[[0,0],[1,1]],"author":"sweep"}"#,
    ),
    (
        "POST /api/v1/pointclouds/{id}/patches",
        r#"{"bounds_wkb_hex":"0101000000000000000000F03F0000000000000040","num_points":1,"patch_hex":"00"}"#,
    ),
    (
        "POST /api/v1/pointclouds/{id}/profile",
        r#"{"line_wkb_hex":"0101000000000000000000F03F0000000000000040"}"#,
    ),
    (
        "POST /api/v1/pointclouds/{id}/query",
        r#"{"min_x":1.0,"min_y":1.0,"max_x":1.0,"max_y":1.0}"#,
    ),
    (
        "POST /api/v1/qgis/branches/{branch_id}/conflicts/resolve",
        r#"{"conflict_id":"{{features}}","resolution":"sweep"}"#,
    ),
    (
        "POST /api/v1/qgis/branches/{branch_id}/sync",
        r#"{"geojson":{},"author":"sweep"}"#,
    ),
    (
        "POST /api/v1/qgis/branches/{branch_id}/transaction",
        r#"{"author":"sweep","operations":[{"action":"insert","geometry_wkb_hex":"0101000000000000000000F03F0000000000000040","properties":{}}]}"#,
    ),
    (
        "POST /api/v1/rasters/{id}/tiles",
        r#"{"zoom_level":1,"bounds_wkb_hex":"0101000000000000000000F03F0000000000000040","rast_hex":"00"}"#,
    ),
    (
        "POST /api/v1/relationship-classes/{id}/records",
        r#"{"origin_feature_id":"{{features}}","destination_feature_id":"{{features}}"}"#,
    ),
    ("POST /api/v1/replication/peers", r#"{"name":"sweep"}"#),
    (
        "POST /api/v1/replication/peers/{id}/sync",
        r#"{"changeset_id":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/reviews",
        r#"{"dataset_id":"{{datasets}}","source_branch_id":"{{other_branches}}","target_branch_id":"{{branches}}","title":"sweep","author":"sweep"}"#,
    ),
    (
        "POST /api/v1/reviews/{id}/comments",
        r#"{"author":"sweep","body":"sweep"}"#,
    ),
    ("POST /api/v1/reviews/{id}/merge", r#"{"author":"sweep"}"#),
    (
        "POST /api/v1/routes/{id}/events",
        r#"{"event_type":"sweep","from_measure":1.0}"#,
    ),
    (
        "POST /api/v1/surveys/compare",
        r#"{"branch_id":"{{branches}}","survey_a":"{{features}}","survey_b":"{{features}}"}"#,
    ),
    (
        "POST /api/v1/sync/push",
        r#"{"branch_id":"{{branches}}","message":"sweep","author":"sweep","operations":[{"type":"insert","feature_id":"{{unique}}","geometry_wkb_hex":"0101000000000000000000F03F0000000000000040","properties":{}}]}"#,
    ),
    (
        "POST /api/v1/topologies/{name}/add-face",
        r#"{"geometry_wkb_hex":"010300000001000000050000000000000000000000000000000000000000000000000008400000000000000000000000000000084000000000000008400000000000000000000000000000084000000000000000000000000000000000"}"#,
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/{layer}/query",
        "f=json&where=1%3D1&outFields=*",
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/{layer}/queryAttachments",
        "f=json&objectIds=1",
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/{layer}/applyEdits",
        "f=json&adds=%5B%5D",
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/extractChanges",
        "f=json&layers=0&returnInserts=true",
    ),
    (
        "POST /arcgis/rest/services/{service}/FeatureServer/{layer}/{oid}/deleteAttachments",
        "f=json&attachmentIds=1",
    ),
];

// ═══════════════════════════════════════════════════════════════════════
// Reading the route list off the router
// ═══════════════════════════════════════════════════════════════════════

/// The routes the app answers, as `(method, path)`.
///
/// axum 0.8 has no public way to walk a `Router`, but its `Debug` carries the
/// path table (`path_router.node.paths`) and the methods each path answers
/// (`routes.*.allow_header`), both keyed by route id. Parsing that is ugly and
/// it is the only way to derive the list, and deriving it is the point: a route
/// added tomorrow is swept without anyone remembering to add it here. If an
/// axum upgrade changes the shape, the asserts below fail rather than the sweep
/// quietly covering nothing.
fn mounted_routes(app: &axum::Router) -> Vec<(String, String)> {
    let dump = format!("{app:?}");
    let head = &dump[..find(&dump, "fallback_router:")];
    let split = find(head, "node: Node");
    let (methods_section, paths_section) = head.split_at(split);

    let mut methods: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (id, text) in route_id_entries(methods_section) {
        // an endpoint mounted as a service carries no method table: assume GET
        let allow = between(text, "allow_header: Bytes(b\"", "\"").unwrap_or("GET");
        methods.insert(
            id,
            allow
                .split(',')
                .filter(|m| *m != "HEAD")
                .map(str::to_string)
                .collect(),
        );
    }

    let mut routes = Vec::new();
    for (id, text) in route_id_entries(paths_section) {
        let path = between(text, "\"", "\"").expect("a path in the debug dump");
        for method in methods.get(&id).expect("every path has an endpoint") {
            routes.push((method.clone(), path.to_string()));
        }
    }
    routes.sort();

    assert!(
        routes.len() > 200,
        "parsed only {} routes off the router, so the debug shape changed",
        routes.len()
    );
    for known in [
        ("GET", "/api/v1/datasets"),
        ("POST", "/api/v1/datasets"),
        ("PATCH", "/api/v1/datasets/{id}"),
        ("DELETE", "/api/v1/webhooks/{id}"),
    ] {
        let known = (known.0.to_string(), known.1.to_string());
        assert!(
            routes.contains(&known),
            "{known:?} not in the parsed routes"
        );
    }
    routes
}

/// `RouteId(7): <text>` entries, each with the text up to the next one.
fn route_id_entries(section: &str) -> Vec<(u32, &str)> {
    let mut starts = Vec::new();
    let mut at = 0;
    while let Some(next) = section[at..].find("RouteId(") {
        starts.push(at + next);
        at += next + 1;
    }
    let mut out = Vec::new();
    for (i, start) in starts.iter().enumerate() {
        let entry = &section[*start..starts.get(i + 1).copied().unwrap_or(section.len())];
        if let Ok(id) = between(entry, "RouteId(", ")").unwrap_or("").parse::<u32>() {
            out.push((id, entry));
        }
    }
    out
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let at = text.find(start)? + start.len();
    let rest = &text[at..];
    Some(&rest[..rest.find(end)?])
}

fn find(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} missing from the router debug dump"))
}

// ═══════════════════════════════════════════════════════════════════════
// Watching the log for the failure this sweep exists to catch
// ═══════════════════════════════════════════════════════════════════════

#[derive(Clone, Default)]
struct SchemaErrors(Arc<Mutex<Vec<String>>>);

impl SchemaErrors {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

struct Capture(SchemaErrors);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut seen = Fields::default();
        event.record(&mut seen);
        if let Some(code) = seen.sqlstate {
            self.0
                .0
                .lock()
                .unwrap()
                .push(format!("{code} {}", seen.message));
        }
    }
}

#[derive(Default)]
struct Fields {
    sqlstate: Option<String>,
    message: String,
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "sqlstate" {
            self.sqlstate = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        match field.name() {
            "sqlstate" => self.sqlstate = Some(text.trim_matches('"').to_string()),
            "message" => self.message = text,
            _ => {}
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// The sweep
// ═══════════════════════════════════════════════════════════════════════

/// Reset the database and return the app, the way api_integration.rs does.
async fn setup() -> (axum::Router, AppState) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ptolemy_test".to_string());
    let pool = PgPool::connect(&url).await.expect("DB connect failed");
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS conflicts CASCADE;
         DROP TABLE IF EXISTS attachments CASCADE;
         DROP TABLE IF EXISTS feature_versions CASCADE;
         DROP TABLE IF EXISTS changesets CASCADE;
         DROP TABLE IF EXISTS branches CASCADE;
         DROP TABLE IF EXISTS raster_tiles CASCADE;
         DROP TABLE IF EXISTS raster_catalogs CASCADE;
         DROP TABLE IF EXISTS pointcloud_patches CASCADE;
         DROP TABLE IF EXISTS pointcloud_catalogs CASCADE;
         DROP TABLE IF EXISTS datasets CASCADE;
         DROP TABLE IF EXISTS dataset_metadata CASCADE;
         DROP TABLE IF EXISTS dataset_tags CASCADE;
         DROP TABLE IF EXISTS _sqlx_migrations CASCADE;
         DO $$
         DECLARE t text;
         BEGIN
             FOR t IN SELECT name FROM topology.topology WHERE name LIKE 'sweep%' LOOP
                 PERFORM topology.DropTopology(t);
             END LOOP;
         EXCEPTION WHEN OTHERS THEN NULL;
         END $$;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = PgStore::new(pool);
    store.migrate().await.unwrap();
    let state = Arc::new(store);
    let app = app_with_auth(state.clone(), AuthConfig::disabled());
    (app, state)
}

struct Sweep {
    app: axum::Router,
    ids: BTreeMap<String, String>,
    /// fixtures the seed created, which a create route may not overwrite
    sealed: BTreeSet<String>,
    errors: SchemaErrors,
    /// route -> what the sweep saw, for the coverage report
    outcomes: BTreeMap<String, String>,
    /// routes an extractor refused, which means the handler never ran
    rejected: Vec<String>,
    /// routes whose query blew up on the schema
    broken: Vec<String>,
    /// every other database error, reported but not failed on
    other_db_errors: Vec<String>,
}

impl Sweep {
    /// Call a route once per query and body variant it has.
    async fn call(&mut self, method: &str, path: &str) {
        let key = format!("{method} {path}");
        for query in variants(QUERY, &key, "") {
            for body in variants(BODY, &key, "{}") {
                self.request(method, path, query, body).await;
            }
        }
    }

    /// Fill a path template and call it.
    async fn request(&mut self, method: &str, path: &str, query: &str, body: &str) {
        let key = format!("{method} {path}");
        let mut uri = self.fill(path);
        if !query.is_empty() {
            uri = format!("{uri}?{}", self.substitute(query));
        }
        let body = self.substitute(body);
        let content_type = if body.starts_with('{') {
            "application/json"
        } else {
            "application/x-www-form-urlencoded"
        };

        let req = Request::builder()
            .method(method)
            .uri(&uri)
            .header("content-type", content_type)
            // ignored: the app is built with auth disabled
            .header("authorization", "Bearer sweep")
            .body(Body::from(body))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let text = String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes())
            .to_string();

        for error in self.errors.take() {
            let line = format!("{key}\n    {error}");
            if error.starts_with(UNDEFINED_COLUMN) || error.starts_with(UNDEFINED_TABLE) {
                self.broken.push(line);
            } else {
                self.other_db_errors.push(line);
            }
        }
        if extractor_refused(status, &text) {
            self.rejected
                .push(format!("{key}\n    {status} {}", first_line(&text)));
        }
        self.learn(method, path, &text);
        // a route with more than one variant reports the worst answer it gave
        let outcome = describe(status, &text);
        match self.outcomes.get(&key) {
            Some(seen) if seen.as_str() >= outcome.as_str() => {}
            _ => {
                self.outcomes.insert(key, outcome);
            }
        }
    }

    /// Register what a create returned, so a later route can name it.
    fn learn(&mut self, method: &str, path: &str, body: &str) {
        if method != "POST" || !path.starts_with("/api/v1") {
            return;
        }
        let Some(kind) = path.rsplit('/').find(|s| !s.starts_with('{')) else {
            return;
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) else {
            return;
        };
        let Some(id) = map.get("id").and_then(id_text) else {
            return;
        };
        let kind = lookup(LEARN_AS, kind).unwrap_or(kind);
        if !self.sealed.contains(kind) {
            self.ids.insert(kind.to_string(), id);
        }
    }

    fn fill(&self, path: &str) -> String {
        let segments: Vec<&str> = path.split('/').skip(1).collect();
        let mut out = String::new();
        let mut prefix = String::new();
        for (i, segment) in segments.iter().enumerate() {
            prefix.push('/');
            prefix.push_str(segment);
            out.push('/');
            match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                None => out.push_str(segment),
                Some(param) => {
                    let before = if i > 0 { segments[i - 1] } else { "" };
                    let key = lookup(PARAM_BY_PATH, &prefix)
                        .or_else(|| lookup(PARAM_BY_NAME, param))
                        .or_else(|| self.ids.contains_key(param).then_some(param))
                        .unwrap_or(before);
                    out.push_str(self.ids.get(key).map(String::as_str).unwrap_or(ABSENT));
                }
            }
        }
        out
    }

    fn substitute(&self, text: &str) -> String {
        // a name with a unique constraint on it, so the second pass is not a
        // duplicate of the first
        let mut out = text.replace("{{unique}}", &Uuid::now_v7().simple().to_string());
        for (key, value) in &self.ids {
            out = out.replace(&format!("{{{{{key}}}}}"), value);
        }
        out
    }
}

/// The topology the sweep creates, since a topology route names a schema and
/// postgis makes one per topology.
const TOPOLOGY: &str = "sweep_topology";

/// An id no row has, for a path parameter no fixture covers. The route answers
/// 404, which is a pass: the query still ran.
const ABSENT: &str = "00000000-0000-7000-8000-000000000000";

fn id_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn lookup<'a>(table: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Every entry a route has in a table, or `fallback` where it has none. More
/// than one entry means more than one call, which is how a handler that runs a
/// different query per parameter gets both of them run.
fn variants<'a>(table: &'a [(&'a str, &'a str)], key: &str, fallback: &'a str) -> Vec<&'a str> {
    let found: Vec<&str> = table
        .iter()
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .collect();
    if found.is_empty() {
        vec![fallback]
    } else {
        found
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

/// Did an extractor refuse the request before the handler ran? Then the sweep
/// proved nothing about that route.
fn extractor_refused(status: StatusCode, body: &str) -> bool {
    status == StatusCode::UNSUPPORTED_MEDIA_TYPE
        || (matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ) && (body.contains("Failed to deserialize")
            || body.contains("Failed to parse")
            || body.contains("Expected request with `Content-Type")))
}

/// The arcgis facade answers 200 with the failure in the body, so the status
/// alone does not say what happened.
fn describe(status: StatusCode, body: &str) -> String {
    let esri = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["code"].as_u64());
    match esri {
        Some(code) => format!("{status} (esri {code})"),
        None => status.to_string(),
    }
}

#[tokio::test]
async fn every_mounted_route_runs_against_the_schema() {
    let errors = SchemaErrors::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Capture(errors.clone())),
    )
    .expect("no other subscriber in this test binary");

    let (app, _state) = setup().await;
    let routes = mounted_routes(&app);

    let mut sweep = Sweep {
        app,
        ids: BTreeMap::new(),
        sealed: BTreeSet::new(),
        errors,
        outcomes: BTreeMap::new(),
        rejected: Vec::new(),
        broken: Vec::new(),
        other_db_errors: Vec::new(),
    };
    seed(&mut sweep).await;

    // writes first so a read has something to read, deletes last so they do not
    // take it away, and the whole thing twice so a create in the first pass
    // fills a path parameter in the second
    let mut ordered: Vec<(String, String)> = routes
        .iter()
        .filter(|(m, p)| lookup(SKIPPED, &format!("{m} {p}")).is_none())
        .cloned()
        .collect();
    ordered.sort_by_key(|(m, p)| {
        let rank = match m.as_str() {
            "DELETE" => 2,
            "GET" => 1,
            _ => 0,
        };
        (rank, p.split('/').count(), p.clone(), m.clone())
    });

    for _pass in 0..2 {
        for (method, path) in &ordered {
            sweep.call(method, path).await;
        }
    }

    report(&sweep, &routes);

    assert!(
        sweep.broken.is_empty(),
        "a handler named a column or a table the migrations do not create:\n{}",
        sweep.broken.join("\n")
    );
    assert!(
        sweep.rejected.is_empty(),
        "an extractor refused these, so the handler never ran and the sweep \
         proved nothing. Give each one a body in BODY or a query in QUERY, or a \
         line in SKIPPED:\n{}",
        sweep.rejected.join("\n")
    );
}

/// The objects most routes hang off: everything else the sweep learns as it
/// goes, from the create routes it calls.
async fn seed(sweep: &mut Sweep) {
    for (key, value) in [
        ("z", "0"),
        ("x", "0"),
        ("y", "0"),
        ("tms", "WebMercatorQuad"),
        ("srid", "4326"),
        ("tag", "sweep"),
        ("layer", "0"),
        ("oid", "1"),
        ("attachmentId", "1"),
        ("user_id", "sweep-user"),
        ("name", TOPOLOGY),
        ("jobId", ABSENT),
    ] {
        sweep.ids.insert(key.to_string(), value.to_string());
    }

    let name = format!("sweep_{}", Uuid::now_v7().simple());
    let dataset = post(
        &sweep.app,
        "/api/v1/datasets",
        &serde_json::json!({"name": name, "geometry_type": "point", "srid": 4326, "created_by": "sweep"}),
    )
    .await;
    sweep.ids.insert(
        "datasets".into(),
        dataset["id"].as_str().expect("dataset id").into(),
    );
    // the arcgis facade addresses a dataset by name
    sweep.ids.insert("service".into(), name);

    for (key, branch) in [("branches", "main"), ("other_branches", "sweep")] {
        let created = post(
            &sweep.app,
            &format!("/api/v1/datasets/{}/branches", sweep.ids["datasets"]),
            &serde_json::json!({"name": branch, "created_by": "sweep"}),
        )
        .await;
        sweep.ids.insert(
            key.into(),
            created["id"].as_str().expect("branch id").into(),
        );
    }

    // one committed feature, so a read has a row to find
    let feature = Uuid::now_v7();
    post(
        &sweep.app,
        &format!("/api/v1/branches/{}/commit", sweep.ids["branches"]),
        &serde_json::json!({
            "message": "sweep fixture",
            "author": "sweep",
            "operations": [{
                "type": "insert",
                "feature_id": feature.to_string(),
                // POINT(1 2), little-endian wkb
                "geometry_wkb_hex": "0101000000000000000000F03F0000000000000040",
                "properties": {"name": "sweep", "pop": 1},
            }],
        }),
    )
    .await;
    sweep.ids.insert("features".into(), feature.to_string());

    // the topology routes take a schema name, and the postgis topology tables
    // only exist once something has created one
    post(
        &sweep.app,
        &format!("/api/v1/datasets/{}/topologies", sweep.ids["datasets"]),
        &serde_json::json!({"name": TOPOLOGY, "srid": 4326}),
    )
    .await;

    // a create route registers what it made under the same word, and the seeded
    // objects are the ones every other route hangs off: keep them
    sweep.sealed = sweep.ids.keys().cloned().collect();
}

async fn post(app: &axum::Router, uri: &str, body: &Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", "Bearer sweep")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        status.is_success(),
        "fixture {uri} answered {status}: {}",
        first_line(&text)
    );
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// What the sweep covered, printed so the run is readable rather than trusted.
fn report(sweep: &Sweep, routes: &[(String, String)]) {
    let mut by_outcome: BTreeMap<&str, usize> = BTreeMap::new();
    for outcome in sweep.outcomes.values() {
        *by_outcome.entry(outcome.as_str()).or_default() += 1;
    }
    println!("\nroute sweep: {} routes mounted", routes.len());
    println!("  called   {}", sweep.outcomes.len());
    println!("  skipped  {}", SKIPPED.len());
    for (outcome, count) in &by_outcome {
        println!("  {count:>4}  {outcome}");
    }
    let failures: Vec<&String> = sweep
        .outcomes
        .iter()
        .filter(|(_, o)| o.starts_with("500") || o.contains("esri 500"))
        .map(|(route, _)| route)
        .collect();
    if !failures.is_empty() {
        println!("\n500s ({}):", failures.len());
        for route in failures {
            println!("  {route}  {}", sweep.outcomes[route]);
        }
    }
    if !sweep.rejected.is_empty() {
        println!("\nrefused by an extractor ({}):", sweep.rejected.len());
        for line in &sweep.rejected {
            println!("  {line}");
        }
    }
    if !sweep.other_db_errors.is_empty() {
        println!("\nother database errors ({}):", sweep.other_db_errors.len());
        for line in &sweep.other_db_errors {
            println!("  {line}");
        }
    }
    for (route, reason) in SKIPPED {
        println!("skipped {route}: {reason}");
    }
}
