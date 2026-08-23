// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! The rule that turns permission rows into a write decision.
//!
//! A write needs a grant. No rows anywhere is not permission to write: an
//! enforced caller is denied, and only the instance admin role gets through,
//! which is who makes the first grant on such a dataset.
//!
//! The branch scope wins over the dataset scope: once a branch has rows, those
//! rows decide, and a dataset-level grant does not reach into it.
//!
//! A dataset attached to a project has a second source of grants: the caller's
//! effective role on that project, mapped by
//! [`CollaborationRole::dataset_permission`]. It joins the dataset scope as the
//! stronger of the two, never the branch scope, so the branch rule above still
//! holds.

use crate::workspace::{CollaborationRole, effective_project_role_sql};

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

/// Who a dataset listing is built for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reader {
    /// Sees every dataset whatever its visibility: auth is off, or the caller
    /// holds the instance admin role.
    pub bypass: bool,
    /// The caller id grants are matched against, `None` when anonymous.
    pub id: Option<String>,
}

/// SQL keeping only the datasets a [`Reader`] may see: public ones, private ones
/// the caller holds a grant on, directly or on one of their branches, and private
/// ones attached to a project the caller has any role on. `AND` it into every
/// listing that names datasets — the per-request visibility layer only covers
/// requests that name an id, so a listing has to filter itself.
///
/// `alias` is the datasets table's alias in the calling query, and the two
/// numbers are the 1-based positions of the binds carrying [`Reader::bypass`]
/// (bool) then [`Reader::id`] (text, NULL when anonymous). All three come from
/// the calling query, never from request data, so nothing a caller sends is
/// interpolated here.
///
/// The project role is the last term because it is the expensive one: a
/// correlated subquery per row, where the terms before it are a column test, a
/// bind and an index lookup. A dataset with no project has nothing to correlate
/// against and yields no role.
pub fn visible_datasets_sql(alias: &str, bypass_param: usize, caller_param: usize) -> String {
    let project_role = effective_project_role_sql(&format!("{alias}.project_id"), caller_param);
    format!(
        "({alias}.visibility = 'public' OR ${bypass_param} OR EXISTS (
             SELECT 1 FROM dataset_permissions dp
              WHERE dp.dataset_id = {alias}.id AND dp.user_id = ${caller_param}
            UNION ALL
             SELECT 1 FROM branch_permissions bp JOIN branches b ON b.id = bp.branch_id
              WHERE b.dataset_id = {alias}.id AND bp.user_id = ${caller_param})
          OR {project_role} IS NOT NULL)"
    )
}

/// The permission a caller holds on one dataset, given their explicit grant row
/// and the effective role they hold on the project the dataset is attached to.
///
/// Grants are additive and nothing denies, so the stronger of the two is what
/// they hold. `None` on both sides means no access, which is what the write
/// ladder refuses on.
pub fn stronger_permission(
    explicit: Option<String>,
    project: Option<CollaborationRole>,
) -> Option<String> {
    let from_project = project.map(CollaborationRole::dataset_permission);
    match (explicit, from_project) {
        (Some(explicit), Some(project)) => {
            if permission_level(project) > permission_level(&explicit) {
                Some(project.to_string())
            } else {
                Some(explicit)
            }
        }
        (Some(explicit), None) => Some(explicit),
        (None, Some(project)) => Some(project.to_string()),
        (None, None) => None,
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
    /// A scope with no rows, which grants nothing.
    pub fn empty() -> Self {
        Scope::default()
    }
}

/// Whether a writer subject to enforcement may write, given the branch scope and
/// the dataset scope of the target. Fails closed: no grant, no write.
pub fn write_allowed(branch: &Scope, dataset: &Scope) -> bool {
    if branch.enforced {
        return can_write(branch.mine.as_deref());
    }
    can_write(dataset.mine.as_deref())
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
    fn no_rows_anywhere_denies() {
        assert!(!write_allowed(&Scope::empty(), &Scope::empty()));
    }

    #[test]
    fn dataset_grant_decides_when_the_branch_has_no_rows() {
        assert!(write_allowed(&Scope::empty(), &granted("write")));
        assert!(write_allowed(&Scope::empty(), &granted("admin")));
        assert!(!write_allowed(&Scope::empty(), &granted("read")));
        assert!(!write_allowed(&Scope::empty(), &enforced_without_me()));
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
        assert!(!write_allowed(&Scope::empty(), &granted("Write")));
        assert!(!write_allowed(&Scope::empty(), &granted("owner")));
        assert_eq!(permission_level("nonsense"), 0);
    }

    /// A branch grant does not need the dataset to have rows of its own.
    #[test]
    fn branch_grant_stands_without_a_dataset_row() {
        assert!(write_allowed(&granted("write"), &Scope::empty()));
        assert!(!write_allowed(&enforced_without_me(), &Scope::empty()));
    }

    #[test]
    fn a_project_role_grants_on_its_own() {
        for (role, expected) in [
            (CollaborationRole::Viewer, "read"),
            (CollaborationRole::Editor, "write"),
            (CollaborationRole::Owner, "admin"),
        ] {
            assert_eq!(
                stronger_permission(None, Some(role)).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(stronger_permission(None, None), None);
    }

    /// The whole point of taking the stronger: neither side can demote the other,
    /// whichever way round they are.
    #[test]
    fn neither_side_demotes_the_other() {
        assert_eq!(
            stronger_permission(Some("read".into()), Some(CollaborationRole::Owner)).as_deref(),
            Some("admin")
        );
        assert_eq!(
            stronger_permission(Some("admin".into()), Some(CollaborationRole::Viewer)).as_deref(),
            Some("admin")
        );
        assert_eq!(
            stronger_permission(Some("write".into()), Some(CollaborationRole::Editor)).as_deref(),
            Some("write")
        );
    }

    /// A project role folds into the dataset scope, so it decides a write on its
    /// own, and it still gives way to a branch that has rows of its own.
    #[test]
    fn a_project_role_writes_through_the_dataset_scope() {
        let editor = Scope {
            enforced: false,
            mine: stronger_permission(None, Some(CollaborationRole::Editor)),
        };
        assert!(write_allowed(&Scope::empty(), &editor));
        assert!(!write_allowed(&enforced_without_me(), &editor));

        let viewer = Scope {
            enforced: false,
            mine: stronger_permission(None, Some(CollaborationRole::Viewer)),
        };
        assert!(!write_allowed(&Scope::empty(), &viewer));
    }

    /// The two enforcement sites read the same string as the SQL builders, so a
    /// renamed level would break the mapping loudly rather than granting nothing.
    #[test]
    fn every_mapped_permission_is_a_known_level() {
        for role in [
            CollaborationRole::Owner,
            CollaborationRole::Editor,
            CollaborationRole::Viewer,
        ] {
            assert!(permission_level(role.dataset_permission()) > 0, "{role:?}");
        }
    }

    #[test]
    fn writer_check_maps_each_case() {
        assert_eq!(Writer::Unenforced.check(), Check::Skip);
        assert_eq!(Writer::Anonymous.check(), Check::Deny);
        assert_eq!(Writer::user("u1", true).check(), Check::Skip);
        assert_eq!(Writer::user("u1", false).check(), Check::Ladder("u1"));
    }
}
