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
    // signing secret must never leave the server in a response
    #[serde(skip_serializing)]
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

/// The event types the server emits, which is also the set a subscription may
/// name. A variant here with no emission point is a promise the server does not
/// keep, so [`EventType::ALL`] is checked against what a live server actually
/// writes: see `test_every_advertised_event_type_is_emitted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Commit,
    Merge,
    BranchCreated,
    SchemaChanged,
}

impl EventType {
    pub const ALL: [EventType; 4] = [
        EventType::Commit,
        EventType::Merge,
        EventType::BranchCreated,
        EventType::SchemaChanged,
    ];
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EventType::Commit => "commit",
            EventType::Merge => "merge",
            EventType::BranchCreated => "branch_created",
            EventType::SchemaChanged => "schema_changed",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is written out by hand, so it can fall behind the enum. The wire
    /// name is what a subscription filter is matched against.
    #[test]
    fn all_names_every_variant_once() {
        let mut names: Vec<String> = EventType::ALL.iter().map(|t| t.to_string()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), EventType::ALL.len());
        assert_eq!(
            names,
            ["branch_created", "commit", "merge", "schema_changed"]
        );
    }
}
