This contains the work in progress design specification for the next version of saltcorn, called Saltcorn Feldspar (or just feldspar)

Files:

docs/GOALS.md - the goals for the next version
docs/Saltcorn1_description.md - a description of the current version of saltcorn
docs/TECHNICAL_DESIGN.md - a technical design document outlining the implementation plan. This is not set in stone - you can deviate from this plan if a better approach is possible. 
TODO.md - a plan with to-do items
CHANGELOG - a list of changes in reverse chronological order

Standard working mode:

1. Pick the most recent unfinished item on the todo list. You can grep for [ ] and [~]
2. Mark the item as in progess if it is not already do
3. Refer to the Technical design document (docs/TECHNICAL_DESIGN.md) if anything is not clear
4. Implement the work, making changes to the repository code
5. Write at least one test to assert that the changes work
6. Describe what you did in the changelog
7. Mark the item as complete
8. Do not commit to git. A human will review the work and commit.

## Tests: tear the catalog down

Services installed on a `Catalog` (triggers, models, mounts, …) hold it back, so a test that
builds a catalog must register its teardown, or everything it built — JS isolate threads,
runtimes, sockets — leaks for the rest of the test binary, which then fails with "Too many open
files" at the common soft limit of 1024. Right after `Catalog::init`:

    db.on_teardown(catalog.teardown_hook());   // db: the test's TestDb

A test with no `TestDb` (SQLite) holds a `sc_test_harness::Teardown` and calls `.add(...)` on it.
`crates/sc-server/tests/catalog_teardown.rs` checks that a full server stack is freed; extend it if
you add a service that holds the catalog in a new way. To reproduce a leak, run the tests after
`ulimit -Sn 1024`.

## Status

The system is now used in production. Any code changes you make must not break compatibility with running applications when those are upgraded to a new version of feldspar.

If the database needs a change the bootstrap cannot make (a rename, a type or nullability change,
rewriting stored data), add a migration to `crates/sc-migrate`, with a test that runs it with
`run_one`. Additive changes (a new table, a new nullable column) stay with the bootstrap. The
migration system is documented in docs/TECHNICAL_DESIGN.md §5.3 (the design: when migrations run,
how they are recorded, locking, new vs. older databases) and in the crate docs at the top of
`crates/sc-migrate/src/lib.rs` (how to write one: ids, SQL vs. Rust bodies, why a migration must
not use the catalog). See the tests in `crates/sc-migrate/tests/migrate.rs` for examples of both
kinds. `TABLES_RENAME.sql` is frozen: never add to it.
