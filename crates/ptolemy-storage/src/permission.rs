// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The rule that turns permission rows into a write decision.
//!
//! Compatibility rule: a scope with no permission rows at all does not enforce,
//! so a dataset that never had a grant keeps accepting writes from any editor
//! the role gate let through. The first grant flips it to enforced.
//!
//! The branch scope wins over the dataset scope: once a branch has rows, those
//! rows decide, and a dataset-level grant does not reach into it.

/// Permission hierarchy: admin > write > read. Anything else is no access.
pub fn permission_level(perm: &str) -> u8 {
    match perm {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

/// Who a write is attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Writer {
    /// Auth is off, or the caller is the CLI or an embedded use of the store.
    /// There is no identity to check, so the permission tables are not consulted.
    Unenforced,
    /// Auth is on but the request carried no verified identity. Cannot write.
    Anonymous,
    /// A caller whose id came from a verified token.
    User {
        id: String,
        /// Instance-wide admin role, which bypasses per-dataset permissions.
        instance_admin: bool,
    },
}

/// What the store has to do about a writer before it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check<'a> {
    /// Nothing to check.
    Skip,
    /// Refuse: auth is on and the caller is unauthenticated.
    Deny,
    /// Run the permission ladder for this id.
    Ladder(&'a str),
}

impl Writer {
    pub fn user(id: impl Into<String>, instance_admin: bool) -> Self {
        Writer::User {
            id: id.into(),
            instance_admin,
        }
    }

    pub fn check(&self) -> Check<'_> {
        match self {
            Writer::Unenforced => Check::Skip,
            Writer::Anonymous => Check::Deny,
            Writer::User { instance_admin, .. } if *instance_admin => Check::Skip,
            Writer::User { id, .. } => Check::Ladder(id),
        }
    }
}

/// One permission table's view of a writer, for a single dataset or branch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Whether the scope has any permission row at all.
    pub enforced: bool,
    /// The writer's own permission here, if it has a row.
    pub mine: Option<String>,
}

impl Scope {
    /// A scope with no rows, which never denies.
    pub fn open() -> Self {
        Scope::default()
    }
}

/// Whether a writer subject to enforcement may write, given the branch scope and
/// the dataset scope of the target.
pub fn write_allowed(branch: &Scope, dataset: &Scope) -> bool {
    if branch.enforced {
        return can_write(branch.mine.as_deref());
    }
    if dataset.enforced {
        return can_write(dataset.mine.as_deref());
    }
    true
}

fn can_write(perm: Option<&str>) -> bool {
    perm.is_some_and(|p| permission_level(p) >= permission_level("write"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted(perm: &str) -> Scope {
        Scope {
            enforced: true,
            mine: Some(perm.into()),
        }
    }

    /// Rows exist but none for this writer.
    fn enforced_without_me() -> Scope {
        Scope {
            enforced: true,
            mine: None,
        }
    }

    #[test]
    fn no_rows_anywhere_stays_open() {
        assert!(write_allowed(&Scope::open(), &Scope::open()));
    }

    #[test]
    fn dataset_grant_decides_when_the_branch_has_no_rows() {
        assert!(write_allowed(&Scope::open(), &granted("write")));
        assert!(write_allowed(&Scope::open(), &granted("admin")));
        assert!(!write_allowed(&Scope::open(), &granted("read")));
        assert!(!write_allowed(&Scope::open(), &enforced_without_me()));
    }

    #[test]
    fn branch_rows_win_over_the_dataset() {
        // a dataset-level write grant does not reach into an enforced branch
        assert!(!write_allowed(&enforced_without_me(), &granted("write")));
        assert!(!write_allowed(&granted("read"), &granted("admin")));
        // and a branch grant stands on its own
        assert!(write_allowed(&granted("write"), &enforced_without_me()));
    }

    #[test]
    fn unknown_permission_strings_grant_nothing() {
        assert!(!write_allowed(&Scope::open(), &granted("Write")));
        assert!(!write_allowed(&Scope::open(), &granted("owner")));
        assert_eq!(permission_level("nonsense"), 0);
    }

    #[test]
    fn writer_check_maps_each_case() {
        assert_eq!(Writer::Unenforced.check(), Check::Skip);
        assert_eq!(Writer::Anonymous.check(), Check::Deny);
        assert_eq!(Writer::user("u1", true).check(), Check::Skip);
        assert_eq!(Writer::user("u1", false).check(), Check::Ladder("u1"));
    }
}
