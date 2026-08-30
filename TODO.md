# TODO

Findings from the adversarial review of the project-role fold, none block the
feature:

- [ ] `idx_datasets_project` is unused, no query filters `datasets` by
      `project_id`.
- [ ] The project term in `visible_datasets_sql` is a correlated subplan that
      runs per candidate row even when `project_id` is null: listing went from
      102 ms to 236 ms at 5000 datasets. Rewrite as a join if listings grow.
