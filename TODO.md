# TODO

Findings from the adversarial review of the project-role fold, none block the
feature:

- [ ] Detach requires project editor on the project the dataset is leaving, so
      a dataset admin outside that project cannot undo an attach. The access an
      attach grants is the one grant not every dataset admin can revoke.
- [ ] A tool token with `ptolemy:write` can call PUT/DELETE
      `/api/v1/datasets/{id}/project` even though `tool_scope` refuses tool
      tokens on every `/api/v1/workspaces` and `/api/v1/projects` route. Decide
      whether attach/detach should refuse them too.
- [ ] Attach checks `require_dataset_admin` and `require_project_editor`
      outside the transaction and has no expected-previous guard like detach's
      `expected_project_id`, so a role revoked mid-request can still land the
      update.
- [ ] With auth off, an external read-only dataset can be attached: both rbac
      guards return Ok when the actor does not enforce, and the store method
      has no external check.
- [ ] Re-attaching a dataset to the project it is already in re-sets
      `visibility` to private, silently re-hiding a dataset an admin
      deliberately published.
- [ ] `idx_datasets_project` is unused, no query filters `datasets` by
      `project_id`.
- [ ] The project term in `visible_datasets_sql` is a correlated subplan that
      runs per candidate row even when `project_id` is null: listing went from
      102 ms to 236 ms at 5000 datasets. Rewrite as a join if listings grow.
- [ ] Moving a dataset from project A to project B needs no consent from A:
      dataset admin plus editor on B suffices. Decide whether A should have a
      say.
