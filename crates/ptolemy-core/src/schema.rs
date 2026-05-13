// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Schema definitions and validation for datasets.
//!
//! Allows defining typed schemas per dataset — required fields, allowed values,
//! geometry type constraints — and validates features against them on commit.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Schema definition for a dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetSchema {
    pub dataset_id: Uuid,
    pub fields: Vec<FieldDef>,
    pub geometry_rules: GeometryRules,
}

/// Definition of a property field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
    /// Optional: allowed values for enum-like fields
    #[serde(default)]
    pub allowed_values: Vec<serde_json::Value>,
    /// Optional: min/max for numeric fields
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    Array,
    Object,
}

/// Rules constraining geometry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeometryRules {
    /// If set, features must match this geometry type
    #[serde(default)]
    pub allowed_types: Vec<String>,
    /// If set, geometry must fit within this bounding box
    #[serde(default)]
    pub bounds: Option<BoundingBox>,
    /// Max number of vertices (prevent overly complex geometries)
    #[serde(default)]
    pub max_vertices: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundingBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

/// Topology rules for spatial integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyRule {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub rule_type: TopologyRuleType,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRuleType {
    // ─── Polygon rules ──────────────────────────────────────────
    /// No two features may overlap
    NoOverlap,
    /// No gaps between adjacent polygons
    NoGaps,
    /// Polygons must not overlap each other (same layer)
    MustNotOverlap,
    /// Polygons must not overlap polygons in another dataset
    MustNotOverlapWith { reference_dataset_id: Uuid },
    /// Polygons must be covered by polygons in another dataset
    MustBeCoveredBy { reference_dataset_id: Uuid },
    /// Polygon must cover each other completely
    MustCoverEachOther { reference_dataset_id: Uuid },
    /// Boundary must be covered by lines in another dataset
    BoundaryMustBeCoveredBy { reference_dataset_id: Uuid },
    /// Area boundary must be covered by boundary of another area
    AreaBoundaryMustBeCoveredByAreaBoundary { reference_dataset_id: Uuid },
    /// Must contain point from another dataset
    ContainsPoint { reference_dataset_id: Uuid },

    // ─── Line rules ─────────────────────────────────────────────
    /// Lines must not overlap
    LineMustNotOverlap,
    /// Lines must not intersect
    LineMustNotIntersect,
    /// Lines must not have dangles (unconnected endpoints)
    MustNotHaveDangles,
    /// Lines must not have pseudonodes (unnecessary vertices)
    MustNotHavePseudonodes,
    /// Lines must be connected at endpoints
    MustConnect,
    /// Lines must not self-overlap
    LineMustNotSelfOverlap,
    /// Lines must not self-intersect
    LineMustNotSelfIntersect,
    /// Lines must be single-part
    LineMustBeSinglePart,
    /// Lines must not intersect or touch interior
    MustNotIntersectOrTouchInterior { reference_dataset_id: Uuid },
    /// Endpoint must be covered by point
    EndpointMustBeCoveredBy { reference_dataset_id: Uuid },
    /// Lines must be covered by boundary of polygon
    LineMustBeCoveredByBoundary { reference_dataset_id: Uuid },

    // ─── Point rules ────────────────────────────────────────────
    /// Points must be covered by line
    PointMustBeCoveredByLine { reference_dataset_id: Uuid },
    /// Points must be covered by endpoint of line
    PointMustBeCoveredByEndpoint { reference_dataset_id: Uuid },
    /// Points must be covered by polygon boundary
    PointMustBeCoveredByBoundary { reference_dataset_id: Uuid },
    /// Points must be inside polygon
    MustBeInside { reference_dataset_id: Uuid },
    /// Points must be properly inside polygon (not on boundary)
    MustBeProperlyInside { reference_dataset_id: Uuid },
    /// Points must not overlap (no coincident points)
    PointMustNotOverlap,

    // ─── General rules ──────────────────────────────────────────
    /// Features must not self-intersect
    NoSelfIntersection,
    /// Features must not have null geometry
    MustNotBeNull,
    /// Geometry must be valid (well-formed)
    MustBeValid,
    /// Features must be within a defined extent
    MustBeWithinExtent {
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    },
    /// Vertex count must not exceed limit
    MaxVertexCount { max: usize },
}

/// Validation error returned when a feature fails schema or topology checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub feature_id: Uuid,
    pub field: Option<String>,
    pub rule: String,
    pub message: String,
}

/// Data quality report for a branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    pub branch_id: Uuid,
    pub total_features: i64,
    pub valid_features: i64,
    pub errors: Vec<ValidationError>,
    pub statistics: QualityStatistics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityStatistics {
    pub null_geometry_count: i64,
    pub invalid_geometry_count: i64,
    pub null_fields: Vec<FieldNullCount>,
    pub out_of_bounds_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldNullCount {
    pub field_name: String,
    pub null_count: i64,
}

// ─── Schema Validation ──────────────────────────────────────────────

impl DatasetSchema {
    /// Validate a feature's properties against this schema.
    /// Returns a list of validation errors (empty = valid).
    pub fn validate_properties(
        &self,
        feature_id: Uuid,
        properties: &serde_json::Value,
    ) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        let obj = match properties.as_object() {
            Some(o) => o,
            None => {
                errors.push(ValidationError {
                    feature_id,
                    field: None,
                    rule: "properties_type".into(),
                    message: "properties must be a JSON object".into(),
                });
                return errors;
            }
        };

        for field in &self.fields {
            let value = obj.get(&field.name);

            // Required check
            if field.required && (value.is_none() || value == Some(&serde_json::Value::Null)) {
                errors.push(ValidationError {
                    feature_id,
                    field: Some(field.name.clone()),
                    rule: "required".into(),
                    message: format!("field '{}' is required", field.name),
                });
                continue;
            }

            let Some(val) = value else { continue };
            if val.is_null() {
                continue;
            }

            // Type check
            let type_ok = match field.field_type {
                FieldType::String => val.is_string(),
                FieldType::Integer => val.is_i64() || val.is_u64(),
                FieldType::Float => val.is_f64() || val.is_i64() || val.is_u64(),
                FieldType::Boolean => val.is_boolean(),
                FieldType::Array => val.is_array(),
                FieldType::Object => val.is_object(),
            };

            if !type_ok {
                errors.push(ValidationError {
                    feature_id,
                    field: Some(field.name.clone()),
                    rule: "type".into(),
                    message: format!(
                        "field '{}' expected type {:?}, got {}",
                        field.name,
                        field.field_type,
                        json_type_name(val)
                    ),
                });
                continue;
            }

            // Allowed values check
            if !field.allowed_values.is_empty() && !field.allowed_values.contains(val) {
                errors.push(ValidationError {
                    feature_id,
                    field: Some(field.name.clone()),
                    rule: "allowed_values".into(),
                    message: format!("field '{}' value not in allowed set", field.name),
                });
            }

            // Numeric range check
            if let Some(num) = val.as_f64() {
                if let Some(min) = field.min
                    && num < min
                {
                    errors.push(ValidationError {
                        feature_id,
                        field: Some(field.name.clone()),
                        rule: "min".into(),
                        message: format!("field '{}' value {} < min {}", field.name, num, min),
                    });
                }
                if let Some(max) = field.max
                    && num > max
                {
                    errors.push(ValidationError {
                        feature_id,
                        field: Some(field.name.clone()),
                        rule: "max".into(),
                        message: format!("field '{}' value {} > max {}", field.name, num, max),
                    });
                }
            }
        }

        errors
    }
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
