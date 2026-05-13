// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! PostgreSQL/PostGIS backend for the versioned feature store.

use sqlx::PgPool;

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
