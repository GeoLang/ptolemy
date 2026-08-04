// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The writes that used to be raw SQL inside `ptolemy-api` handlers.
//!
//! Every method here takes a [`WriteGrant`] and takes the id it writes under
//! from that grant, never from its own arguments. Only the write ladder mints a
//! grant, so a handler cannot reach one of these without having been through it,
//! and cannot aim it at a target other than the one that was checked.
//!
//! They sit together rather than beside the reads of the same tables because
//! this is the crate's whole guarded write surface, and it is easier to audit as
//! one list than as forty methods scattered by feature.

use serde_json::Value;
use uuid::Uuid;

use crate::grant::WriteGrant;
use crate::postgres::{PgStore, StoreError};

// ─── Relationships ──────────────────────────────────────────────────

/// A relationship class spans two datasets, so it takes a grant on each. The
/// ids come from the grants, which is what makes "both sides were checked" a
/// property of the call rather than of the handler remembering to pass the same
/// ids twice.
pub struct RelationshipClassInput<'a> {
    pub name: &'a str,
    pub origin_foreign_key: &'a str,
    pub cardinality: &'a str,
    pub forward_label: &'a str,
    pub backward_label: &'a str,
}

impl PgStore {
    pub async fn create_relationship_class(
        &self,
        origin: &WriteGrant,
        destination: &WriteGrant,
        input: &RelationshipClassInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO relationship_classes
                (id, name, origin_dataset_id, destination_dataset_id, origin_foreign_key, cardinality, forward_label, backward_label)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(input.name)
        .bind(origin.id())
        .bind(destination.id())
        .bind(input.origin_foreign_key)
        .bind(input.cardinality)
        .bind(input.forward_label)
        .bind(input.backward_label)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn delete_relationship_class(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM relationship_classes WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `grant` is on the relationship class, which is what the route names and
    /// what the ladder resolved to its two datasets.
    pub async fn create_relationship_record(
        &self,
        grant: &WriteGrant,
        origin_feature_id: Uuid,
        destination_feature_id: Uuid,
        properties: &Value,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO relationship_records (id, relationship_class_id, origin_feature_id, destination_feature_id, properties)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(origin_feature_id)
        .bind(destination_feature_id)
        .bind(properties)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn delete_relationship_record(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM relationship_records WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ─── Cartography ────────────────────────────────────────────────────

pub struct SymbologyRuleInput<'a> {
    pub name: &'a str,
    pub min_scale: Option<f64>,
    pub max_scale: Option<f64>,
    pub filter_expression: Option<&'a str>,
    pub symbol: &'a Value,
    pub priority: i32,
}

/// Only the fields the update route has ever applied. `min_scale` and
/// `max_scale` are accepted in the request body and silently ignored, which is
/// what the handler did before this moved; changing it would change behaviour.
pub struct SymbologyRulePatch<'a> {
    pub symbol: Option<&'a Value>,
    pub filter_expression: Option<&'a str>,
    pub priority: Option<i32>,
}

pub struct LabelRuleInput<'a> {
    pub name: &'a str,
    pub min_scale: Option<f64>,
    pub max_scale: Option<f64>,
    pub field_expression: &'a str,
    pub placement: &'a Value,
    pub font: &'a Value,
    pub priority: i32,
}

pub struct LabelRulePatch<'a> {
    pub field_expression: Option<&'a str>,
    pub placement: Option<&'a Value>,
    pub font: Option<&'a Value>,
    pub priority: Option<i32>,
}

impl PgStore {
    /// `grant` is on the dataset the rule belongs to.
    pub async fn create_symbology_rule(
        &self,
        grant: &WriteGrant,
        input: &SymbologyRuleInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO symbology_rules (id, dataset_id, name, min_scale, max_scale, filter_expression, symbol, priority)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.name)
        .bind(input.min_scale)
        .bind(input.max_scale)
        .bind(input.filter_expression)
        .bind(input.symbol)
        .bind(input.priority)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// One statement per set field rather than one with COALESCE: a JSON `null`
    /// symbol is a value the caller can mean, and COALESCE cannot tell it from
    /// "leave this alone".
    pub async fn update_symbology_rule(
        &self,
        grant: &WriteGrant,
        patch: &SymbologyRulePatch<'_>,
    ) -> Result<(), StoreError> {
        if let Some(symbol) = patch.symbol {
            sqlx::query("UPDATE symbology_rules SET symbol = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(symbol)
                .execute(&self.pool)
                .await?;
        }
        if let Some(expr) = patch.filter_expression {
            sqlx::query("UPDATE symbology_rules SET filter_expression = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(expr)
                .execute(&self.pool)
                .await?;
        }
        if let Some(priority) = patch.priority {
            sqlx::query("UPDATE symbology_rules SET priority = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(priority)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn delete_symbology_rule(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM symbology_rules WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `grant` is on the dataset the rule belongs to.
    pub async fn create_label_rule(
        &self,
        grant: &WriteGrant,
        input: &LabelRuleInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO label_rules (id, dataset_id, name, min_scale, max_scale, field_expression, placement, font, priority)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.name)
        .bind(input.min_scale)
        .bind(input.max_scale)
        .bind(input.field_expression)
        .bind(input.placement)
        .bind(input.font)
        .bind(input.priority)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn update_label_rule(
        &self,
        grant: &WriteGrant,
        patch: &LabelRulePatch<'_>,
    ) -> Result<(), StoreError> {
        if let Some(expr) = patch.field_expression {
            sqlx::query("UPDATE label_rules SET field_expression = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(expr)
                .execute(&self.pool)
                .await?;
        }
        if let Some(placement) = patch.placement {
            sqlx::query("UPDATE label_rules SET placement = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(placement)
                .execute(&self.pool)
                .await?;
        }
        if let Some(font) = patch.font {
            sqlx::query("UPDATE label_rules SET font = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(font)
                .execute(&self.pool)
                .await?;
        }
        if let Some(priority) = patch.priority {
            sqlx::query("UPDATE label_rules SET priority = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(priority)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn delete_label_rule(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM label_rules WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ─── Domains, subtypes and attribute rules ──────────────────────────

pub struct DomainInput<'a> {
    pub name: &'a str,
    pub domain_type: &'a str,
    pub field_type: &'a str,
    pub coded_values: Option<&'a Value>,
    pub range_min: Option<f64>,
    pub range_max: Option<f64>,
}

pub struct SubtypeInput<'a> {
    pub subtype_field: &'a str,
    pub name: &'a str,
    pub code: i32,
    pub default_values: &'a Value,
    pub domain_assignments: &'a Value,
}

pub struct AttributeRuleInput<'a> {
    pub name: &'a str,
    pub rule_type: &'a str,
    pub trigger_event: &'a str,
    pub expression: &'a str,
    pub error_message: Option<&'a str>,
}

pub struct AttributeRulePatch<'a> {
    pub expression: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub enabled: Option<bool>,
}

impl PgStore {
    /// `grant` is on the dataset the domain belongs to.
    pub async fn create_domain(
        &self,
        grant: &WriteGrant,
        input: &DomainInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO domains (id, dataset_id, name, domain_type, field_type, coded_values, range_min, range_max)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.name)
        .bind(input.domain_type)
        .bind(input.field_type)
        .bind(input.coded_values)
        .bind(input.range_min)
        .bind(input.range_max)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn delete_domain(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM domains WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `grant` is on the dataset the subtype belongs to.
    pub async fn create_subtype(
        &self,
        grant: &WriteGrant,
        input: &SubtypeInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO subtypes (id, dataset_id, subtype_field, name, code, default_values, domain_assignments)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.subtype_field)
        .bind(input.name)
        .bind(input.code)
        .bind(input.default_values)
        .bind(input.domain_assignments)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn delete_subtype(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM subtypes WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `grant` is on the dataset the rule belongs to.
    pub async fn create_attribute_rule(
        &self,
        grant: &WriteGrant,
        input: &AttributeRuleInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO attribute_rules (id, dataset_id, name, rule_type, trigger_event, expression, error_message)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.name)
        .bind(input.rule_type)
        .bind(input.trigger_event)
        .bind(input.expression)
        .bind(input.error_message)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn update_attribute_rule(
        &self,
        grant: &WriteGrant,
        patch: &AttributeRulePatch<'_>,
    ) -> Result<(), StoreError> {
        if let Some(expr) = patch.expression {
            sqlx::query("UPDATE attribute_rules SET expression = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(expr)
                .execute(&self.pool)
                .await?;
        }
        if let Some(message) = patch.error_message {
            sqlx::query("UPDATE attribute_rules SET error_message = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(message)
                .execute(&self.pool)
                .await?;
        }
        if let Some(enabled) = patch.enabled {
            sqlx::query("UPDATE attribute_rules SET enabled = $2 WHERE id = $1")
                .bind(grant.id())
                .bind(enabled)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn delete_attribute_rule(&self, grant: &WriteGrant) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM attribute_rules WHERE id = $1")
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ─── Catalog: tags and metadata ─────────────────────────────────────

pub struct DatasetMetadataInput<'a> {
    pub description: &'a str,
    pub source: Option<&'a str>,
    pub license: Option<&'a str>,
    pub attribution: Option<&'a str>,
    pub keywords: &'a [String],
}

impl PgStore {
    pub async fn add_dataset_tag(&self, grant: &WriteGrant, tag: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO dataset_tags (dataset_id, tag) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(grant.id())
        .bind(tag)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `tag` is free text out of the path, so it can be anything; the dataset it
    /// is deleted from comes from the grant and cannot be.
    pub async fn remove_dataset_tag(
        &self,
        grant: &WriteGrant,
        tag: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM dataset_tags WHERE dataset_id = $1 AND tag = $2")
            .bind(grant.id())
            .bind(tag)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_dataset_metadata(
        &self,
        grant: &WriteGrant,
        input: &DatasetMetadataInput<'_>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO dataset_metadata (dataset_id, description, source, license, attribution, keywords, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, now())
             ON CONFLICT (dataset_id) DO UPDATE SET
                description = $2, source = $3, license = $4, attribution = $5, keywords = $6, updated_at = now()",
        )
        .bind(grant.id())
        .bind(input.description)
        .bind(input.source)
        .bind(input.license)
        .bind(input.attribution)
        .bind(input.keywords)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// ─── Networks ───────────────────────────────────────────────────────

impl PgStore {
    /// `grant` is on the dataset the network belongs to.
    pub async fn create_network(
        &self,
        grant: &WriteGrant,
        name: &str,
        network_type: &str,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO networks (id, dataset_id, name, network_type) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(name)
        .bind(network_type)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `grant` is on the network, which the ladder resolved to its dataset.
    pub async fn add_network_junction(
        &self,
        grant: &WriteGrant,
        feature_id: Option<Uuid>,
        lng: f64,
        lat: f64,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO network_junctions (id, network_id, feature_id, geometry)
             VALUES ($1, $2, $3, ST_SetSRID(ST_MakePoint($4, $5), 4326))",
        )
        .bind(id)
        .bind(grant.id())
        .bind(feature_id)
        .bind(lng)
        .bind(lat)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `grant` is on the network, which the ladder resolved to its dataset.
    pub async fn add_network_edge(
        &self,
        grant: &WriteGrant,
        feature_id: Uuid,
        from_junction: Option<Uuid>,
        to_junction: Option<Uuid>,
        cost: f64,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO network_edges (id, network_id, feature_id, from_junction, to_junction, cost)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(feature_id)
        .bind(from_junction)
        .bind(to_junction)
        .bind(cost)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }
}

// ─── Linear referencing ─────────────────────────────────────────────

pub struct RouteEventInput<'a> {
    pub event_type: &'a str,
    pub from_measure: f64,
    /// `Some` makes it a linear event located between the two measures, `None` a
    /// point event located at the one.
    pub to_measure: Option<f64>,
    pub properties: &'a Value,
}

impl PgStore {
    /// `grant` is on the dataset the route belongs to.
    pub async fn create_route(
        &self,
        grant: &WriteGrant,
        name: &str,
        geometry_wkb: &[u8],
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO routes (id, dataset_id, name, geometry, total_length)
             VALUES ($1, $2, $3, ST_GeomFromWKB($4, 4326), ST_Length(ST_GeomFromWKB($4, 4326)::geography))",
        )
        .bind(id)
        .bind(grant.id())
        .bind(name)
        .bind(geometry_wkb)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `grant` is on the route, which the ladder resolved to its dataset. The
    /// event's geometry is derived from that same route, so it cannot be placed
    /// on one the caller was not checked against.
    pub async fn create_route_event(
        &self,
        grant: &WriteGrant,
        input: &RouteEventInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        match input.to_measure {
            Some(to_measure) => {
                sqlx::query(
                    "INSERT INTO route_events (id, route_id, event_type, from_measure, to_measure, properties, geometry)
                     SELECT $1, $2, $3, $4, $5, $6,
                            ST_LocateBetween(r.geometry, $4, $5)
                     FROM routes r WHERE r.id = $2",
                )
                .bind(id)
                .bind(grant.id())
                .bind(input.event_type)
                .bind(input.from_measure)
                .bind(to_measure)
                .bind(input.properties)
                .execute(&self.pool)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO route_events (id, route_id, event_type, from_measure, properties, geometry)
                     SELECT $1, $2, $3, $4, $5,
                            ST_LocateAlong(r.geometry, $4)
                     FROM routes r WHERE r.id = $2",
                )
                .bind(id)
                .bind(grant.id())
                .bind(input.event_type)
                .bind(input.from_measure)
                .bind(input.properties)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(id)
    }
}

// ─── Raster and point cloud catalogs ────────────────────────────────

pub struct RasterCatalogInput<'a> {
    pub name: &'a str,
    pub srid: i32,
    pub pixel_type: &'a str,
    pub num_bands: i32,
}

pub struct RasterTileInput<'a> {
    pub bounds_wkb: &'a [u8],
    pub zoom_level: i32,
    pub rast: &'a [u8],
}

pub struct PointCloudPatchInput<'a> {
    pub bounds_wkb: &'a [u8],
    pub num_points: i32,
    pub patch: &'a [u8],
}

impl PgStore {
    /// `grant` is on the dataset the catalog hangs off.
    pub async fn create_raster_catalog(
        &self,
        grant: &WriteGrant,
        input: &RasterCatalogInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO raster_catalogs (id, dataset_id, name, srid, pixel_type, num_bands)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.name)
        .bind(input.srid)
        .bind(input.pixel_type)
        .bind(input.num_bands)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `grant` is on the catalog, which the ladder resolved to its dataset.
    pub async fn upload_raster_tile(
        &self,
        grant: &WriteGrant,
        input: &RasterTileInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO raster_tiles (id, catalog_id, bounds, zoom_level, rast)
             VALUES ($1, $2, ST_GeomFromWKB($3, 4326), $4, $5::raster)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.bounds_wkb)
        .bind(input.zoom_level)
        .bind(input.rast)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `grant` is on the dataset the catalog hangs off.
    pub async fn create_pointcloud_catalog(
        &self,
        grant: &WriteGrant,
        name: &str,
        srid: i32,
        schema_xml: Option<&str>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO pointcloud_catalogs (id, dataset_id, name, srid, schema_xml)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(name)
        .bind(srid)
        .bind(schema_xml)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `grant` is on the catalog, which the ladder resolved to its dataset.
    pub async fn add_pointcloud_patch(
        &self,
        grant: &WriteGrant,
        input: &PointCloudPatchInput<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO pointcloud_patches (id, catalog_id, bounds, num_points, pa)
             VALUES ($1, $2, ST_GeomFromWKB($3, 4326), $4, $5::pcpatch)",
        )
        .bind(id)
        .bind(grant.id())
        .bind(input.bounds_wkb)
        .bind(input.num_points)
        .bind(input.patch)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }
}

// ─── Trajectories ───────────────────────────────────────────────────

/// Migration 015 only gives `trajectories` the MobilityDB column types where the
/// extension is installed, so the trip goes in as one of two different things.
pub enum TrajectoryTrip<'a> {
    /// A `tgeompoint` literal, which the query also derives the period from.
    MobilityDb(&'a str),
    /// The raw point array, with the period derived from its timestamps.
    Jsonb(&'a Value),
}

impl PgStore {
    /// `grant` is on the dataset the trajectory belongs to.
    pub async fn create_trajectory(
        &self,
        grant: &WriteGrant,
        name: &str,
        trip: &TrajectoryTrip<'_>,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::now_v7();
        match trip {
            TrajectoryTrip::MobilityDb(literal) => {
                sqlx::query(
                    "INSERT INTO trajectories (id, dataset_id, name, trip, period)
                     VALUES ($1, $2, $3, $4::tgeompoint, period($4::tgeompoint))",
                )
                .bind(id)
                .bind(grant.id())
                .bind(name)
                .bind(*literal)
                .execute(&self.pool)
                .await?;
            }
            TrajectoryTrip::Jsonb(points) => {
                sqlx::query(
                    "INSERT INTO trajectories (id, dataset_id, name, trip, period)
                     SELECT $1, $2, $3, $4,
                            tstzrange(min((e->>'timestamp')::timestamptz),
                                      max((e->>'timestamp')::timestamptz), '[]')
                     FROM jsonb_array_elements($4) e",
                )
                .bind(id)
                .bind(grant.id())
                .bind(name)
                .bind(*points)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(id)
    }
}

// ─── Branch-wide rewrites ───────────────────────────────────────────

impl PgStore {
    /// `grant` is on the branch whose feature versions get an H3 cell.
    pub async fn index_branch_features_h3(
        &self,
        grant: &WriteGrant,
        resolution: i32,
    ) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE feature_versions fv
             SET h3_index = h3_lat_lng_to_cell(ST_Centroid(fv.geometry), $2)
             FROM changesets c
             WHERE fv.changeset_id = c.id
               AND c.branch_id = $1
               AND fv.geometry IS NOT NULL",
        )
        .bind(grant.id())
        .bind(resolution)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// `grant` is on the branch whose feature versions get an embedding.
    ///
    /// `fields` names property keys, and they are interpolated into the
    /// statement because a JSON key cannot be a bind parameter. Each one is
    /// quote-doubled before it goes into a single-quoted literal, which is the
    /// whole escape needed while `standard_conforming_strings` is on — Postgres
    /// has defaulted to on since 9.1 and nothing here turns it off.
    pub async fn embed_branch_features(
        &self,
        grant: &WriteGrant,
        fields: &[String],
    ) -> Result<u64, StoreError> {
        let fields_expr = fields
            .iter()
            .map(|f| format!("COALESCE(fv.properties->>'{}', '')", f.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" || ' ' || ");

        let props_expr = if fields_expr.is_empty() {
            "fv.properties::text".to_string()
        } else {
            fields_expr
        };

        let query = format!(
            "UPDATE feature_versions fv
             SET embedding = (
                SELECT array_agg(v)::vector(256)
                FROM (
                    SELECT (get_byte(digest(({props}) || i::text, 'sha256'), i % 32)::float / 255.0) as v
                    FROM generate_series(0, 255) as i
                ) sub
             )
             FROM changesets c
             WHERE fv.changeset_id = c.id
               AND c.branch_id = $1
               AND fv.properties IS NOT NULL",
            props = props_expr,
        );

        let result = sqlx::query(&query)
            .bind(grant.id())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// `grant` is on the branch the conflict belongs to, and the branch is part
    /// of the WHERE clause, so a conflict id from another branch matches nothing
    /// and the caller gets the same 404 as for one that does not exist.
    ///
    /// `Ok(None)` is no such conflict on that branch. An `Err` also covers the
    /// instance where `merge_conflicts` was never created, which the caller
    /// reports rather than failing.
    pub async fn resolve_merge_conflict(
        &self,
        grant: &WriteGrant,
        conflict_id: Uuid,
        resolution: &str,
    ) -> Result<Option<Uuid>, StoreError> {
        let row = sqlx::query_scalar(
            "UPDATE merge_conflicts SET resolved = true, resolution = $2, resolved_at = now()
             WHERE id = $1 AND branch_id = $3
             RETURNING feature_id",
        )
        .bind(conflict_id)
        .bind(resolution)
        .bind(grant.id())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}

// ─── Writes with no grant, and why ──────────────────────────────────
//
// These two have no dataset or branch behind them, so there is no ladder to
// run and no grant to demand. They live here rather than in `ptolemy-api` so
// that `ci/no-raw-writes.sh` needs no allowlist entry for them and the reason
// each one is ungrantable sits next to its SQL.

impl PgStore {
    /// Background maintenance: no request, no caller, no target. Deletes locks
    /// whose expiry has already passed, which is the row's own decision.
    pub async fn delete_expired_feature_locks(&self) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM feature_locks WHERE expires_at < now()")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Background maintenance, as above: the retention window is fixed here and
    /// nothing about it comes from a caller.
    pub async fn delete_events_older_than_retention(&self) -> Result<u64, StoreError> {
        let result =
            sqlx::query("DELETE FROM events WHERE created_at < now() - interval '30 days'")
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }
}
