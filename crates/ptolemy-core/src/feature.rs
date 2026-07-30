// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A spatial feature: geometry + attributes, identified by UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub geometry_wkb: Vec<u8>,
    pub properties: serde_json::Value,
    /// When the feature is true in the world, as opposed to `created_at`, which
    /// is when it was written. Half-open: [valid_from, valid_to). Either end may
    /// be None for an open range, and both None means no time was recorded.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub valid_from: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub valid_to: Option<OffsetDateTime>,
}
