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
    /// No two features may overlap
    NoOverlap,
    /// No gaps between adjacent polygons
    NoGaps,
    /// Lines must be connected at endpoints
    MustConnect,
    /// Points must be within polygons of another dataset
    MustBeInside { reference_dataset_id: Uuid },
    /// Features must not self-intersect
    NoSelfIntersection,
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
