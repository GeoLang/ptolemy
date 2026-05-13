// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A changeset is an atomic unit of edits to a dataset branch.
/// Forms a DAG (directed acyclic graph) via parent pointers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changeset {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub message: String,
    pub author: String,
    pub created_at: OffsetDateTime,
}
