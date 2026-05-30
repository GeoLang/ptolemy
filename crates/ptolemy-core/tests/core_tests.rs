// Comprehensive tests for ptolemy-core data types and validation logic.

use ptolemy_core::*;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════════════
// Schema validation tests
// ═══════════════════════════════════════════════════════════════════════════

fn sample_schema() -> DatasetSchema {
    DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![
            FieldDef {
                name: "name".into(),
                field_type: FieldType::String,
                required: true,
                allowed_values: vec![],
                min: None,
                max: None,
            },
            FieldDef {
                name: "population".into(),
                field_type: FieldType::Integer,
                required: false,
                allowed_values: vec![],
                min: Some(0.0),
                max: Some(10_000_000.0),
            },
            FieldDef {
                name: "active".into(),
                field_type: FieldType::Boolean,
                required: true,
                allowed_values: vec![],
                min: None,
                max: None,
            },
        ],
        geometry_rules: GeometryRules {
            allowed_types: vec!["Point".into()],
            bounds: None,
            max_vertices: Some(1000),
        },
    }
}

#[test]
fn test_validate_valid_properties() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": "Test City",
        "population": 50000,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}

#[test]
fn test_validate_missing_required_field() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "population": 100,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "required");
    assert_eq!(errors[0].field, Some("name".into()));
}

#[test]
fn test_validate_null_required_field() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": null,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "required");
}

#[test]
fn test_validate_wrong_type_string_expected() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": 123,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "type");
    assert!(errors[0].message.contains("expected type String"));
}

#[test]
fn test_validate_wrong_type_boolean_expected() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": "Test",
        "active": "yes"
    });
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "type");
    assert!(errors[0].message.contains("Boolean"));
}

#[test]
fn test_validate_integer_below_min() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": "Negative City",
        "population": -5,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "min");
}

#[test]
fn test_validate_integer_above_max() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": "Mega City",
        "population": 99_999_999,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "max");
}

#[test]
fn test_validate_integer_at_min_boundary() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": "Zero City",
        "population": 0,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert!(errors.is_empty());
}

#[test]
fn test_validate_integer_at_max_boundary() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!({
        "name": "Max City",
        "population": 10_000_000,
        "active": true
    });
    let errors = schema.validate_properties(fid, &props);
    assert!(errors.is_empty());
}

#[test]
fn test_validate_multiple_errors() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    // Missing "name" (required) + "active" (required) + population out of range
    let props = json!({
        "population": -100
    });
    let errors = schema.validate_properties(fid, &props);
    assert!(
        errors.len() >= 2,
        "expected multiple errors, got: {errors:?}"
    );
}

#[test]
fn test_validate_properties_not_object() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    let props = json!("not an object");
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "properties_type");
}

#[test]
fn test_validate_optional_field_absent_is_ok() {
    let schema = sample_schema();
    let fid = Uuid::now_v7();
    // population is optional — not providing it should be fine
    let props = json!({
        "name": "Small Town",
        "active": false
    });
    let errors = schema.validate_properties(fid, &props);
    assert!(errors.is_empty());
}

#[test]
fn test_validate_allowed_values() {
    let schema = DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![FieldDef {
            name: "status".into(),
            field_type: FieldType::String,
            required: true,
            allowed_values: vec![json!("active"), json!("inactive"), json!("pending")],
            min: None,
            max: None,
        }],
        geometry_rules: GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        },
    };
    let fid = Uuid::now_v7();

    // Valid value
    let props = json!({"status": "active"});
    assert!(schema.validate_properties(fid, &props).is_empty());

    // Invalid value
    let props = json!({"status": "deleted"});
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "allowed_values");
}

#[test]
fn test_validate_float_field() {
    let schema = DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![FieldDef {
            name: "elevation".into(),
            field_type: FieldType::Float,
            required: true,
            allowed_values: vec![],
            min: Some(-500.0),
            max: Some(9000.0),
        }],
        geometry_rules: GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        },
    };
    let fid = Uuid::now_v7();

    let props = json!({"elevation": 1234.5});
    assert!(schema.validate_properties(fid, &props).is_empty());

    let props = json!({"elevation": 9001.0});
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "max");
}

// ═══════════════════════════════════════════════════════════════════════════
// Data model tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_feature_serialization_roundtrip() {
    let feature = Feature {
        id: Uuid::now_v7(),
        dataset_id: Uuid::now_v7(),
        geometry_wkb: vec![0x01, 0x01, 0x00, 0x00, 0x00], // WKB point header
        properties: json!({"name": "test", "value": 42}),
    };
    let serialized = serde_json::to_string(&feature).unwrap();
    let deserialized: Feature = serde_json::from_str(&serialized).unwrap();
    assert_eq!(feature.id, deserialized.id);
    assert_eq!(feature.dataset_id, deserialized.dataset_id);
    assert_eq!(feature.properties, deserialized.properties);
}

#[test]
fn test_dataset_geometry_types() {
    use ptolemy_core::dataset::GeometryType;

    let types = vec![
        GeometryType::Point,
        GeometryType::LineString,
        GeometryType::Polygon,
        GeometryType::MultiPoint,
        GeometryType::MultiLineString,
        GeometryType::MultiPolygon,
        GeometryType::GeometryCollection,
    ];
    for gt in types {
        let json = serde_json::to_string(&gt).unwrap();
        let back: GeometryType = serde_json::from_str(&json).unwrap();
        assert_eq!(gt, back);
    }
}

#[test]
fn test_branch_serialization() {
    let branch = Branch {
        id: Uuid::now_v7(),
        dataset_id: Uuid::now_v7(),
        name: "main".into(),
        head: Some(Uuid::now_v7()),
        created_at: OffsetDateTime::now_utc(),
        created_by: "test_user".into(),
    };
    let json = serde_json::to_string(&branch).unwrap();
    assert!(json.contains("\"main\""));
    let back: Branch = serde_json::from_str(&json).unwrap();
    assert_eq!(branch.name, back.name);
}

#[test]
fn test_changeset_serialization() {
    let cs = Changeset {
        id: Uuid::now_v7(),
        branch_id: Uuid::now_v7(),
        parent_id: None,
        message: "Initial commit".into(),
        author: "user@example.com".into(),
        created_at: OffsetDateTime::now_utc(),
    };
    let json = serde_json::to_string(&cs).unwrap();
    let back: Changeset = serde_json::from_str(&json).unwrap();
    assert_eq!(cs.id, back.id);
    assert_eq!(cs.message, back.message);
    assert!(back.parent_id.is_none());
}

#[test]
fn test_changeset_with_parent() {
    let parent_id = Uuid::now_v7();
    let cs = Changeset {
        id: Uuid::now_v7(),
        branch_id: Uuid::now_v7(),
        parent_id: Some(parent_id),
        message: "Second commit".into(),
        author: "user@example.com".into(),
        created_at: OffsetDateTime::now_utc(),
    };
    let json = serde_json::to_string(&cs).unwrap();
    let back: Changeset = serde_json::from_str(&json).unwrap();
    assert_eq!(back.parent_id, Some(parent_id));
}

// ═══════════════════════════════════════════════════════════════════════════
// Diff tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_diff_insert_operation() {
    use ptolemy_core::diff::{Diff, DiffOp};
    let fid = Uuid::now_v7();
    let diff = Diff {
        from_changeset: None,
        to_changeset: Uuid::now_v7(),
        operations: vec![DiffOp::Insert {
            feature_id: fid,
            geometry_wkb: vec![1, 2, 3],
            properties: json!({"name": "new feature"}),
        }],
    };
    assert_eq!(diff.operations.len(), 1);
    let json = serde_json::to_string(&diff).unwrap();
    assert!(json.contains("Insert"));
}

#[test]
fn test_diff_update_operation() {
    use ptolemy_core::diff::{Diff, DiffOp};
    let fid = Uuid::now_v7();
    let diff = Diff {
        from_changeset: Some(Uuid::now_v7()),
        to_changeset: Uuid::now_v7(),
        operations: vec![DiffOp::Update {
            feature_id: fid,
            geometry_wkb: Some(vec![4, 5, 6]),
            properties: None,
        }],
    };
    let json = serde_json::to_string(&diff).unwrap();
    let back: Diff = serde_json::from_str(&json).unwrap();
    assert_eq!(back.operations.len(), 1);
}

#[test]
fn test_diff_delete_operation() {
    use ptolemy_core::diff::{Diff, DiffOp};
    let fid = Uuid::now_v7();
    let diff = Diff {
        from_changeset: Some(Uuid::now_v7()),
        to_changeset: Uuid::now_v7(),
        operations: vec![DiffOp::Delete { feature_id: fid }],
    };
    let json = serde_json::to_string(&diff).unwrap();
    assert!(json.contains("Delete"));
}

#[test]
fn test_diff_multiple_operations() {
    use ptolemy_core::diff::{Diff, DiffOp};
    let diff = Diff {
        from_changeset: Some(Uuid::now_v7()),
        to_changeset: Uuid::now_v7(),
        operations: vec![
            DiffOp::Insert {
                feature_id: Uuid::now_v7(),
                geometry_wkb: vec![1],
                properties: json!({}),
            },
            DiffOp::Update {
                feature_id: Uuid::now_v7(),
                geometry_wkb: None,
                properties: Some(json!({"updated": true})),
            },
            DiffOp::Delete {
                feature_id: Uuid::now_v7(),
            },
        ],
    };
    assert_eq!(diff.operations.len(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// Review / Merge Request tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_merge_request_status_serialization() {
    use ptolemy_core::review::MergeRequestStatus;
    let statuses = vec![
        MergeRequestStatus::Open,
        MergeRequestStatus::Approved,
        MergeRequestStatus::Merged,
        MergeRequestStatus::Closed,
    ];
    for status in statuses {
        let json = serde_json::to_string(&status).unwrap();
        let back: MergeRequestStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}

#[test]
fn test_merge_request_roundtrip() {
    let mr = MergeRequest {
        id: Uuid::now_v7(),
        dataset_id: Uuid::now_v7(),
        source_branch_id: Uuid::now_v7(),
        target_branch_id: Uuid::now_v7(),
        title: "Add new parcels".into(),
        description: "10 new parcels from survey".into(),
        author: "surveyor@example.com".into(),
        status: MergeRequestStatus::Open,
        created_at: OffsetDateTime::now_utc(),
        updated_at: OffsetDateTime::now_utc(),
    };
    let json = serde_json::to_string(&mr).unwrap();
    let back: MergeRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(mr.id, back.id);
    assert_eq!(mr.title, back.title);
    assert_eq!(back.status, MergeRequestStatus::Open);
}

#[test]
fn test_review_comment_with_feature_link() {
    let comment = ReviewComment {
        id: Uuid::now_v7(),
        merge_request_id: Uuid::now_v7(),
        feature_id: Some(Uuid::now_v7()),
        author: "reviewer@example.com".into(),
        body: "This polygon overlaps parcel 42".into(),
        created_at: OffsetDateTime::now_utc(),
    };
    let json = serde_json::to_string(&comment).unwrap();
    let back: ReviewComment = serde_json::from_str(&json).unwrap();
    assert!(back.feature_id.is_some());
}

#[test]
fn test_review_comment_without_feature_link() {
    let comment = ReviewComment {
        id: Uuid::now_v7(),
        merge_request_id: Uuid::now_v7(),
        feature_id: None,
        author: "reviewer@example.com".into(),
        body: "Looks good overall".into(),
        created_at: OffsetDateTime::now_utc(),
    };
    let json = serde_json::to_string(&comment).unwrap();
    let back: ReviewComment = serde_json::from_str(&json).unwrap();
    assert!(back.feature_id.is_none());
}

// ═══════════════════════════════════════════════════════════════════════════
// Event tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_event_type_display() {
    assert_eq!(EventType::Commit.to_string(), "commit");
    assert_eq!(EventType::Merge.to_string(), "merge");
    assert_eq!(EventType::BranchCreated.to_string(), "branch_created");
    assert_eq!(EventType::BranchDeleted.to_string(), "branch_deleted");
    assert_eq!(EventType::SchemaChanged.to_string(), "schema_changed");
    assert_eq!(EventType::QualityAlert.to_string(), "quality_alert");
}

#[test]
fn test_event_type_serde_roundtrip() {
    let types = vec![
        EventType::Commit,
        EventType::Merge,
        EventType::BranchCreated,
        EventType::BranchDeleted,
        EventType::SchemaChanged,
        EventType::QualityAlert,
    ];
    for et in types {
        let json = serde_json::to_string(&et).unwrap();
        let back: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(et, back);
    }
}

#[test]
fn test_webhook_serialization() {
    use ptolemy_core::event::Webhook;
    let wh = Webhook {
        id: Uuid::now_v7(),
        dataset_id: Uuid::now_v7(),
        url: "https://example.com/webhook".into(),
        events: vec!["commit".into(), "merge".into()],
        secret: Some("s3cr3t".into()),
        active: true,
    };
    let json = serde_json::to_string(&wh).unwrap();
    let back: Webhook = serde_json::from_str(&json).unwrap();
    assert_eq!(back.url, "https://example.com/webhook");
    assert_eq!(back.events.len(), 2);
    assert!(back.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// Topology rule type tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_topology_rule_type_serde() {
    let rule = TopologyRuleType::NoOverlap;
    let json = serde_json::to_string(&rule).unwrap();
    let back: TopologyRuleType = serde_json::from_str(&json).unwrap();
    assert_eq!(back, TopologyRuleType::NoOverlap);
}

#[test]
fn test_topology_rule_with_reference() {
    let ref_id = Uuid::now_v7();
    let rule = TopologyRuleType::MustBeCoveredBy {
        reference_dataset_id: ref_id,
    };
    let json = serde_json::to_string(&rule).unwrap();
    let back: TopologyRuleType = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rule);
}

#[test]
fn test_topology_rule_vertex_count() {
    let rule = TopologyRuleType::MaxVertexCount { max: 500 };
    let json = serde_json::to_string(&rule).unwrap();
    let back: TopologyRuleType = serde_json::from_str(&json).unwrap();
    if let TopologyRuleType::MaxVertexCount { max } = back {
        assert_eq!(max, 500);
    } else {
        panic!("wrong variant");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Quality report tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_quality_report_serialization() {
    use ptolemy_core::schema::{FieldNullCount, QualityStatistics};
    let report = QualityReport {
        branch_id: Uuid::now_v7(),
        total_features: 1000,
        valid_features: 980,
        errors: vec![ValidationError {
            feature_id: Uuid::now_v7(),
            field: Some("name".into()),
            rule: "required".into(),
            message: "field 'name' is required".into(),
        }],
        statistics: QualityStatistics {
            null_geometry_count: 2,
            invalid_geometry_count: 5,
            null_fields: vec![FieldNullCount {
                field_name: "email".into(),
                null_count: 15,
            }],
            out_of_bounds_count: 3,
        },
    };
    let json = serde_json::to_string(&report).unwrap();
    let back: QualityReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.total_features, 1000);
    assert_eq!(back.valid_features, 980);
    assert_eq!(back.errors.len(), 1);
    assert_eq!(back.statistics.null_geometry_count, 2);
}

// ═══════════════════════════════════════════════════════════════════════════
// Edge case tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_validate_empty_schema_accepts_anything() {
    let schema = DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![],
        geometry_rules: GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        },
    };
    let fid = Uuid::now_v7();
    let props = json!({"anything": "goes", "nested": {"deep": true}});
    let errors = schema.validate_properties(fid, &props);
    assert!(errors.is_empty());
}

#[test]
fn test_validate_extra_properties_are_ignored() {
    let schema = DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![FieldDef {
            name: "name".into(),
            field_type: FieldType::String,
            required: true,
            allowed_values: vec![],
            min: None,
            max: None,
        }],
        geometry_rules: GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        },
    };
    let fid = Uuid::now_v7();
    let props = json!({"name": "valid", "extra_field": 123, "another": true});
    let errors = schema.validate_properties(fid, &props);
    assert!(errors.is_empty());
}

#[test]
fn test_validate_array_field_type() {
    let schema = DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![FieldDef {
            name: "tags".into(),
            field_type: FieldType::Array,
            required: true,
            allowed_values: vec![],
            min: None,
            max: None,
        }],
        geometry_rules: GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        },
    };
    let fid = Uuid::now_v7();

    // Valid
    let props = json!({"tags": ["urban", "residential"]});
    assert!(schema.validate_properties(fid, &props).is_empty());

    // Invalid
    let props = json!({"tags": "not an array"});
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].rule, "type");
}

#[test]
fn test_validate_object_field_type() {
    let schema = DatasetSchema {
        dataset_id: Uuid::now_v7(),
        fields: vec![FieldDef {
            name: "metadata".into(),
            field_type: FieldType::Object,
            required: false,
            allowed_values: vec![],
            min: None,
            max: None,
        }],
        geometry_rules: GeometryRules {
            allowed_types: vec![],
            bounds: None,
            max_vertices: None,
        },
    };
    let fid = Uuid::now_v7();

    let props = json!({"metadata": {"source": "gps", "accuracy": 2.5}});
    assert!(schema.validate_properties(fid, &props).is_empty());

    let props = json!({"metadata": [1, 2, 3]});
    let errors = schema.validate_properties(fid, &props);
    assert_eq!(errors.len(), 1);
}

#[test]
fn test_dataset_creation() {
    use ptolemy_core::dataset::GeometryType;
    let ds = Dataset {
        id: Uuid::now_v7(),
        name: "parcels".into(),
        srid: 4326,
        geometry_type: GeometryType::Polygon,
        created_at: OffsetDateTime::now_utc(),
        created_by: "admin".into(),
    };
    let json = serde_json::to_string(&ds).unwrap();
    let back: Dataset = serde_json::from_str(&json).unwrap();
    assert_eq!(back.name, "parcels");
    assert_eq!(back.srid, 4326);
    assert_eq!(back.geometry_type, GeometryType::Polygon);
}
