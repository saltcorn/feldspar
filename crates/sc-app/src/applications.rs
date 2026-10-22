//! The `_fd_applications` table: its schema and one-time bootstrap (design §9,
//! §13.2).
//!
//! This is the **one stored-metadata table the MVP needs**, and the reason is
//! the distinction §9 draws between an overlay and a definition. `_fd_tables` /
//! `_fd_fields` are overlays: introspection already yields the tables and
//! fields, so a row only *adds* access rules and attributes, and a legacy
//! database needs none. An application has no such underlying reality — there is
//! nothing to introspect it from — so its row **is** the application.
//!
//! Being an `_fd_*` table it is a system table, hidden from users
//! ([`Table::is_system`](sc_catalog::Table::is_system)), and it obeys the §9
//! required columns: UUID [`id`](COL_ID), [`name`](COL_NAME),
//! [`description`](COL_DESCRIPTION), [`attributes`](COL_ATTRIBUTES).
//!
//! The remaining columns follow the §9 column-vs-attributes rule — *a value
//! present for many rows gets its own column; a sparse value goes into
//! `attributes`* — and every app has all of them, so each gets a column:
//! subdomain, framework, extra frameworks, the table/store subsets, the API and
//! static-dir lists, and the CSP. Only [`subdomain`](COL_SUBDOMAIN) is a scalar
//! the database can index and constrain; the rest are structured lists and maps,
//! stored as JSON. That is deliberate — they are read as a whole app or not at
//! all, and no query filters on them.
//!
//! Like [`sc_auth::bootstrap`], this goes through the [`Catalog`] rather than
//! issuing DDL directly, so it is driver-agnostic, and it invents no
//! database-side defaults: the UUID is generated in Rust when the app is saved.

use sc_catalog::{Catalog, DataField, Table};
use sc_error::Result;
use sc_types::{BasicType, TypeRef};

/// Name of the applications table in the primary database.
pub const APPLICATIONS_TABLE: &str = "_fd_applications";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The human-readable name column (§9).
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9). Nullable: a description is
/// genuinely optional, and a `NULL` reads back as the empty string.
pub const COL_DESCRIPTION: &str = "description";
/// The sparse per-app values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The subdomain the app is served on: the unique routing key (§13.2).
pub const COL_SUBDOMAIN: &str = "subdomain";
/// The primary framework reference (JSON `{name, config}`).
pub const COL_FRAMEWORK: &str = "framework";
/// Additional framework references (JSON array).
pub const COL_EXTRA_FRAMEWORKS: &str = "extra_frameworks";
/// The declared table subset (JSON array of table names).
pub const COL_TABLES: &str = "tables";
/// The declared file-store subset (JSON array of store names).
pub const COL_FILE_STORES: &str = "file_stores";
/// The exposed trigger subset (JSON array of trigger names).
///
/// **Nullable**, unlike its siblings, and that is a fact about *when* it was
/// added rather than about what it means. It arrived after `_fd_applications`
/// existed in the field, so [`bootstrap`] adds it to tables that already have
/// rows — and a `NOT NULL` column cannot be added to a table with rows in it
/// without inventing a value for them. `NULL` reads back as "this app exposes no
/// triggers", which is exactly what an app written before triggers existed did.
pub const COL_TRIGGERS: &str = "triggers";
/// The exposed stream subset (JSON array of stream names).
///
/// Nullable for [`COL_TRIGGERS`]' reason and no other: it arrived later still,
/// so [`bootstrap`] adds it to tables that already have rows, and a `NOT NULL`
/// column cannot be added to one of those without inventing a value. `NULL`
/// reads back as "this app exposes no streams".
pub const COL_STREAMS: &str = "streams";
/// The enabled API providers (JSON array of `{provider, mount}`).
pub const COL_APIS: &str = "apis";
/// The statically-served directories (JSON array of `{mount, store, path}`).
pub const COL_STATIC_DIRS: &str = "static_dirs";
/// The upload slots (JSON array of `{name, store, folder, …}`, TODO F2 §2.3).
///
/// Nullable for [`COL_TRIGGERS`]' reason: it arrived after the table did, so
/// [`bootstrap`] adds it to tables that already have rows. `NULL` reads back as
/// "no upload slots".
pub const COL_UPLOADS: &str = "uploads";
/// The content-security policy (JSON object of directive → source list).
pub const COL_CSP: &str = "csp";
/// The least privileged role that may use the application (an integer,
/// `1..=100`).
///
/// Nullable for [`COL_TRIGGERS`]' reason: it arrived after the table did, so
/// [`bootstrap`] adds it to tables that already have rows. `NULL` reads back as
/// `100`, the public role — nobody refused.
pub const COL_MIN_ROLE: &str = "min_role";
/// The most privileged role a caller holds while using the application (an
/// integer, `1..=100`). Nullable as [`COL_MIN_ROLE`] is; `NULL` reads back as
/// `1` — nobody capped.
pub const COL_MAX_ROLE: &str = "max_role";
/// The formula a caller must satisfy to use the application (text). Nullable;
/// `NULL` reads back as no condition.
pub const COL_ACCESS_IF: &str = "access_if";

/// The fields of the `_fd_applications` table, in declaration order.
///
/// `subdomain` carries the `UNIQUE` constraint: routing dispatches on it, so two
/// apps claiming the same one is not a state the system can serve. Enforcing it
/// here means the database rejects it even when two admins save concurrently —
/// the in-memory mount registry cannot see the other transaction, so it is not
/// the place to be authoritative.
fn applications_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_SUBDOMAIN, text()).required().unique(),
        DataField::plain(COL_FRAMEWORK, json()).required(),
        DataField::plain(COL_EXTRA_FRAMEWORKS, json()).required(),
        DataField::plain(COL_TABLES, json()).required(),
        DataField::plain(COL_FILE_STORES, json()).required(),
        // Not `required`: see [`COL_TRIGGERS`] — it is reconciled onto existing
        // tables, and those rows have no value for it.
        DataField::plain(COL_TRIGGERS, json()),
        // Not `required`, for the reason above it: see [`COL_STREAMS`].
        DataField::plain(COL_STREAMS, json()),
        DataField::plain(COL_APIS, json()).required(),
        DataField::plain(COL_STATIC_DIRS, json()).required(),
        // Not `required`, for the reason above it: see [`COL_UPLOADS`].
        DataField::plain(COL_UPLOADS, json()),
        DataField::plain(COL_CSP, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        // Not `required`, for the reason above it: see [`COL_MIN_ROLE`].
        DataField::plain(COL_MIN_ROLE, int()),
        DataField::plain(COL_MAX_ROLE, int()),
        DataField::plain(COL_ACCESS_IF, text()),
    ]
}

/// Ensure the `_fd_applications` table exists, creating it if absent, and return
/// it.
///
/// Idempotent, and **additively reconciled**: a declared column an existing table
/// does not have is created ([`Catalog::bootstrap_table`]). The design bans a
/// migration framework for now, and without that the alternative for a column
/// added by a release — [`COL_TRIGGERS`] is the first — is that every existing
/// deployment breaks on the next read of a table that worked yesterday. Adding a
/// column is the part of migration that is always safe; nothing here drops,
/// renames or re-types anything.
///
/// Call this once at startup after the [`Catalog`] is initialised — including
/// against a database that has never seen Saltcorn, which is exactly the case the
/// table's absence covers: a legacy database bootstraps into one holding
/// applications without any migration step.
pub async fn bootstrap(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(APPLICATIONS_TABLE, &applications_fields())
        .await?;
    // An application's translations are part of what an application is (§16.1,
    // D4): they are created with it, deleted with it, and a deployment that can
    // hold an app can hold its catalogue. So they bootstrap together, rather
    // than leaving every call site to remember a second one.
    crate::i18n::bootstrap_translations(catalog).await?;
    // …and so are its uploads in progress (TODO F2, §2.3).
    crate::uploads::bootstrap_uploads(catalog).await?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = applications_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        // §9: every system metadata table MUST have id (UUID), name,
        // description, attributes (JSON object).
        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        // A description is optional; NULL reads back as "".
        assert!(!by_name(COL_DESCRIPTION).required);
    }

    #[test]
    fn subdomain_is_the_unique_routing_key() {
        let fields = applications_fields();
        let subdomain = fields
            .iter()
            .find(|f| f.base.name == COL_SUBDOMAIN)
            .unwrap();
        assert!(subdomain.required && subdomain.unique);
        assert_eq!(subdomain.base.type_, TypeRef::Basic(BasicType::Text));
    }

    #[test]
    fn the_structured_columns_are_json_and_required() {
        let fields = applications_fields();
        for name in [
            COL_FRAMEWORK,
            COL_EXTRA_FRAMEWORKS,
            COL_TABLES,
            COL_FILE_STORES,
            COL_APIS,
            COL_STATIC_DIRS,
            COL_CSP,
            COL_ATTRIBUTES,
        ] {
            let f = fields.iter().find(|f| f.base.name == name).unwrap();
            assert_eq!(f.base.type_, TypeRef::Basic(BasicType::Json), "{name}");
            // Every one is always written (an empty list is `[]`, not NULL), so
            // a reader never has to distinguish "absent" from "empty".
            assert!(f.required, "{name} should be NOT NULL");
        }
    }

    #[test]
    fn the_trigger_subset_is_json_but_nullable_because_it_arrived_later() {
        let fields = applications_fields();
        let triggers = fields.iter().find(|f| f.base.name == COL_TRIGGERS).unwrap();
        assert_eq!(triggers.base.type_, TypeRef::Basic(BasicType::Json));
        // The one exception to the rule above, and the reason is the additive
        // bootstrap: this column is created on tables that already have rows,
        // and NOT NULL would have no value to give them.
        assert!(!triggers.required);
    }

    #[test]
    fn the_stream_subset_is_json_but_nullable_for_the_same_reason() {
        let fields = applications_fields();
        let streams = fields.iter().find(|f| f.base.name == COL_STREAMS).unwrap();
        assert_eq!(streams.base.type_, TypeRef::Basic(BasicType::Json));
        assert!(!streams.required);
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(APPLICATIONS_TABLE.starts_with("_fd_"));
    }
}
