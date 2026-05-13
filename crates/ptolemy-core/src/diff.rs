// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Deserialize, Serialize};
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
    },
    Update {
        feature_id: Uuid,
        geometry_wkb: Option<Vec<u8>>,
        properties: Option<serde_json::Value>,
    },
    Delete {
        feature_id: Uuid,
    },
}
