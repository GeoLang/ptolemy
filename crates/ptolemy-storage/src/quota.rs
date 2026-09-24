// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use crate::StoreError;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub const MAX_ATTACHMENT_MEGABYTES_PER_USER: &str = "PTOLEMY_MAX_ATTACHMENT_MEGABYTES_PER_USER";
pub const MAX_ATTACHMENTS_PER_USER: &str = "PTOLEMY_MAX_ATTACHMENTS_PER_USER";
pub const MAX_WORKSPACES_PER_USER: &str = "PTOLEMY_MAX_WORKSPACES_PER_USER";
pub const MAX_PROJECTS_PER_USER: &str = "PTOLEMY_MAX_PROJECTS_PER_USER";
pub const MAX_INVITATIONS_PER_USER: &str = "PTOLEMY_MAX_INVITATIONS_PER_USER";
pub const MAX_MEMBERS_PER_WORKSPACE: &str = "PTOLEMY_MAX_MEMBERS_PER_WORKSPACE";
pub const MAX_MEMBERS_PER_PROJECT: &str = "PTOLEMY_MAX_MEMBERS_PER_PROJECT";
pub const MAX_STATE_KEYS_PER_PROJECT: &str = "PTOLEMY_MAX_STATE_KEYS_PER_PROJECT";

const BYTES_PER_MEGABYTE: i64 = 1024 * 1024;

// arbitrary and fixed, apart from the arcgis object id lock space
const USER_QUOTA_LOCK_SPACE: i32 = 0x0A_7E_57_0E;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserQuotas {
    pub attachment_bytes_per_user: Option<i64>,
    pub attachments_per_user: Option<i64>,
    pub workspaces_per_user: Option<i64>,
    pub projects_per_user: Option<i64>,
    pub invitations_per_user: Option<i64>,
    pub members_per_workspace: Option<i64>,
    pub members_per_project: Option<i64>,
    pub state_keys_per_project: Option<i64>,
}

impl UserQuotas {
    pub fn from_env() -> Result<Self, String> {
        Self::resolve(|name| std::env::var(name).ok())
    }

    pub fn resolve(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let limit = |name: &str| parse_limit(name, lookup(name).as_deref());
        let attachment_bytes_per_user = limit(MAX_ATTACHMENT_MEGABYTES_PER_USER)?
            .map(|megabytes| {
                megabytes.checked_mul(BYTES_PER_MEGABYTE).ok_or_else(|| {
                    format!("{MAX_ATTACHMENT_MEGABYTES_PER_USER}={megabytes} is too large")
                })
            })
            .transpose()?;
        Ok(Self {
            attachment_bytes_per_user,
            attachments_per_user: limit(MAX_ATTACHMENTS_PER_USER)?,
            workspaces_per_user: limit(MAX_WORKSPACES_PER_USER)?,
            projects_per_user: limit(MAX_PROJECTS_PER_USER)?,
            invitations_per_user: limit(MAX_INVITATIONS_PER_USER)?,
            members_per_workspace: limit(MAX_MEMBERS_PER_WORKSPACE)?,
            members_per_project: limit(MAX_MEMBERS_PER_PROJECT)?,
            state_keys_per_project: limit(MAX_STATE_KEYS_PER_PROJECT)?,
        })
    }
}

// a typo must not silently turn a limit off
fn parse_limit(name: &str, raw: Option<&str>) -> Result<Option<i64>, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|limit| *limit >= 0)
            .map(Some)
            .ok_or_else(|| format!("{name}={value} is not a whole number, unset it for no limit")),
    }
}

// take it before any row lock in the transaction, or an attachment insert can deadlock with it
pub(crate) async fn lock_user_quota(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(USER_QUOTA_LOCK_SPACE)
        .bind(user_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) fn quota_refusal(limit: i64, what: &str, detail: &str) -> StoreError {
    StoreError::Forbidden(format!(
        "quota reached: the limit on {what} is {limit}{detail}"
    ))
}

fn refuse_at_limit(used: i64, limit: i64, what: &str) -> Result<(), StoreError> {
    if used >= limit {
        return Err(quota_refusal(limit, what, ""));
    }
    Ok(())
}

pub(crate) async fn ensure_user_row_quota(
    tx: &mut Transaction<'_, Postgres>,
    limit: Option<i64>,
    count_by_user_sql: &str,
    user_id: &str,
    what: &str,
) -> Result<(), StoreError> {
    let Some(limit) = limit else {
        return Ok(());
    };
    lock_user_quota(tx, user_id).await?;
    let used: i64 = sqlx::query_scalar(count_by_user_sql)
        .bind(user_id)
        .fetch_one(&mut **tx)
        .await?;
    refuse_at_limit(used, limit, what)
}

// the caller holds the row lock on the owning workspace or project
pub(crate) async fn ensure_room_for_row(
    tx: &mut Transaction<'_, Postgres>,
    limit: Option<i64>,
    count_others_sql: &str,
    owner_id: Uuid,
    row_key: &str,
    what: &str,
) -> Result<(), StoreError> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let others: i64 = sqlx::query_scalar(count_others_sql)
        .bind(owner_id)
        .bind(row_key)
        .fetch_one(&mut **tx)
        .await?;
    refuse_at_limit(others, limit, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_blank_mean_no_limit() {
        assert_eq!(parse_limit(MAX_WORKSPACES_PER_USER, None), Ok(None));
        assert_eq!(parse_limit(MAX_WORKSPACES_PER_USER, Some("  ")), Ok(None));
        assert_eq!(UserQuotas::resolve(|_| None), Ok(UserQuotas::default()));
    }

    #[test]
    fn a_count_is_read_and_zero_is_a_limit() {
        assert_eq!(
            parse_limit(MAX_WORKSPACES_PER_USER, Some(" 3 ")),
            Ok(Some(3))
        );
        assert_eq!(parse_limit(MAX_WORKSPACES_PER_USER, Some("0")), Ok(Some(0)));
    }

    #[test]
    fn anything_else_refuses_startup() {
        for raw in ["-1", "3.5", "three", "5OO"] {
            let error = parse_limit(MAX_WORKSPACES_PER_USER, Some(raw)).unwrap_err();
            assert!(error.contains(MAX_WORKSPACES_PER_USER), "{error}");
        }
    }

    #[test]
    fn attachment_megabytes_become_bytes() {
        let quotas = UserQuotas::resolve(|name| {
            (name == MAX_ATTACHMENT_MEGABYTES_PER_USER).then(|| "50".to_string())
        })
        .unwrap();
        assert_eq!(quotas.attachment_bytes_per_user, Some(50 * 1024 * 1024));
        assert_eq!(quotas.attachments_per_user, None);

        let overflow = UserQuotas::resolve(|name| {
            (name == MAX_ATTACHMENT_MEGABYTES_PER_USER).then(|| i64::MAX.to_string())
        });
        assert!(overflow.is_err());
    }
}
