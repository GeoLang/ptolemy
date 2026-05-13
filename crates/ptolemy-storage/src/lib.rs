// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

pub mod postgres;

use ptolemy_core::{Branch, Changeset, Dataset, Feature};
use uuid::Uuid;

/// Trait for the versioned feature store backend.
pub trait Store: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    // Dataset operations
    fn create_dataset(&self, dataset: &Dataset) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send;
    fn get_dataset(&self, id: Uuid) -> impl std::future::Future<Output = Result<Option<Dataset>, Self::Error>> + Send;
    fn list_datasets(&self) -> impl std::future::Future<Output = Result<Vec<Dataset>, Self::Error>> + Send;

    // Branch operations
    fn create_branch(&self, branch: &Branch) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send;
    fn get_branch(&self, id: Uuid) -> impl std::future::Future<Output = Result<Option<Branch>, Self::Error>> + Send;
    fn list_branches(&self, dataset_id: Uuid) -> impl std::future::Future<Output = Result<Vec<Branch>, Self::Error>> + Send;

    // Changeset operations
    fn create_changeset(&self, changeset: &Changeset) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send;
    fn get_changeset(&self, id: Uuid) -> impl std::future::Future<Output = Result<Option<Changeset>, Self::Error>> + Send;
    fn get_branch_history(&self, branch_id: Uuid) -> impl std::future::Future<Output = Result<Vec<Changeset>, Self::Error>> + Send;

    // Feature operations (scoped to a changeset/branch head)
    fn insert_feature(&self, changeset_id: Uuid, feature: &Feature) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send;
    fn get_feature(&self, changeset_id: Uuid, feature_id: Uuid) -> impl std::future::Future<Output = Result<Option<Feature>, Self::Error>> + Send;
    fn list_features(&self, changeset_id: Uuid, dataset_id: Uuid) -> impl std::future::Future<Output = Result<Vec<Feature>, Self::Error>> + Send;
}
