// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A named branch of a dataset, analogous to a git branch.
/// Each branch maintains a pointer to its latest changeset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub name: String,
    pub head: Option<Uuid>, // points to latest Changeset
    pub created_at: OffsetDateTime,
    pub created_by: String,
}
