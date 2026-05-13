// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Webhook and event types for CDC (Change Data Capture).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub url: String,
    pub events: Vec<String>,
    pub secret: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Standard event types emitted by the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Commit,
    Merge,
    BranchCreated,
    BranchDeleted,
    SchemaChanged,
    QualityAlert,
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EventType::Commit => "commit",
            EventType::Merge => "merge",
            EventType::BranchCreated => "branch_created",
            EventType::BranchDeleted => "branch_deleted",
            EventType::SchemaChanged => "schema_changed",
            EventType::QualityAlert => "quality_alert",
        };
        f.write_str(s)
    }
}
