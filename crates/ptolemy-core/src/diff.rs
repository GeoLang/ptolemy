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

/// The geometry as the source recorded it, in the source's own CRS, kept
/// because reprojecting to 4326 moves every vertex and a survey-grade consumer
/// wants the coordinates that were measured, not recomputed ones.
///
/// One construction path: [`NativeGeometry::new`] refuses srid 4326, so a
/// value of this type is always a *distinct* original. "No distinct original"
/// is said with `None`, never with a duplicate of the 4326 geometry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeGeometry {
    wkb: Vec<u8>,
    srid: i32,
}

impl NativeGeometry {
    /// None for 4326: a copy in the storage srid is not a distinct original.
    pub fn new(wkb: Vec<u8>, srid: i32) -> Option<Self> {
        (srid != 4326).then_some(NativeGeometry { wkb, srid })
    }

    pub fn wkb(&self) -> &[u8] {
        &self.wkb
    }

    pub fn srid(&self) -> i32 {
        self.srid
    }
}

/// A single operation within a diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffOp {
    Insert {
        feature_id: Uuid,
        geometry_wkb: Vec<u8>,
        properties: serde_json::Value,
        /// The pre-reprojection original, when the source had one. Stored as
        /// written, never updated: an edit's new version carries None.
        #[serde(default)]
        native: Option<NativeGeometry>,
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
        /// Unlike geometry and properties, None is stored as NULL rather than
        /// inherited: an edited shape has no original, and inheriting one would
        /// claim the old survey measured the new geometry.
        #[serde(default)]
        native: Option<NativeGeometry>,
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
