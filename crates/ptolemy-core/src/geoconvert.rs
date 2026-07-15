// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Geometry conversions shared by data store plugins: standard WKB (as stored
//! in `Feature::geometry_wkb`, srid 4326) to and from GeoJSON, plus a bbox
//! extractor. Backends that speak GeoJSON (mongodb, elasticsearch) reuse these
//! so the encoding stays identical across stores.

use crate::DataStoreError;
use geozero::{CoordDimensions, GeomProcessor, ToJson, ToWkb};
use serde_json::Value;

/// Convert standard WKB to a GeoJSON geometry value.
pub fn wkb_to_geojson(wkb: &[u8]) -> Result<Value, DataStoreError> {
    let json = geozero::wkb::Wkb(wkb)
        .to_json()
        .map_err(|e| DataStoreError::Internal(format!("wkb to geojson: {e}")))?;
    serde_json::from_str(&json).map_err(|e| DataStoreError::Internal(format!("parse geojson: {e}")))
}

/// Convert a GeoJSON geometry value to standard WKB (2d).
pub fn geojson_to_wkb(geom: &Value) -> Result<Vec<u8>, DataStoreError> {
    let s = geom.to_string();
    geozero::geojson::GeoJson(&s)
        .to_wkb(CoordDimensions::xy())
        .map_err(|e| DataStoreError::Internal(format!("geojson to wkb: {e}")))
}

/// Compute the [minx, miny, maxx, maxy] envelope of a WKB geometry.
pub fn wkb_bbox(wkb: &[u8]) -> Result<[f64; 4], DataStoreError> {
    use geozero::GeozeroGeometry;
    let mut proc = BboxProcessor::default();
    geozero::wkb::Wkb(wkb)
        .process_geom(&mut proc)
        .map_err(|e| DataStoreError::Internal(format!("wkb bbox: {e}")))?;
    proc.bbox()
        .ok_or_else(|| DataStoreError::Internal("empty geometry has no bbox".into()))
}

/// geozero processor that accumulates the coordinate envelope.
#[derive(Default)]
struct BboxProcessor {
    min_x: Option<f64>,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl BboxProcessor {
    fn bbox(&self) -> Option<[f64; 4]> {
        self.min_x
            .map(|min_x| [min_x, self.min_y, self.max_x, self.max_y])
    }
}

impl GeomProcessor for BboxProcessor {
    fn xy(&mut self, x: f64, y: f64, _idx: usize) -> geozero::error::Result<()> {
        match self.min_x {
            None => {
                self.min_x = Some(x);
                self.min_y = y;
                self.max_x = x;
                self.max_y = y;
            }
            Some(min_x) => {
                self.min_x = Some(min_x.min(x));
                self.min_y = self.min_y.min(y);
                self.max_x = self.max_x.max(x);
                self.max_y = self.max_y.max(y);
            }
        }
        Ok(())
    }
}
