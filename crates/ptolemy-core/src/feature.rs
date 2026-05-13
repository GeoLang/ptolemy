// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A spatial feature: geometry + attributes, identified by UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub geometry_wkb: Vec<u8>,
    pub properties: serde_json::Value,
}
