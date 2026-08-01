// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! One mapping from a store error to an HTTP response, and one place a database
//! error is logged.
//!
//! Every route module wraps [`StoreError`] in its own error type. They all
//! translate it here, so a new variant cannot silently come out as a 500 in one
//! module and a 403 in another, and a database error message never reaches the
//! client.
//!
//! Because the client only ever sees `internal error`, the log line is the only
//! record of what went wrong, and [`log_db_error`] is what writes it. It carries
//! the SQLSTATE, which is what `tests/route_sweep.rs` reads to fail the build on
//! a handler that queries a column or a table the migrations never create.

use axum::http::StatusCode;
use ptolemy_storage::StoreError;

/// A query naming a column no relation has.
pub const UNDEFINED_COLUMN: &str = "42703";
/// A query naming a relation that does not exist.
pub const UNDEFINED_TABLE: &str = "42P01";

/// The SQLSTATE a database error carries, or `-` when it carries none.
///
/// `ColumnNotFound` never reaches the server and so has no SQLSTATE of its own,
/// but it is the same mistake as [`UNDEFINED_COLUMN`]: the handler asked a row
/// for a name the result set does not have. It reports as 42703 so one check
/// catches both.
pub fn db_sqlstate(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db.code().map(|c| c.into_owned()),
        sqlx::Error::ColumnNotFound(_) => Some(UNDEFINED_COLUMN.to_string()),
        _ => None,
    }
    .unwrap_or_else(|| "-".to_string())
}

/// Log a database error under the module that hit it. `context` is the module
/// name, so a log line says where the query lives.
pub fn log_db_error(context: &str, e: &sqlx::Error) {
    tracing::error!(sqlstate = %db_sqlstate(e), "{context} database error: {e}");
}

pub fn store_error_status(e: &StoreError) -> (StatusCode, String) {
    match e {
        StoreError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
        StoreError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
        StoreError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
        StoreError::Db(e) => {
            log_db_error("store", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        }
    }
}
