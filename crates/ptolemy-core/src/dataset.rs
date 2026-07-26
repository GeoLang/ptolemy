// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use crate::external::ExternalTable;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A dataset is a collection of spatial features with a shared schema.
/// Analogous to a "feature class" in Esri or a table in PostGIS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    pub id: Uuid,
    pub name: String,
    pub srid: i32,
    pub geometry_type: GeometryType,
    pub created_at: OffsetDateTime,
    pub created_by: String,
    /// Set when the dataset is a read-only view over a PostGIS relation
    /// ptolemy does not own. Omitted from JSON for ordinary datasets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalTable>,
    #[serde(default)]
    pub visibility: Visibility,
}

/// Who may read a dataset's content. `Private` is enforced only when auth is
/// on: reads then need a permission row on the dataset or one of its branches.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Public,
    Private,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }

    /// Unknown strings are rejected rather than defaulted, so a typo cannot
    /// silently publish a dataset that was meant to be private.
    pub fn parse(s: &str) -> Option<Visibility> {
        match s {
            "public" => Some(Visibility::Public),
            "private" => Some(Visibility::Private),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GeometryType {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    GeometryCollection,
}
