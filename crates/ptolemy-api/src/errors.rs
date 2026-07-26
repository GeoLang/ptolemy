// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! One mapping from a store error to an HTTP response.
//!
//! Every route module wraps [`StoreError`] in its own error type. They all
//! translate it here, so a new variant cannot silently come out as a 500 in one
//! module and a 403 in another, and a database error message never reaches the
//! client.

use axum::http::StatusCode;
use ptolemy_storage::StoreError;

pub fn store_error_status(e: &StoreError) -> (StatusCode, String) {
    match e {
        StoreError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
        StoreError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
        StoreError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
        StoreError::Db(e) => {
            tracing::error!("database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        }
    }
}
