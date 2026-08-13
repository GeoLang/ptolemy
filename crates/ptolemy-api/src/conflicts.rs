// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Programmatic conflict resolution API with visual GeoJSON support.
//!
//! When a merge produces conflicts, clients can use this API to:
//! 1. Preview a merge and see conflicts as a GeoJSON FeatureCollection
//! 2. List pending conflicts with ours/theirs/base geometries
//! 3. Resolve conflicts by choosing ours/theirs/custom/auto-merge
//! 4. Finalize the merge after all conflicts are resolved

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use ptolemy_core::diff::DiffOp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, auth::Actor};

pub fn conflict_routes() -> Router<AppState> {
    Router::new()
        .route("/conflicts/{merge_id}", get(list_conflicts))
        // Visual merge preview: returns GeoJSON with ours/theirs/base for rendering
        .route(
            "/branches/{target_id}/merge/{source_id}/preview",
            get(preview_merge_visual),
        )
        // Resolve-and-merge: apply resolutions and create merge commit
        .route(
            "/branches/{target_id}/merge/{source_id}/resolve",
            post(resolve_and_merge),
        )
}

#[derive(Serialize)]
struct ConflictDetail {
    feature_id: Uuid,
    field: Option<String>,
    ours: Option<serde_json::Value>,
    theirs: Option<serde_json::Value>,
    base: Option<serde_json::Value>,
}

async fn list_conflicts(
    State(store): State<AppState>,
    Path(merge_id): Path<Uuid>,
) -> Result<Json<Vec<ConflictDetail>>, ConflictError> {
    // merge_id corresponds to the source branch ID in a failed merge.
    // Look up features that differ between source and target.
    let rows = sqlx::query(
        "WITH source_latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id, fv.properties, fv.geometry
            FROM feature_versions fv
            JOIN changesets c ON c.id = fv.changeset_id
            WHERE c.branch_id = $1
            ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
        ),
        target_branch AS (
            SELECT b.id FROM branches b
            WHERE b.dataset_id = (SELECT dataset_id FROM branches WHERE id = $1)
              AND b.name = 'main'
            LIMIT 1
        ),
        target_latest AS (
            SELECT DISTINCT ON (fv.feature_id) fv.feature_id, fv.properties, fv.geometry
            FROM feature_versions fv
            JOIN changesets c ON c.id = fv.changeset_id
            JOIN target_branch tb ON c.branch_id = tb.id
            ORDER BY fv.feature_id, fv.created_at DESC, fv.id DESC
        )
        SELECT
            s.feature_id,
            s.properties as ours,
            t.properties as theirs
        FROM source_latest s
        JOIN target_latest t ON s.feature_id = t.feature_id
        WHERE s.properties IS DISTINCT FROM t.properties
           OR ST_AsBinary(s.geometry) IS DISTINCT FROM ST_AsBinary(t.geometry)",
    )
    .bind(merge_id)
    .fetch_all(store.read_pool())
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| ConflictDetail {
                feature_id: row.get("feature_id"),
                field: None,
                ours: row.get("ours"),
                theirs: row.get("theirs"),
                base: None,
            })
            .collect(),
    ))
}

// ─── Visual Merge Preview ───────────────────────────────────────────

/// Returns a GeoJSON FeatureCollection showing all conflicting features
/// with `side` property ("ours", "theirs", "base") for visual rendering
/// in map UIs.
#[derive(Serialize)]
struct MergePreview {
    source_branch_id: Uuid,
    target_branch_id: Uuid,
    auto_mergeable: usize,
    conflict_count: usize,
    /// GeoJSON FeatureCollection with all conflict geometries
    conflicts_geojson: serde_json::Value,
    /// Structured conflict details
    conflicts: Vec<VisualConflict>,
    suggestion_summary: SuggestionSummary,
}

#[derive(Serialize)]
struct VisualConflict {
    feature_id: Uuid,
    conflict_type: &'static str,
    ours_geometry: Option<serde_json::Value>,
    theirs_geometry: Option<serde_json::Value>,
    base_geometry: Option<serde_json::Value>,
    ours_properties: Option<serde_json::Value>,
    theirs_properties: Option<serde_json::Value>,
    base_properties: Option<serde_json::Value>,
    suggestion: &'static str,
}

#[derive(Serialize)]
struct SuggestionSummary {
    auto_merge_possible: usize,
    manual_required: usize,
    delete_modify: usize,
}

async fn preview_merge_visual(
    State(store): State<AppState>,
    Path((target_id, source_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<MergePreview>, ConflictError> {
    let source = store.get_branch(source_id).await?;
    let target = store.get_branch(target_id).await?;

    let source_head =
        source
            .head
            .ok_or(ConflictError::Store(ptolemy_storage::StoreError::NotFound(
                "source has no commits".into(),
            )))?;
    let target_head =
        target
            .head
            .ok_or(ConflictError::Store(ptolemy_storage::StoreError::NotFound(
                "target has no commits".into(),
            )))?;

    let base = store.find_merge_base(source_head, target_head).await?;

    let diff_ours = store.diff(base, target_head).await?;
    let diff_theirs = store.diff(base, source_head).await?;

    let ours_map: std::collections::HashMap<Uuid, &DiffOp> = diff_ours
        .operations
        .iter()
        .map(|op| (diff_op_fid(op), op))
        .collect();
    let theirs_map: std::collections::HashMap<Uuid, &DiffOp> = diff_theirs
        .operations
        .iter()
        .map(|op| (diff_op_fid(op), op))
        .collect();

    let all_fids: std::collections::HashSet<Uuid> =
        ours_map.keys().chain(theirs_map.keys()).copied().collect();

    let mut auto_mergeable = 0usize;
    let mut conflicts: Vec<VisualConflict> = Vec::new();
    let mut geojson_features: Vec<serde_json::Value> = Vec::new();

    for fid in &all_fids {
        match (ours_map.get(fid), theirs_map.get(fid)) {
            (Some(ours), None) | (None, Some(ours)) => {
                auto_mergeable += 1;
                // No conflict - auto merge
                let _ = ours;
            }
            (Some(ours), Some(theirs)) => {
                if diff_ops_match(ours, theirs) {
                    auto_mergeable += 1;
                } else {
                    // This is a conflict - get visual data
                    let ours_geom = get_op_geojson(&store, ours).await;
                    let theirs_geom = get_op_geojson(&store, theirs).await;
                    let base_geom = get_feature_base_geojson(&store, *fid).await;

                    // Determine conflict type and suggestion
                    let is_delete = matches!(ours, DiffOp::Delete { .. })
                        || matches!(theirs, DiffOp::Delete { .. });
                    let geom_differs = ours_geom != theirs_geom;
                    let props_differ = get_op_props(ours) != get_op_props(theirs);

                    let (conflict_type, suggestion) = if is_delete {
                        ("delete_modify", "manual_required")
                    } else if geom_differs && props_differ {
                        ("full_conflict", "manual_required")
                    } else if geom_differs {
                        ("geometry_conflict", "manual_required")
                    } else {
                        ("property_conflict", "auto_merge_properties")
                    };

                    // Add to GeoJSON FeatureCollection for map rendering
                    if let Some(ref g) = ours_geom {
                        geojson_features.push(serde_json::json!({
                            "type": "Feature",
                            "id": format!("{fid}-ours"),
                            "geometry": g,
                            "properties": {"feature_id": fid, "side": "ours", "conflict_type": conflict_type}
                        }));
                    }
                    if let Some(ref g) = theirs_geom {
                        geojson_features.push(serde_json::json!({
                            "type": "Feature",
                            "id": format!("{fid}-theirs"),
                            "geometry": g,
                            "properties": {"feature_id": fid, "side": "theirs", "conflict_type": conflict_type}
                        }));
                    }
                    if let Some(ref g) = base_geom {
                        geojson_features.push(serde_json::json!({
                            "type": "Feature",
                            "id": format!("{fid}-base"),
                            "geometry": g,
                            "properties": {"feature_id": fid, "side": "base", "conflict_type": conflict_type}
                        }));
                    }

                    conflicts.push(VisualConflict {
                        feature_id: *fid,
                        conflict_type,
                        ours_geometry: ours_geom,
                        theirs_geometry: theirs_geom,
                        base_geometry: base_geom,
                        ours_properties: Some(get_op_props(ours)),
                        theirs_properties: Some(get_op_props(theirs)),
                        base_properties: None,
                        suggestion,
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }

    let conflict_count = conflicts.len();
    let auto_merge_count = conflicts
        .iter()
        .filter(|c| c.suggestion == "auto_merge_properties")
        .count();
    let delete_modify_count = conflicts
        .iter()
        .filter(|c| c.conflict_type == "delete_modify")
        .count();

    Ok(Json(MergePreview {
        source_branch_id: source_id,
        target_branch_id: target_id,
        auto_mergeable,
        conflict_count,
        conflicts_geojson: serde_json::json!({
            "type": "FeatureCollection",
            "features": geojson_features,
        }),
        conflicts,
        suggestion_summary: SuggestionSummary {
            auto_merge_possible: auto_merge_count,
            manual_required: conflict_count - auto_merge_count - delete_modify_count,
            delete_modify: delete_modify_count,
        },
    }))
}

// ─── Resolve and Merge ──────────────────────────────────────────────

#[derive(Deserialize)]
struct VisualResolveRequest {
    resolutions: Vec<VisualResolution>,
    author: String,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct VisualResolution {
    feature_id: Uuid,
    /// "ours", "theirs", "custom", "delete", "auto_merge"
    strategy: String,
    #[serde(default)]
    custom_geometry_wkb_hex: Option<String>,
    #[serde(default)]
    custom_properties: Option<serde_json::Value>,
}

async fn resolve_and_merge(
    State(store): State<AppState>,
    Path((target_id, source_id)): Path<(Uuid, Uuid)>,
    actor: Actor,
    Json(req): Json<VisualResolveRequest>,
) -> Result<Json<serde_json::Value>, ConflictError> {
    let source = store.get_branch(source_id).await?;
    let target = store.get_branch(target_id).await?;

    let source_head =
        source
            .head
            .ok_or(ConflictError::Store(ptolemy_storage::StoreError::NotFound(
                "source has no commits".into(),
            )))?;
    let target_head =
        target
            .head
            .ok_or(ConflictError::Store(ptolemy_storage::StoreError::NotFound(
                "target has no commits".into(),
            )))?;

    let base = store.find_merge_base(source_head, target_head).await?;
    let diff_ours = store.diff(base, target_head).await?;
    let diff_theirs = store.diff(base, source_head).await?;

    let ours_map: std::collections::HashMap<Uuid, &DiffOp> = diff_ours
        .operations
        .iter()
        .map(|op| (diff_op_fid(op), op))
        .collect();
    let theirs_map: std::collections::HashMap<Uuid, &DiffOp> = diff_theirs
        .operations
        .iter()
        .map(|op| (diff_op_fid(op), op))
        .collect();

    let all_fids: std::collections::HashSet<Uuid> =
        ours_map.keys().chain(theirs_map.keys()).copied().collect();

    let resolution_map: std::collections::HashMap<Uuid, &VisualResolution> =
        req.resolutions.iter().map(|r| (r.feature_id, r)).collect();

    let mut final_ops: Vec<DiffOp> = Vec::new();

    for fid in &all_fids {
        match (ours_map.get(fid), theirs_map.get(fid)) {
            (Some(ours), None) => final_ops.push((*ours).clone()),
            (None, Some(theirs)) => final_ops.push((*theirs).clone()),
            (Some(ours), Some(theirs)) => {
                if diff_ops_match(ours, theirs) {
                    final_ops.push((*ours).clone());
                } else if let Some(res) = resolution_map.get(fid) {
                    match res.strategy.as_str() {
                        "ours" => final_ops.push((*ours).clone()),
                        "theirs" => final_ops.push((*theirs).clone()),
                        "delete" => final_ops.push(DiffOp::Delete { feature_id: *fid }),
                        "auto_merge" => {
                            // Take geometry from theirs, merge properties
                            let geom = op_wkb(theirs).map(|w| w.to_vec());
                            let mut merged_props = get_op_props(ours);
                            if let Some(theirs_obj) = get_op_props(theirs).as_object()
                                && let Some(ours_obj) = merged_props.as_object_mut()
                            {
                                for (k, v) in theirs_obj {
                                    ours_obj.entry(k.clone()).or_insert_with(|| v.clone());
                                }
                            }
                            final_ops.push(DiffOp::Update {
                                feature_id: *fid,
                                geometry_wkb: geom,
                                properties: Some(merged_props),
                                // the geometry is theirs wholesale, so its original is too
                                native: op_native(theirs).cloned(),
                                valid_from: None,
                                valid_to: None,
                            });
                        }
                        "custom" => {
                            let wkb = res
                                .custom_geometry_wkb_hex
                                .as_ref()
                                .and_then(|h| hex::decode(h).ok());
                            final_ops.push(DiffOp::Update {
                                feature_id: *fid,
                                geometry_wkb: wkb,
                                properties: res.custom_properties.clone(),
                                // a hand-resolved geometry has no survey original
                                native: None,
                                valid_from: None,
                                valid_to: None,
                            });
                        }
                        _ => {
                            // Default: take ours
                            final_ops.push((*ours).clone());
                        }
                    }
                } else {
                    // No resolution provided — fail
                    return Ok(Json(serde_json::json!({
                        "error": format!("no resolution for conflicting feature {fid}"),
                        "success": false,
                    })));
                }
            }
            (None, None) => unreachable!(),
        }
    }

    let message = req.message.unwrap_or_else(|| {
        format!(
            "Merge '{}' into '{}' (conflicts resolved)",
            source.name, target.name
        )
    });

    let changeset = store
        .commit_merge(
            target_id,
            &message,
            actor.or_body(&req.author),
            &final_ops,
            Some(source_head),
            &actor.writer(),
        )
        .await?;

    Ok(Json(serde_json::json!({
        "success": true,
        "changeset_id": changeset.id,
        "resolved_count": req.resolutions.len(),
        "total_operations": final_ops.len(),
    })))
}

// ─── Helpers ────────────────────────────────────────────────────────

fn diff_op_fid(op: &DiffOp) -> Uuid {
    match op {
        DiffOp::Insert { feature_id, .. }
        | DiffOp::Update { feature_id, .. }
        | DiffOp::Delete { feature_id } => *feature_id,
    }
}

fn diff_ops_match(a: &DiffOp, b: &DiffOp) -> bool {
    match (a, b) {
        (
            DiffOp::Insert {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
                native: na,
                valid_from: vfa,
                valid_to: vta,
            },
            DiffOp::Insert {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
                native: nb,
                valid_from: vfb,
                valid_to: vtb,
            },
        ) => fa == fb && ga == gb && pa == pb && na == nb && vfa == vfb && vta == vtb,
        (
            DiffOp::Update {
                feature_id: fa,
                geometry_wkb: ga,
                properties: pa,
                native: na,
                valid_from: vfa,
                valid_to: vta,
            },
            DiffOp::Update {
                feature_id: fb,
                geometry_wkb: gb,
                properties: pb,
                native: nb,
                valid_from: vfb,
                valid_to: vtb,
            },
        ) => fa == fb && ga == gb && pa == pb && na == nb && vfa == vfb && vta == vtb,
        (DiffOp::Delete { feature_id: fa }, DiffOp::Delete { feature_id: fb }) => fa == fb,
        _ => false,
    }
}

fn op_wkb(op: &DiffOp) -> Option<&[u8]> {
    match op {
        DiffOp::Insert { geometry_wkb, .. } => Some(geometry_wkb),
        DiffOp::Update { geometry_wkb, .. } => geometry_wkb.as_deref(),
        DiffOp::Delete { .. } => None,
    }
}

fn op_native(op: &DiffOp) -> Option<&ptolemy_core::NativeGeometry> {
    match op {
        DiffOp::Insert { native, .. } | DiffOp::Update { native, .. } => native.as_ref(),
        DiffOp::Delete { .. } => None,
    }
}

fn get_op_props(op: &DiffOp) -> serde_json::Value {
    match op {
        DiffOp::Insert { properties, .. } => properties.clone(),
        DiffOp::Update { properties, .. } => properties.clone().unwrap_or(serde_json::json!({})),
        DiffOp::Delete { .. } => serde_json::json!(null),
    }
}

async fn get_op_geojson(store: &AppState, op: &DiffOp) -> Option<serde_json::Value> {
    let wkb = op_wkb(op)?;
    let row = sqlx::query("SELECT ST_AsGeoJSON(ST_GeomFromWKB($1))::jsonb as g")
        .bind(wkb)
        .fetch_optional(store.read_pool())
        .await
        .ok()??;
    Some(row.get("g"))
}

async fn get_feature_base_geojson(store: &AppState, feature_id: Uuid) -> Option<serde_json::Value> {
    let row = sqlx::query(
        "SELECT ST_AsGeoJSON(geometry)::jsonb as g FROM feature_versions
         WHERE feature_id = $1 ORDER BY created_at ASC, id ASC LIMIT 1",
    )
    .bind(feature_id)
    .fetch_optional(store.read_pool())
    .await
    .ok()??;
    Some(row.get("g"))
}

// ─── Error Handling ─────────────────────────────────────────────────

enum ConflictError {
    Db(sqlx::Error),
    Store(ptolemy_storage::StoreError),
}

impl From<sqlx::Error> for ConflictError {
    fn from(e: sqlx::Error) -> Self {
        ConflictError::Db(e)
    }
}

impl From<ptolemy_storage::StoreError> for ConflictError {
    fn from(e: ptolemy_storage::StoreError) -> Self {
        ConflictError::Store(e)
    }
}

impl IntoResponse for ConflictError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            ConflictError::Db(e) => {
                crate::errors::log_db_error("conflicts", &e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            ConflictError::Store(e) => crate::errors::store_error_status(&e),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}
