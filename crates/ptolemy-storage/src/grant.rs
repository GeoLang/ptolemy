// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The proof that the write ladder ran.
//!
//! [`WriteGrant`] is minted by [`crate::PgStore::ensure_id_writable`] and by
//! nothing else outside this crate, apart from the one dev-mode constructor
//! below. A store method that takes `&WriteGrant` therefore cannot run unless
//! the ladder passed.
//!
//! The grant carries the id the ladder was run against, and a guarded write
//! takes the id it writes under from the grant rather than from its own
//! arguments. That is what keeps a grant from being reused: holding one for a
//! dataset the caller owns cannot authorize a write to a dataset they do not.
//!
//! [`WriteGrant`] is not [`crate::Writer`] and the two must not be merged.
//! `Writer` is the input to the ladder — who is calling — and it is freely
//! constructible on purpose, because the CLI and embedded uses of the store
//! build a `Writer::Unenforced` for themselves. `WriteGrant` is the output, and
//! it is worth nothing unless it is unforgeable.

use uuid::Uuid;

/// Proof that the write ladder passed for one id, and the id it passed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteGrant {
    /// Private, and read-only through [`WriteGrant::id`]: nothing outside this
    /// module can forge a grant or repoint an existing one at another target.
    id: Uuid,
}

impl WriteGrant {
    /// Minted by the ladder, which is the only thing in this crate that calls
    /// it. Not public: outside `ptolemy-storage` the ladder is the sole source.
    pub(crate) fn issue(id: Uuid) -> Self {
        WriteGrant { id }
    }

    /// A grant for a request the ladder does not apply to, because auth is off
    /// and there is no verified identity to check permission rows against.
    ///
    /// This is the one construction path that skips the ladder, so it is named
    /// to stand out in review and in a grep. `ci/no-raw-writes.sh` refuses it
    /// anywhere in `ptolemy-api` except the write layer that owns the dev-mode
    /// decision.
    pub fn unenforced(id: Uuid) -> Self {
        WriteGrant { id }
    }

    /// The id the ladder was run against, and the only id a guarded write may
    /// scope itself by.
    pub fn id(&self) -> Uuid {
        self.id
    }
}
