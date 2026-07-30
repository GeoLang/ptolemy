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
