//! Every integration test in this crate, in one binary.
//!
//! Each file below is still an ordinary test file — it is pulled in as a module
//! rather than compiled as its own target. The workspace statically links V8 into
//! every test binary, so a target per file cost ~400 MB of disk and a link each;
//! CI ran out of disk on the link (`ld terminated with signal 7`) before it ran
//! out of patience. Files stay where they are, so paths relative to a test file
//! (fixtures, `include_str!`, `#[path]`) are unaffected.
//!
//! Add a new test file and it is picked up here — the list is the whole wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "admin_client_sync.rs"]
mod admin_client_sync;
#[path = "calc_after_read.rs"]
mod calc_after_read;
#[path = "calc_fields.rs"]
mod calc_fields;
#[path = "code_host_as_user.rs"]
mod code_host_as_user;
#[path = "code_host_reads.rs"]
mod code_host_reads;
#[path = "code_host_sql.rs"]
mod code_host_sql;
#[path = "code_host_writes.rs"]
mod code_host_writes;
#[path = "constraints.rs"]
mod constraints;
#[path = "custom_queries.rs"]
mod custom_queries;
#[path = "file_field_write.rs"]
mod file_field_write;
#[path = "generated_client_csrf.rs"]
mod generated_client_csrf;
#[path = "graphql_aggregates.rs"]
mod graphql_aggregates;
#[path = "graphql_authz.rs"]
mod graphql_authz;
#[path = "graphql_children.rs"]
mod graphql_children;
#[path = "graphql_mutations.rs"]
mod graphql_mutations;
#[path = "graphql_rows.rs"]
mod graphql_rows;
#[path = "graphql_schema.rs"]
mod graphql_schema;
#[path = "query_params.rs"]
mod query_params;
#[path = "rest_auth.rs"]
mod rest_auth;
#[path = "rest_provider.rs"]
mod rest_provider;
#[path = "rest_query.rs"]
mod rest_query;
#[path = "rich_type_write_path.rs"]
mod rich_type_write_path;
#[path = "sqlite_rows.rs"]
mod sqlite_rows;
#[path = "typescript_typecheck.rs"]
mod typescript_typecheck;
#[path = "v1_table_reads.rs"]
mod v1_table_reads;
#[path = "v1_table_writes.rs"]
mod v1_table_writes;
