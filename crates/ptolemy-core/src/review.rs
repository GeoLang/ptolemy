// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Merge request (review) data types for pull-request-style geodata review.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A merge request proposes merging one branch into another, with review workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequest {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub source_branch_id: Uuid,
    pub target_branch_id: Uuid,
    pub title: String,
    pub description: String,
    pub author: String,
    pub status: MergeRequestStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeRequestStatus {
    Open,
    Approved,
    Merged,
    Closed,
}

/// A comment on a merge request, optionally linked to a specific feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: Uuid,
    pub merge_request_id: Uuid,
    /// If set, this comment is about a specific feature change
    pub feature_id: Option<Uuid>,
    pub author: String,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}
