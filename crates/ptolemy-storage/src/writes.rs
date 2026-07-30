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
