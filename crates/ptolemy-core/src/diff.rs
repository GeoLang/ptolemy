// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Represents the diff between two changesets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diff {
    pub from_changeset: Option<Uuid>,
    pub to_changeset: Uuid,
    pub operations: Vec<DiffOp>,
}

/// A single operation within a diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffOp {
    Insert {
        feature_id: Uuid,
        geometry_wkb: Vec<u8>,
        properties: serde_json::Value,
        /// Valid time of the new version; see [`crate::feature::Feature`].
        #[serde(default, with = "time::serde::rfc3339::option")]
        valid_from: Option<OffsetDateTime>,
        #[serde(default, with = "time::serde::rfc3339::option")]
        valid_to: Option<OffsetDateTime>,
    },
    Update {
        feature_id: Uuid,
        geometry_wkb: Option<Vec<u8>>,
        properties: Option<serde_json::Value>,
        /// Both None keeps the previous version's valid time, the same way an
        /// omitted geometry or properties is inherited.
        #[serde(default, with = "time::serde::rfc3339::option")]
        valid_from: Option<OffsetDateTime>,
        #[serde(default, with = "time::serde::rfc3339::option")]
        valid_to: Option<OffsetDateTime>,
    },
    Delete {
        feature_id: Uuid,
    },
}
