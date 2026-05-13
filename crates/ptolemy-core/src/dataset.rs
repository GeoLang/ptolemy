// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
