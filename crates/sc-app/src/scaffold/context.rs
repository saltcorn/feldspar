//! The application, as JSON, for a framework that generates its project in a
//! guest language.
//!
//! [`files`](super::files) is the React generator: it holds a [`ProjectContext`]
//! and writes TypeScript. A **declared** framework (`crate::declared`) writes its
//! project in JavaScript, on a module worker, so what it gets is the same context
//! serialised — and this module is that serialisation.
//!
//! # It carries the derivations, not just the data
//!
//! The obvious version of this is "the tables and the endpoint set as JSON, let
//! the module work it out". That is the version that produces a project which
//! does not compile. Whether a page may offer a delete button, what a table hangs
//! off the generated client as, which columns a create form asks for and which
//! the database issues — each is a question `files.rs` answers *by asking the
//! endpoint set*, and each has a wrong answer that looks right. A module
//! re-deriving them from table shapes would agree with the generated client only
//! by coincidence, and stop agreeing at the first table whose name collides with
//! an endpoint or whose key is a UUID.
//!
//! So the answers cross, not the evidence: `ops`, `client`, `form.inputs`,
//! `form.minted`, `pk_ts_type`. The module writes the framework's idiom over
//! facts this crate established.

use sc_api::EndpointSet;
use sc_catalog::{DataField, Table};
use sc_types::BasicType;
use serde_json::{Value as Json, json};

use super::files::{
    CreateForm, ProjectContext, basic_type, client_object, exposed_tables, has_auth, has_op,
    key_ts_type, pascal, shown_fields, single_pk, storable, title, ts_empty,
};

/// The whole context one call to a declared framework's generator receives.
///
/// `runtime` and `client` are **project-relative**, like every path a generator
/// answers with: the module writes `${ctx.runtime}/composables.ts` and never has
/// to know where in the file store the project sits.
pub fn context_json(ctx: &ProjectContext<'_>, runtime: &str, client: &str) -> Json {
    let exposed = exposed_tables(ctx.tables, ctx.endpoints);
    json!({
        "project": ctx.project,
        "name": ctx.project_name(),
        "runtime": runtime,
        "client": client,
        "auth": has_auth(ctx.endpoints),
        // A table is synced and the client can keep it offline: the framework
        // passes the client a storage for its copy.
        "offline": sc_api::has_offline_sync(ctx.endpoints),
        "app": {
            "name": ctx.app.name,
            "subdomain": ctx.app.subdomain.trim(),
            "description": ctx.app.description,
            "url": ctx.app_url_or_placeholder(),
        },
        // The framework's own settings as the admin filled them in: a setting
        // is the framework's to declare, so it is the framework's to read when
        // it generates — the store and project as much as anything it added.
        "settings": Json::Object(ctx.app.framework.config.clone()),
        "tables": exposed
            .iter()
            .map(|t| table_json(t, ctx.endpoints))
            .collect::<Vec<_>>(),
        "graphql": ctx.graphql.map(|g| json!({ "mount": g.mount, "sdl": g.sdl })),
        "schema_sql": ctx.schema_sql,
        "roles": ctx.roles
            .iter()
            .map(|r| json!({ "name": r.name, "role": r.role, "description": r.description }))
            .collect::<Vec<_>>(),
        "skill": ctx.skill,
    })
}

/// One exposed table: its names, its key, what the API lets a page do to it, and
/// the create form this crate worked out.
fn table_json(table: &Table, endpoints: &EndpointSet) -> Json {
    let name = &table.name;
    let form = CreateForm::of(table, endpoints);
    json!({
        "name": name,
        "pascal": pascal(name),
        "title": title(name),
        // The property the table hangs off the generated client under — asked of
        // the generator, because it is the one that resolves a table whose name
        // collides with an endpoint's.
        "client": client_object(endpoints, name),
        "pk": single_pk(table),
        "pk_ts_type": key_ts_type(endpoints, name),
        // Kept on the device by a client created with `offline` (§14.11).
        "synced": table.sync_enabled(),
        // Whether an optimistic store can be built over it: it has an
        // addressable row and the app exposes the writes.
        "storable": storable(table, endpoints),
        "ops": {
            "list": has_op(endpoints, "list", name),
            "get": has_op(endpoints, "get", name),
            "create": has_op(endpoints, "create", name),
            "update": has_op(endpoints, "update", name),
            "delete": has_op(endpoints, "delete", name),
        },
        "fields": shown_fields(table, endpoints).into_iter().map(|f| field_json(f, endpoints, name)).collect::<Vec<_>>(),
        "form": {
            "inputs": form.inputs.iter().map(|f| field_json(f, endpoints, name)).collect::<Vec<_>>(),
            // Required UUID keys the page mints at submit time. Typing one into a
            // text box is not a thing anyone should have to do.
            "minted": form.minted.iter().map(|f| f.base.name.clone()).collect::<Vec<_>>(),
        },
    })
}

/// One column: what a table shows, and what a form control needs to know.
///
/// `ts_type` is read off the **endpoint set's** model of the column, exactly as
/// `key_ts_type` reads the key's: that is the type the generated client declares,
/// and a second mapping here would be free to disagree with it — which is a form
/// that does not compile against the client it posts through.
fn field_json(field: &DataField, endpoints: &EndpointSet, table: &str) -> Json {
    let ts_type = endpoints
        .resource(table)
        .and_then(|r| r.fields.iter().find(|f| f.name == field.base.name))
        .map_or("string", |f| f.ty.ts_type());
    json!({
        "name": field.base.name,
        "label": field.base.label,
        // Which control a form draws: a checkbox, a number box, or a text box.
        "control": control_kind(field),
        "ts_type": ts_type,
        // The value a blank form starts this column at — this crate's, so a
        // generated form and the generated client cannot disagree about a column.
        "empty": ts_empty(field),
        "required": field.required,
        "primary_key": field.primary_key,
    })
}

/// The three shapes a create-form control comes in, named rather than derived
/// from a type spelling the module would have to enumerate.
///
/// `form_control` in the React generator switches on exactly this, and the point
/// of naming it here is that a framework's generator writes an idiom rather than
/// a type table: a fourth control kind is added once, in this crate, and every
/// declared framework can render it.
fn control_kind(field: &DataField) -> &'static str {
    match basic_type(field) {
        BasicType::Bool => "checkbox",
        BasicType::Int | BasicType::Float | BasicType::Decimal => "number",
        _ => "text",
    }
}
