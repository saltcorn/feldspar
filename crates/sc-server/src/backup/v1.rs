//! Reading a **Saltcorn 1** backup.
//!
//! A v1 archive is a different file with the same job: `pack.json` holds the
//! application's metadata (tables, their fields, triggers, roles — and a good
//! deal this system has no counterpart for), `tables/<name>.json` holds the rows,
//! `files.csv` plus `files/<name>` hold the uploads, and `config/<key>` holds one
//! setting each.
//!
//! **Nothing here restores anything.** This module *translates*: it reads a v1
//! archive and produces the entries a Feldspar backup would have had
//! ([`convert`]), and the restorer then does what it always does. That is the
//! whole design. A second restore path would be a second set of answers to "what
//! happens when the table is already here", "which parser validates this field",
//! "what does the report say" — and the answers would drift. This way an import
//! is inspected by the same dialog, narrowed by the same selection, and reported
//! in the same lines as any other restore.
//!
//! ## What is imported, and what is not
//!
//! The kinds this system has: **tables**, their **fields** (calculated ones
//! translated, [`formulas`]), their **rows**, **triggers** (v1 calls them
//! actions) and **workflows** ([`workflows`]), **agents** with the LLM provider,
//! MCP servers and memories they use ([`agents`]), **files** and **users** — and the
//! **views**, **pages**, **library** and **menu**, which become one **Saltcorn
//! UI application** named after the site (TODO "Saltcorn UI" §13, "The builder"
//! §8). A view's configuration, a page's layout and a library item's layout cross
//! unchanged apart from the two rewrites below: they are what v1's own `list.ts`
//! reads, and the Saltcorn UI framework runs v1's own `list.ts`.
//! v1's page groups, tags, models, plugins and event logs have no counterpart,
//! and a half-translation of them would be worse than their absence.
//! Everything left out — every field whose type has no counterpart, every menu
//! entry that opens v1's admin UI — becomes a line in the restore report rather
//! than a silence.
//!
//! ## Translations that are not mechanical
//!
//! - **Users are keyed differently.** v1 numbers users; this system gives them
//!   UUIDs (§7.1). Each imported account therefore gets a fresh UUID *here*, in
//!   the converter, so that every foreign key pointing at it in every other table
//!   can be rewritten to match in the same pass — and keeps its old number in a
//!   `legacy_id` column, which is the only way to recognise the same person in
//!   the v1 database afterwards.
//! - **Files need a store.** v1 has one file area per application; this system
//!   has named stores (§6). The uploads are put into a local store named after
//!   the application (`site_name`), created if it is not already there — and left
//!   exactly as it is if it is, like every other restored store.
//! - **Library items are keyed differently too** (TODO "The builder" §8). A v1
//!   layout places an item as `{ type: "library", library_id: 3 }`, a serial of
//!   v1's `_sc_library`; here an item's id is a UUID. Each item gets one here, and
//!   every `library_id` in every view's configuration, page's layout and item's
//!   own layout is rewritten to it in the same pass ([`LibraryKeys`]).
//! - **A page's legacy fixed states are folded into its layout** (§7). v1 has
//!   two spellings of an embedded view's fixed state: `configuration` on the
//!   `view` segment, which the builder writes, and the page's `fixed_states`,
//!   which older builders wrote and v1's page editor folds into the segments
//!   before opening the builder. The import does that fold once, and
//!   `fixed_states` is not kept, so this system has one spelling.
//!
//! Those two rewrites are the only places where "a layout crosses unchanged"
//! gives way, and each is v1's own reading of the data rather than a new one.
//!
//! Password hashes **are** carried: v1 hashes with bcrypt, and this system
//! verifies a bcrypt hash as well as its own argon2id, deciding by the hash
//! (`sc_auth::HashScheme`) — and replaces it with argon2id the first time the
//! account signs in. An account whose v1 hash is not one (empty, or a scheme
//! neither system writes) arrives with no password and is reported.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use sc_viewpattern::{CFG_SITE_NAME, MENU_CONFIG_KEY, SALTCORN_UI_FRAMEWORK, saltcorn_ui_csp};

use super::archive::Entries;
use super::{Available, Item, MANIFEST_FILE};
use crate::handlers::csp_json;

mod agents;
mod formulas;
mod workflows;

use formulas::{V1Schema, calc_formula};

/// The entry every v1 backup has and no Feldspar backup has: the application's
/// metadata. What [`is_v1_backup`] recognises a file by.
const PACK: &str = "pack.json";
/// Where v1 records which version wrote the file — the counterpart of this
/// system's `feldspar_version`.
const BACKUP_INFO: &str = "backup-info.json";
/// The v1 config key holding the application's name, which the imported file
/// store is named after.
const SITE_NAME: &str = "site_name";
/// The column an imported user's v1 primary key is kept in.
const LEGACY_ID: &str = "legacy_id";
/// v1's users table, which is this system's users table and not an ordinary one.
const V1_USERS: &str = "users";

/// The columns of v1's users table that this system either owns itself or has no
/// use for. Anything else an admin added travels as an ordinary column.
const V1_USER_COLUMNS: [&str; 13] = [
    "id",
    "email",
    "password",
    "role_id",
    "disabled",
    "language",
    "api_token",
    "verification_token",
    "verified_on",
    "reset_password_token",
    "reset_password_expiry",
    "last_mobile_login",
    "_attributes",
];

/// Whether an archive with these entry names is a Saltcorn 1 backup rather than
/// one of ours.
///
/// By the presence of `pack.json`, which is v1's manifest in all but name — and
/// by the *names* alone, so a Feldspar backup is never read into memory to be
/// told apart. The caller has already established that there is no
/// `manifest.json`, so this does not have to rule ours out.
pub(super) fn is_v1_backup<'a>(mut names: impl Iterator<Item = &'a str>) -> bool {
    names.any(|name| name == PACK)
}

/// Translate a Saltcorn 1 archive into the entries a Feldspar backup would have
/// had, manifest included.
///
/// Fails only when the file is not a v1 backup at all — a `pack.json` that is not
/// JSON, or not an object. Everything narrower (a table whose rows are missing, a
/// field of a type with no counterpart, a trigger whose event this system does
/// not have) is a *note*: it is carried in the manifest and read back into the
/// restore report, so the admin is told what did not come across on the same
/// screen that tells them what did.
pub(super) fn convert(entries: &Entries) -> Result<Entries> {
    let pack = json_entry(entries, PACK)?;
    let pack = pack
        .as_object()
        .ok_or_else(|| Error::invalid("`pack.json` in this Saltcorn 1 backup is not an object"))?;

    let mut out = Entries::new();
    let mut contents = Available::default();
    let mut notes: Vec<String> = Vec::new();
    let site = site_name(entries);
    let schema = V1Schema::of(pack);

    // Users first, because every other table's references to them are rewritten
    // against the keys minted here.
    let users = convert_users(entries, pack, &site, &schema, &mut notes);
    contents.users = i64::try_from(users.rows.len()).unwrap_or(i64::MAX);
    out.insert(super::USERS_FILE.to_owned(), pretty(&users.document)?);
    out.insert(super::USER_ROWS_FILE.to_owned(), ndjson(&users.rows)?);
    let user_keys = users.keys;

    // The agents before the tables, because what they need of a table — a
    // full-text constraint, a vector field's embedding model — is written into
    // the table's document, and v1's memory table is theirs, not a table.
    let agents = agents::convert_agents(pack, &site, &mut notes);
    let mut wants_store = agents.needs_store;
    for value in array(pack, "tables") {
        let Some(table) = value.as_object() else {
            continue;
        };
        if name_of(table) == agents::V1_MEMORY_TABLE && agents.takes_memories() {
            let memories = match table_rows(entries, agents::V1_MEMORY_TABLE) {
                Ok(rows) => agents.convert_memories(&rows, |id| user_keys.rewrite(id), &mut notes),
                Err(e) => {
                    notes.push(format!(
                        "the agents' memories were not imported: {}",
                        e.causes()
                    ));
                    Vec::new()
                }
            };
            if !memories.is_empty() {
                out.insert(super::AGENT_MEMORY_FILE.to_owned(), ndjson(&memories)?);
            }
            continue;
        }
        match convert_table(
            entries, table, &site, &schema, &user_keys, &agents, &mut notes,
        ) {
            Ok(Some(converted)) => {
                wants_store |= converted.has_file_field;
                let rows = i64::try_from(converted.rows.len()).unwrap_or(i64::MAX);
                out.insert(
                    format!("tables/{}/table.json", converted.name),
                    pretty(&converted.document)?,
                );
                out.insert(
                    format!("tables/{}/rows.ndjson", converted.name),
                    ndjson(&converted.rows)?,
                );
                contents
                    .tables
                    .push(Item::new(converted.name).counting(rows));
            }
            Ok(None) => {}
            Err(e) => notes.push(format!(
                "table `{}` was not imported: {}",
                name_of(table),
                e.causes()
            )),
        }
    }

    let triggers = convert_triggers(pack, &agents, &mut notes);
    contents.agents = i64::try_from(agents.agents.len()).unwrap_or(i64::MAX);
    // The MCP servers travel under the LLM providers' choice, as they do in a
    // backup of this system's own.
    contents.llm_providers =
        i64::try_from(agents.providers.len() + agents.mcp_servers.len()).unwrap_or(i64::MAX);
    if !agents.agents.is_empty() {
        out.insert(
            "agents.json".to_owned(),
            pretty(&Json::Array(agents.agents))?,
        );
    }
    if !agents.providers.is_empty() {
        out.insert(
            "llm-providers.json".to_owned(),
            pretty(&Json::Array(agents.providers))?,
        );
    }
    if !agents.mcp_servers.is_empty() {
        out.insert(
            "mcp-servers.json".to_owned(),
            pretty(&Json::Array(agents.mcp_servers))?,
        );
    }
    contents.triggers = i64::try_from(triggers.len()).unwrap_or(i64::MAX);
    let trigger_names: Vec<String> = triggers
        .iter()
        .filter_map(|t| t.get("name").and_then(Json::as_str).map(str::to_owned))
        .collect();
    out.insert("triggers.json".to_owned(), pretty(&Json::Array(triggers))?);

    let mut store_name = None;
    if let Some(store) = convert_files(entries, &site, wants_store, &user_keys, &mut notes)? {
        store_name = Some(store.name.clone());
        for (path, bytes) in store.files {
            out.insert(format!("file-stores/{}/files/{path}", store.name), bytes);
        }
        contents
            .file_stores
            .push(Item::new(store.name.clone()).counting(store.count));
        out.insert(
            format!("file-stores/{}/store.json", store.name),
            pretty(&store.document)?,
        );
    }

    let tables: Vec<String> = contents.tables.iter().map(|t| t.name.clone()).collect();
    let application = convert_application(
        entries,
        pack,
        &site,
        &tables,
        store_name.as_deref(),
        &trigger_names,
        &mut notes,
    );
    let subdomain = &application.subdomain;
    out.insert(
        format!("applications/{subdomain}.json"),
        pretty(&application.document)?,
    );
    out.insert(
        format!("applications/{subdomain}/views.json"),
        pretty(&Json::Array(application.views.clone()))?,
    );
    out.insert(
        format!("applications/{subdomain}/pages.json"),
        pretty(&Json::Array(application.pages.clone()))?,
    );
    out.insert(
        format!("applications/{subdomain}/library.json"),
        pretty(&Json::Array(application.library.clone()))?,
    );
    contents
        .applications
        .push(Item::new(subdomain.clone()).labelled(site.clone()));
    contents.views = i64::try_from(application.views.len()).unwrap_or(i64::MAX);
    contents.pages = i64::try_from(application.pages.len()).unwrap_or(i64::MAX);

    note_what_was_left_out(pack, &mut notes);

    let version = v1_version(entries);
    out.insert(
        MANIFEST_FILE.to_owned(),
        pretty(&json!({
            "format": super::FORMAT,
            "version": super::FORMAT_VERSION,
            "feldspar_version": super::PRODUCT_VERSION,
            // Where it really came from. The archive being restored was written by
            // Saltcorn 1; saying only "Feldspar" here would make a translated file
            // indistinguishable from one this system wrote.
            "imported_from": { "product": "saltcorn", "version": version },
            // When *the v1 backup* was taken. The dialog shows it, and the answer
            // an admin wants is the age of the data, not the minute this server
            // read the file.
            "created_at": backup_date(entries),
            "source": format!("Saltcorn {version}, imported"),
            "notes": notes,
            "contents": contents.to_json(),
        }))?,
    );
    Ok(out)
}

/// The notes a converted manifest carries, for the restore report to repeat.
///
/// Empty for a Feldspar backup, which has nothing to explain.
pub(super) fn notes_of(manifest: &Map<String, Json>) -> Vec<String> {
    manifest
        .get("notes")
        .and_then(Json::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// --- tables ---------------------------------------------------------------------

/// One converted table: what goes in `table.json`, and its rows.
struct ConvertedTable {
    name: String,
    document: Json,
    rows: Vec<Json>,
    /// Whether one of its columns points into the imported file store, which is
    /// what makes that store worth defining even when the archive carries no
    /// bytes.
    has_file_field: bool,
}

/// A v1 table as `{ table, fields, constraints }`, with its rows translated.
///
/// `Ok(None)` for the users table, which is not an ordinary table here (§7.1) and
/// is converted by [`convert_users`] instead, and for a table this system cannot
/// hold at all.
fn convert_table(
    entries: &Entries,
    table: &Map<String, Json>,
    site: &str,
    schema: &V1Schema,
    user_keys: &UserKeys,
    agents: &agents::ConvertedAgents,
    notes: &mut Vec<String>,
) -> Result<Option<ConvertedTable>> {
    let name = name_of(table);
    if name.is_empty() {
        return Err(Error::invalid("it has no name"));
    }
    if name == V1_USERS {
        return Ok(None);
    }
    if table.get("provider_name").and_then(Json::as_str).is_some() {
        notes.push(format!(
            "table `{name}` was not imported: it is served by the v1 table provider \
             `{}`, which this system does not have",
            table
                .get("provider_name")
                .and_then(Json::as_str)
                .unwrap_or_default()
        ));
        return Ok(None);
    }

    let mut fields = Vec::new();
    let mut converted: BTreeMap<String, V1Field> = BTreeMap::new();
    for value in array(table, "fields") {
        let Some(field) = value.as_object() else {
            continue;
        };
        let field_name = name_of(field);
        match convert_field(field, site, schema, &name) {
            Ok(Some((json, kind))) => {
                fields.push(json);
                converted.insert(field_name, kind);
            }
            Ok(None) => {}
            Err(e) => notes.push(format!(
                "column `{name}.{field_name}` was not imported: {}",
                e.causes()
            )),
        }
    }

    let mut rows: Vec<Json> = match table_rows(entries, &name) {
        Ok(rows) => rows
            .iter()
            .map(|row| convert_row(row, &converted, user_keys))
            .collect(),
        Err(e) => {
            notes.push(format!(
                "the rows of `{name}` were not imported: {}",
                e.causes()
            ));
            Vec::new()
        }
    };
    // A v1 vector field whose dimensions its attributes do not say takes them
    // from its values; one with no values to say either is left out, and its
    // values with it.
    let mut dropped = Vec::new();
    fields.retain_mut(|field| {
        if field.get("type").and_then(Json::as_str) != Some(VECTOR_UNSIZED) {
            return true;
        }
        let field_name = field.get("name").and_then(Json::as_str).unwrap_or_default();
        let found = rows.iter().find_map(|row| {
            row.get(field_name)
                .and_then(Json::as_array)
                .map(|values| values.len())
                .filter(|n| *n > 0)
        });
        match found {
            Some(n) => {
                field["type"] = json!(format!("vector({n})"));
                true
            }
            None => {
                notes.push(format!(
                    "column `{name}.{field_name}` was not imported: it is a v1 `PGVector` \
                     field whose number of dimensions neither its attributes nor its values \
                     say; add it as a vector field of the embedding model's size"
                ));
                dropped.push(field_name.to_owned());
                false
            }
        }
    });
    for row in &mut rows {
        if let Some(obj) = row.as_object_mut() {
            for column in &dropped {
                obj.remove(column);
            }
        }
    }

    // v1 keeps its constraints in the pack as `constraints`, but they are
    // expressed against v1's field semantics; the restore reads what the
    // database reported, and an import has no database to report from. What
    // is added is what the agents need (a full-text constraint where one
    // searches the table).
    let mut constraints = Vec::new();
    agents.amend_table(&name, &mut fields, &mut constraints);

    let document = json!({
        "table": {
            // v1 has no separate label for a table; its name is what was shown.
            "label": name,
            "description": text(table, "description"),
            "min_role_read": role(table, "min_role_read"),
            "min_role_write": role(table, "min_role_write"),
            // Neither travels: a v1 ownership formula is a JavaScript expression
            // and this system's is `sc-expr` (§7.3), so importing one would be
            // importing an access rule that means something else. RLS is this
            // system's own idea.
            "ownership_formula": "",
            "rls_enabled": false,
            "sync_enabled": false,
        },
        "fields": fields,
        "constraints": constraints,
    });
    Ok(Some(ConvertedTable {
        has_file_field: converted.values().any(|k| *k == V1Field::File),
        name,
        document,
        rows,
    }))
}

/// What a converted field turned out to be, as far as its *values* are concerned.
///
/// Only what row translation needs to know: a date whose values must lose their
/// time, and a reference to the users table whose values must be rewritten from
/// numbers to UUIDs.
#[derive(Clone, Copy, PartialEq)]
enum V1Field {
    Plain,
    DayOnlyDate,
    UserKey,
    /// A file in the store this import creates — the one kind that makes the
    /// store necessary even when the archive carries no bytes for it.
    File,
    /// Calculated: computed on read here, so the value v1 stored (if it stored
    /// one) has no column to go in.
    Calc,
    /// A pgvector value (the `pgvector` plugin's `PGVector`), which v1's backup
    /// writes as the text pgvector prints, `[0.1,0.2]`.
    Vector,
}

/// The type a v1 `PGVector` field is given until its rows say its
/// dimensions — never one that reaches the restore.
const VECTOR_UNSIZED: &str = "vector";

/// One v1 field as this system's `createField` shape, plus how its values must be
/// read.
///
/// `Ok(None)` for a field that is deliberately not imported; `Err` for one whose
/// type has no counterpart, which the caller turns into a note.
fn convert_field(
    field: &Map<String, Json>,
    site: &str,
    schema: &V1Schema,
    table: &str,
) -> Result<Option<(Json, V1Field)>> {
    let name = name_of(field);
    if name.is_empty() {
        return Err(Error::invalid("it has no name"));
    }
    if flag(field, "calculated") {
        // Stored or not, it is a calculated field here, and those are never
        // stored (§3.4): the formula is translated ([`formulas`]) and the
        // schema editor checks it when the restore adds the field.
        let expression = calc_formula(schema, table, field)?;
        let mut out = Map::new();
        out.insert("name".to_owned(), json!(name));
        out.insert("label".to_owned(), json!(text(field, "label")));
        out.insert("description".to_owned(), json!(text(field, "description")));
        if let Some(type_name) = calc_type(field) {
            out.insert("type".to_owned(), json!(type_name));
        }
        out.insert(
            "kind".to_owned(),
            json!({ "type": "calc", "expression": expression }),
        );
        return Ok(Some((Json::Object(out), V1Field::Calc)));
    }

    let type_name = field.get("type").and_then(Json::as_str).unwrap_or_default();
    let attrs = field
        .get("attributes")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(name));
    out.insert("label".to_owned(), json!(text(field, "label")));
    out.insert("description".to_owned(), json!(text(field, "description")));
    out.insert("required".to_owned(), json!(flag(field, "required")));
    out.insert("unique".to_owned(), json!(flag(field, "is_unique")));
    out.insert("primary_key".to_owned(), json!(flag(field, "primary_key")));

    let mut kind = V1Field::Plain;
    let mut attributes = Map::new();
    match type_name {
        "String" => {
            out.insert("type".to_owned(), json!("string"));
            if let Some(max) = attrs.get("max_length").and_then(Json::as_i64) {
                attributes.insert("max_length".to_owned(), json!(max));
            }
            if let Some(regexp) = attrs.get("regexp").and_then(Json::as_str)
                && !regexp.is_empty()
            {
                attributes.insert("regex".to_owned(), json!(regexp));
            }
            // v1 writes the option set as one comma-separated string; this
            // system's `string` type takes a JSON array.
            if let Some(options) = attrs.get("options").and_then(Json::as_str)
                && !options.trim().is_empty()
            {
                let values: Vec<Json> = options
                    .split(',')
                    .map(|o| json!(o.trim()))
                    .filter(|o| o.as_str() != Some(""))
                    .collect();
                attributes.insert("options".to_owned(), Json::Array(values));
            }
        }
        "Integer" => {
            out.insert("type".to_owned(), json!("integer"));
            for bound in ["min", "max"] {
                if let Some(value) = attrs.get(bound).and_then(Json::as_i64) {
                    attributes.insert(bound.to_owned(), json!(value));
                }
            }
        }
        "Float" => {
            out.insert("type".to_owned(), json!("float"));
        }
        "Bool" => {
            out.insert("type".to_owned(), json!("bool"));
        }
        "Date" => {
            // v1 stores every date as a timestamp and renders the day alone when
            // the field says so; this system has a `date` type, which is what a
            // day-only field means.
            if flag_value(attrs.get("day_only")) {
                out.insert("type".to_owned(), json!("date"));
                kind = V1Field::DayOnlyDate;
            } else {
                out.insert("type".to_owned(), json!("timestamp"));
            }
        }
        "JSON" => {
            out.insert("type".to_owned(), json!("json"));
        }
        // The `pgvector` plugin's vector (TODO-agent-skills AS2.6). Its size is
        // its `dimensions` attribute where it has one, else its values'.
        "PGVector" => {
            let dimensions = ["dimensions", "dims", "size"]
                .iter()
                .find_map(|key| match attrs.get(*key) {
                    Some(Json::Number(n)) => n.as_u64(),
                    Some(Json::String(s)) => s.trim().parse::<u64>().ok(),
                    _ => None,
                })
                .filter(|n| *n > 0);
            let type_name = match dimensions {
                Some(n) => format!("vector({n})"),
                None => VECTOR_UNSIZED.to_owned(),
            };
            out.insert("type".to_owned(), json!(type_name));
            kind = V1Field::Vector;
        }
        // A v1 colour is a `#rrggbb` string and there is no colour type here yet.
        "Color" => {
            out.insert("type".to_owned(), json!("text"));
        }
        "Key" => {
            let target = attrs
                .get("reftable_name")
                .and_then(Json::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    field
                        .get("reftable_name")
                        .and_then(Json::as_str)
                        .map(str::to_owned)
                })
                .filter(|t| !t.is_empty())
                .ok_or_else(|| Error::invalid("it is a key that names no table"))?;
            let target_field = field
                .get("refname")
                .and_then(Json::as_str)
                .filter(|f| !f.is_empty())
                .unwrap_or("id");
            if target == V1_USERS {
                kind = V1Field::UserKey;
            }
            let mut key = Map::new();
            key.insert("type".to_owned(), json!("key"));
            key.insert("target_table".to_owned(), json!(target));
            key.insert("target_field".to_owned(), json!(target_field));
            if let Some(summary) = attrs.get("summary_field").and_then(Json::as_str)
                && !summary.is_empty()
            {
                key.insert("summary_field".to_owned(), json!(summary));
            }
            out.insert("kind".to_owned(), Json::Object(key));
            // No `type`: a reference's storage type is its target's, and the
            // schema editor is what knows that.
        }
        "File" => {
            // A v1 file field holds the name of a file in the application's one
            // file area; here it is a `File` field pointing into the store that
            // area became, which is why the store's name is threaded this far in.
            out.insert(
                "kind".to_owned(),
                json!({ "type": "file", "store": site, "folder": Json::Null, "mime_allow": [] }),
            );
            kind = V1Field::File;
        }
        other => {
            return Err(Error::invalid(format!(
                "`{other}` is not a type this system has"
            )));
        }
    }
    if !attributes.is_empty() {
        out.insert("attributes".to_owned(), Json::Object(attributes));
    }
    Ok(Some((Json::Object(out), kind)))
}

/// One v1 row as this system reads rows: the columns that were imported, with the
/// two kinds of value that need rewriting rewritten.
fn convert_row(row: &Json, fields: &BTreeMap<String, V1Field>, user_keys: &UserKeys) -> Json {
    let mut out = Map::new();
    let Some(obj) = row.as_object() else {
        return Json::Object(out);
    };
    for (column, value) in obj {
        let Some(kind) = fields.get(column) else {
            // A column that was not imported: its values have nowhere to go.
            continue;
        };
        let value = match kind {
            // Computed on read here; v1's stored value has nowhere to go.
            V1Field::Calc => continue,
            V1Field::Plain | V1Field::File => value.clone(),
            V1Field::Vector => vector_value(value),
            V1Field::DayOnlyDate => day_only(value),
            V1Field::UserKey => user_keys.rewrite(value),
        };
        out.insert(column.clone(), value);
    }
    Json::Object(out)
}

/// A v1 vector as the array of numbers it is here: pgvector's text, `[0.1,0.2]`,
/// parsed; anything else as it was, for the restore to refuse by name.
fn vector_value(value: &Json) -> Json {
    match value
        .as_str()
        .map(|s| serde_json::from_str::<Json>(s.trim()))
    {
        Some(Ok(parsed @ Json::Array(_))) => parsed,
        _ => value.clone(),
    }
}

/// A v1 timestamp reduced to the day it names — `2026-09-02T00:00:00.000Z` to
/// `2026-09-02` — since a day-only v1 field becomes a `date` column here.
fn day_only(value: &Json) -> Json {
    match value.as_str() {
        Some(text) if text.len() > 10 => json!(&text[..10]),
        _ => value.clone(),
    }
}

/// The type a v1 calculated field's values have, as this system names it — the
/// scalar types alone, since a calculated `Key` or `File` is not one here.
fn calc_type(field: &Map<String, Json>) -> Option<&'static str> {
    Some(match field.get("type").and_then(Json::as_str)? {
        "String" | "Color" => "string",
        "Integer" => "integer",
        "Float" => "float",
        "Bool" => "bool",
        "Date" => "timestamp",
        "JSON" => "json",
        _ => return None,
    })
}

// --- users ----------------------------------------------------------------------

/// The UUID each imported v1 user number was given.
///
/// Minted before any table is converted, because every `Key` onto the users table
/// in every other table is rewritten through this.
#[derive(Default)]
struct UserKeys {
    by_v1_id: BTreeMap<i64, Uuid>,
}

impl UserKeys {
    /// A v1 user number as the UUID it became — or null, when the number names a
    /// user this backup does not carry, since a foreign key onto a row that is not
    /// there cannot be restored at all.
    fn rewrite(&self, value: &Json) -> Json {
        match value.as_i64().and_then(|id| self.by_v1_id.get(&id)) {
            Some(uuid) => json!(uuid.to_string()),
            None => Json::Null,
        }
    }

    /// The same, for a v1 id that may be a string (as `files.csv` spells it).
    fn owner(&self, raw: &str) -> Option<String> {
        raw.parse::<i64>()
            .ok()
            .and_then(|id| self.by_v1_id.get(&id))
            .map(std::string::ToString::to_string)
    }
}

/// The converted `users/users.json` and the accounts for `users/rows.ndjson`,
/// plus the key map the rest of the conversion needs.
struct ConvertedUsers {
    document: Json,
    rows: Vec<Json>,
    keys: UserKeys,
}

/// v1's roles and accounts: `{ roles, fields }`, and the rows.
///
/// `fields` carries `legacy_id` and any column an admin added to v1's users table:
/// the restore adds the ones the users table here has not got before it inserts
/// the rows, so an imported account keeps what was on it.
fn convert_users(
    entries: &Entries,
    pack: &Map<String, Json>,
    site: &str,
    schema: &V1Schema,
    notes: &mut Vec<String>,
) -> ConvertedUsers {
    let roles: Vec<Json> = array(pack, "roles")
        .iter()
        .filter_map(|value| {
            let obj = value.as_object()?;
            let id = obj.get("id").and_then(Json::as_i64)?;
            let name = obj.get("role").and_then(Json::as_str)?;
            Some(json!({ "role": id, "name": name }))
        })
        .collect();

    let v1_users = array(pack, "tables")
        .into_iter()
        .find(|t| t.as_object().map(name_of).as_deref() == Some(V1_USERS));
    let custom: Vec<(Json, V1Field)> = v1_users
        .as_ref()
        .and_then(Json::as_object)
        .map(|table| array(table, "fields"))
        .unwrap_or_default()
        .iter()
        .filter_map(|value| {
            let field = value.as_object()?;
            let name = name_of(field);
            if V1_USER_COLUMNS.contains(&name.as_str()) {
                return None;
            }
            match convert_field(field, site, schema, V1_USERS) {
                Ok(Some(converted)) => Some(converted),
                Ok(None) => None,
                Err(e) => {
                    notes.push(format!(
                        "column `users.{name}` was not imported: {}",
                        e.causes()
                    ));
                    None
                }
            }
        })
        .collect();

    let mut fields = vec![json!({
        "name": LEGACY_ID,
        "label": "Legacy id",
        "description": "The primary key this user had in Saltcorn 1.",
        "type": "int",
        "required": false,
        // Not unique, though it was in the database it came from: two v1
        // applications imported onto one server would each bring a user 1.
        "unique": false,
        "primary_key": false,
    })];
    fields.extend(custom.iter().map(|(json, _)| json.clone()));

    let source = match table_rows(entries, V1_USERS) {
        Ok(rows) => rows,
        Err(e) => {
            notes.push(format!(
                "the user accounts were not imported: {}",
                e.causes()
            ));
            Vec::new()
        }
    };
    let custom_kinds: BTreeMap<String, V1Field> = custom
        .iter()
        .filter_map(|(f, kind)| {
            f.get("name")
                .and_then(Json::as_str)
                .map(|name| (name.to_owned(), *kind))
        })
        .collect();

    let mut keys = UserKeys::default();
    let mut rows = Vec::with_capacity(source.len());
    let mut without_password = 0;
    for value in &source {
        let Some(user) = value.as_object() else {
            continue;
        };
        let Some(email) = user.get("email").and_then(Json::as_str) else {
            notes.push("a user without an email address was not imported".to_owned());
            continue;
        };
        let id = Uuid::new_v4();
        if let Some(v1_id) = user.get("id").and_then(Json::as_i64) {
            keys.by_v1_id.insert(v1_id, id);
        }
        let mut row = Map::new();
        row.insert(sc_auth::COL_ID.to_owned(), json!(id.to_string()));
        row.insert(sc_auth::COL_EMAIL.to_owned(), json!(email));
        row.insert(
            sc_auth::COL_ROLE.to_owned(),
            json!(user.get("role_id").and_then(Json::as_i64).unwrap_or(100)),
        );
        if let Some(disabled) = user.get("disabled").and_then(Json::as_bool) {
            row.insert(sc_auth::COL_DISABLED.to_owned(), json!(disabled));
        }
        row.insert(
            LEGACY_ID.to_owned(),
            user.get("id").cloned().unwrap_or(Json::Null),
        );
        for (name, kind) in &custom_kinds {
            if *kind != V1Field::Calc
                && let Some(value) = user.get(name)
            {
                row.insert(name.clone(), value.clone());
            }
        }
        // v1's bcrypt hash, verified as it is and rehashed at the first login.
        match user.get("password").and_then(Json::as_str) {
            Some(hash) if sc_auth::is_valid_hash(hash) => {
                row.insert(sc_auth::COL_PASSWORD_HASH.to_owned(), json!(hash));
            }
            _ => without_password += 1,
        }
        rows.push(Json::Object(row));
    }
    // A user's own reference to a user (`manager`) is rewritten once every
    // account has its UUID, since it may name one further down the file.
    let rewritten = custom_kinds
        .iter()
        .filter(|(_, kind)| matches!(kind, V1Field::UserKey | V1Field::DayOnlyDate));
    for (name, kind) in rewritten {
        for row in &mut rows {
            if let Some(slot) = row.as_object_mut().and_then(|r| r.get_mut(name)) {
                *slot = match kind {
                    V1Field::UserKey => keys.rewrite(slot),
                    _ => day_only(slot),
                };
            }
        }
    }
    if without_password > 0 {
        notes.push(format!(
            "{} imported with no password: the Saltcorn 1 backup holds no password \
             hash this system can verify for {}. Set a password on each, or have each \
             user reset theirs.",
            plural(without_password, "account was", "accounts were"),
            if without_password == 1 { "it" } else { "them" }
        ));
    }

    ConvertedUsers {
        document: json!({ "roles": roles, "fields": fields }),
        rows,
        keys,
    }
}

// --- files ----------------------------------------------------------------------

/// The converted file store: its definition, its files' metadata, and the bytes.
struct ConvertedStore {
    name: String,
    document: Json,
    files: Vec<(String, Vec<u8>)>,
    count: i64,
}

/// v1's uploads as a **local file store named after the application**.
///
/// v1 has one file area per application and this system has named stores (§6), so
/// the import has to choose a name; the application's own (`site_name`) is the
/// one an admin will recognise, and it is what the restore creates the store under
/// when there is not one already. Its directory is the one this system suggests
/// for a local store of that name, created on connect — an imported store has no
/// directory on this machine to point at.
fn convert_files(
    entries: &Entries,
    site: &str,
    needed: bool,
    user_keys: &UserKeys,
    notes: &mut Vec<String>,
) -> Result<Option<ConvertedStore>> {
    let mut files = Vec::new();
    let mut metadata = Vec::new();
    if let Some(csv) = entries.get("files.csv") {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(csv.as_slice());
        for record in reader.deserialize::<BTreeMap<String, String>>() {
            let record = match record {
                Ok(record) => record,
                Err(e) => {
                    notes.push(format!("a line of `files.csv` could not be read: {e}"));
                    continue;
                }
            };
            let get = |key: &str| record.get(key).map(String::as_str).unwrap_or("").trim();
            // A v1 directory is a row with no bytes; the paths of the files in it
            // recreate it.
            if matches!(get("isDirectory"), "true" | "1") {
                continue;
            }
            let location = get("location");
            let path = if location.is_empty() {
                get("filename")
            } else {
                location
            };
            if path.is_empty() {
                continue;
            }
            let Some(bytes) = entries.get(&format!("files/{path}")) else {
                notes.push(format!(
                    "file `{path}` is listed in `files.csv` but its contents are not in \
                     the archive"
                ));
                continue;
            };
            let mut meta = Map::new();
            meta.insert("path".to_owned(), json!(path));
            meta.insert(
                "min_role".to_owned(),
                match get("min_role_read").parse::<i64>() {
                    Ok(role) if (1..=100).contains(&role) => json!(role),
                    _ => Json::Null,
                },
            );
            meta.insert(
                "owner".to_owned(),
                match user_keys.owner(get("user_id")) {
                    Some(owner) => json!(owner),
                    None => Json::Null,
                },
            );
            metadata.push(Json::Object(meta));
            files.push((path.to_owned(), bytes.clone()));
        }
    }
    // A store with no files is still worth defining when a column points into it:
    // a `File` field whose store is not there is a column that refuses every
    // value written to it.
    if files.is_empty() && !needed {
        return Ok(None);
    }

    let directory = sc_files::suggest_local_dir(site)?;
    let mut config = Map::new();
    config.insert(
        sc_files::CFG_PATH.to_owned(),
        json!(directory.to_string_lossy()),
    );
    // The directory does not exist on this machine — nothing has put files there
    // yet — so the store makes it rather than refusing to connect and leaving the
    // bytes nowhere to go.
    config.insert(sc_files::CFG_CREATE.to_owned(), json!(true));
    let count = i64::try_from(files.len()).unwrap_or(i64::MAX);
    Ok(Some(ConvertedStore {
        name: site.to_owned(),
        document: json!({
            "definition": {
                "name": site,
                "description": format!("The files of the imported Saltcorn 1 application {site}"),
                "backend": sc_files::LOCAL_BACKEND,
                "config": Json::Object(config),
                "min_role": Json::Null,
            },
            "files": metadata,
        }),
        files,
        count,
    }))
}

/// The application's name, from v1's `config/site_name`, as the imported file
/// store is named.
///
/// `saltcorn-import` when there is none: a store still needs a name, and one that
/// says where it came from beats one that says nothing.
fn site_name(entries: &Entries) -> String {
    entries
        .get(&format!("config/{SITE_NAME}"))
        .and_then(|bytes| serde_json::from_slice::<Json>(bytes).ok())
        // v1 wraps every stored config value as `{ "v": … }`.
        .and_then(|value| value.get("v").and_then(Json::as_str).map(str::to_owned))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "saltcorn-import".to_owned())
}

// --- triggers -------------------------------------------------------------------

/// v1's actions as this system's triggers.
///
/// The *name* of the action is carried through unchanged rather than mapped:
/// where the two systems have an action in common the settings mean the same
/// thing, and where they do not, `save_trigger` refuses the trigger by name —
/// "unknown action `modify_row`" — which is a better answer than an action
/// silently turned into the nearest one that exists.
fn convert_triggers(
    pack: &Map<String, Json>,
    agents: &agents::ConvertedAgents,
    notes: &mut Vec<String>,
) -> Vec<Json> {
    let mut out = Vec::new();
    for value in array(pack, "triggers") {
        let Some(trigger) = value.as_object() else {
            continue;
        };
        let name = name_of(trigger);
        // An agent here is not a trigger but an agent ([`agents`]).
        if text(trigger, "action") == agents::AGENT_ACTION {
            continue;
        }
        // A workflow crosses step for step ([`workflows`]), or not at all.
        let workflow = if trigger.contains_key("steps") {
            match workflows::convert_workflow(trigger) {
                Ok(workflow) => Some(workflow),
                Err(e) => {
                    notes.push(format!(
                        "workflow `{name}` was not imported: {}",
                        e.causes()
                    ));
                    continue;
                }
            }
        } else {
            None
        };
        let raw = trigger
            .get("when_trigger")
            .and_then(Json::as_str)
            .unwrap_or_default();
        let Some(when) = convert_event(raw) else {
            notes.push(format!(
                "trigger `{name}` was not imported: this system has no `{raw}` event"
            ));
            continue;
        };
        // v1 consolidates every agent's memories at once; here the action
        // names its agent, so there is one trigger per agent with a memory.
        if text(trigger, "action") == agents::CONSOLIDATE_ACTION {
            let consolidations = agents.consolidations(&name);
            if consolidations.is_empty() {
                notes.push(format!(
                    "trigger `{name}` was not imported: it consolidates agents' memories,                      and no agent here has a memory"
                ));
            }
            for (each, configuration) in consolidations {
                let mut converted = json!({
                    "name": each,
                    "description": text(trigger, "description"),
                    "when": when,
                    "action": "consolidate_memory",
                    "configuration": configuration,
                });
                if let Some(min_role) = trigger.get("min_role").and_then(Json::as_i64) {
                    converted["min_role"] = json!(min_role);
                }
                out.push(converted);
            }
            continue;
        }
        let mut out_trigger = Map::new();
        out_trigger.insert("name".to_owned(), json!(name));
        out_trigger.insert(
            "description".to_owned(),
            json!(text(trigger, "description")),
        );
        out_trigger.insert("when".to_owned(), json!(when));
        // Only a table event has a table. v1 keeps a table on a `Never` trigger
        // as the rows it can be run against, which this system expresses on the
        // action itself; a channel there would be refused.
        if matches!(when, "insert" | "update" | "delete") {
            out_trigger.insert("channel".to_owned(), json!(text(trigger, "table_name")));
        }
        if let Some(workflow) = workflow {
            out_trigger.insert("body".to_owned(), json!(sc_action::TriggerBody::WORKFLOW));
            out_trigger.insert("workflow".to_owned(), workflow);
        } else {
            out_trigger.insert(
                "action".to_owned(),
                trigger.get("action").cloned().unwrap_or(Json::Null),
            );
            out_trigger.insert(
                "configuration".to_owned(),
                trigger
                    .get("configuration")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
        }
        if let Some(min_role) = trigger.get("min_role").and_then(Json::as_i64) {
            out_trigger.insert("min_role".to_owned(), json!(min_role));
        }
        out.push(Json::Object(out_trigger));
    }
    out
}

/// A v1 `when_trigger` as this system's event name, or `None` for an event it
/// does not have.
fn convert_event(when: &str) -> Option<&'static str> {
    Some(match when {
        "Insert" => "insert",
        "Update" => "update",
        "Delete" => "delete",
        // v1's "Never" is "only when something asks", which is this system's
        // `none`; so is an API call, which is one of the things that asks.
        "Never" | "API call" => "none",
        "Login" => "login",
        "Error" => "error",
        "Often" => "often",
        "Hourly" => "hourly",
        "Daily" => "daily",
        "Weekly" => "weekly",
        _ => return None,
    })
}

// --- the application: views, pages and the menu ---------------------------------

/// The Saltcorn UI application a v1 backup becomes, as the entries a Feldspar
/// backup of one would hold.
struct ConvertedApplication {
    subdomain: String,
    document: Json,
    views: Vec<Json>,
    pages: Vec<Json>,
    /// Library items, each with the UUID minted for it.
    library: Vec<Json>,
}

/// **One application per backup** (§13): named after the site, framework
/// `saltcorn-ui`, every imported table, store and trigger in its subsets, the
/// Saltcorn UI CSP, and v1's menu less what points at v1's own screens.
///
/// The triggers are in the subset because v1's views name them: an action column
/// running `TrimPages` is refused on save unless the application declares a
/// trigger of that name (§12.3), and a v1 trigger is global, so every one the
/// backup carries is one its views may run.
///
/// The document has **no `id`**. The restore matches an imported application by
/// its name instead, which is what a second import of the same backup has in
/// common with the first (8.6) — and it is also where the subdomain derived here
/// is de-duplicated, since only the restore can see which ones are taken.
fn convert_application(
    entries: &Entries,
    pack: &Map<String, Json>,
    site: &str,
    tables: &[String],
    store: Option<&str>,
    triggers: &[String],
    notes: &mut Vec<String>,
) -> ConvertedApplication {
    let subdomain = subdomain_for(site);
    let mut config = Map::new();
    config.insert(CFG_SITE_NAME.to_owned(), json!(site));
    config.insert(
        MENU_CONFIG_KEY.to_owned(),
        Json::Array(convert_menu(entries, notes)),
    );
    let document = json!({
        "name": site,
        "description": "Imported from a Saltcorn 1 backup.",
        "subdomain": subdomain,
        "framework": { "name": SALTCORN_UI_FRAMEWORK, "config": config },
        "extra_frameworks": [],
        "tables": tables,
        "file_stores": store.into_iter().collect::<Vec<_>>(),
        "triggers": triggers,
        "apis": [],
        "static_dirs": [],
        "csp": csp_json(&saltcorn_ui_csp()),
        "attributes": {},
    });
    let objects = |key: &str| -> Vec<Map<String, Json>> {
        array(pack, key)
            .into_iter()
            .filter_map(|v| v.as_object().cloned())
            .collect()
    };
    // The library first: its ids are what every layout's `library_id` is
    // rewritten to, its own items' layouts included.
    let (library, keys) = convert_library(pack, notes);
    ConvertedApplication {
        views: objects("views")
            .iter()
            .map(|view| convert_view(view, &keys))
            .collect(),
        pages: objects("pages")
            .iter()
            .map(|page| convert_page(page, &keys))
            .collect(),
        library,
        subdomain,
        document,
    }
}

/// The UUID each v1 library item was given, keyed by the serial v1 knows it by.
///
/// **By position, because the pack has no ids.** v1 writes a library entry as
/// `Library.toJson`, which drops `id`, and its `install_pack` restores the
/// entries in pack order with `Library.create` onto v1's empty `_sc_library` — so
/// the first entry becomes serial 1, the second serial 2. That is the only
/// reading under which a v1 backup's own layouts resolve in v1 after a restore,
/// and so the one taken here. A `library_id` that names no position is left as
/// written: it renders blank, as it would in v1, and the restore reports it.
#[derive(Default)]
struct LibraryKeys {
    by_v1_id: BTreeMap<i64, Uuid>,
}

impl LibraryKeys {
    /// Rewrite every `library` segment's `library_id` in `value` that names an
    /// imported item, slots and nested containers included. v1 writes a number;
    /// a string of digits is read as the same number.
    fn rewrite(&self, value: &mut Json) {
        match value {
            Json::Object(segment) => {
                if segment.get("type").and_then(Json::as_str) == Some("library") {
                    let serial = match segment.get("library_id") {
                        Some(Json::Number(n)) => n.as_i64(),
                        Some(Json::String(text)) => text.trim().parse::<i64>().ok(),
                        _ => None,
                    };
                    if let Some(id) = serial.and_then(|serial| self.by_v1_id.get(&serial)) {
                        segment.insert("library_id".to_owned(), json!(id.to_string()));
                    }
                }
                segment.values_mut().for_each(|child| self.rewrite(child));
            }
            Json::Array(items) => items.iter_mut().for_each(|item| self.rewrite(item)),
            _ => {}
        }
    }
}

/// v1's `library` entries as this system's library items, in the shape a
/// Feldspar backup carries them in, each with a fresh UUID, and the key map
/// the views' and pages' layouts are rewritten through.
fn convert_library(pack: &Map<String, Json>, notes: &mut Vec<String>) -> (Vec<Json>, LibraryKeys) {
    let entries = array(pack, "library");
    let mut keys = LibraryKeys::default();
    let mut kept = Vec::new();
    for (position, value) in (1_i64..).zip(&entries) {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let name = name_of(entry);
        if name.trim().is_empty() {
            notes.push("a library entry without a name was not imported".to_owned());
            continue;
        }
        let id = Uuid::new_v4();
        keys.by_v1_id.insert(position, id);
        // v1's `Library` constructor reads a layout stored as JSON text too.
        let layout = match entry.get("layout") {
            Some(Json::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
            Some(layout) if !layout.is_null() => layout.clone(),
            _ => json!({}),
        };
        kept.push((id, name, text(entry, "icon"), layout));
    }
    let library = kept
        .into_iter()
        .map(|(id, name, icon, mut layout)| {
            keys.rewrite(&mut layout);
            json!({
                "id": id.to_string(),
                "name": name,
                "description": "",
                "icon": icon,
                "layout": layout,
                "attributes": {},
            })
        })
        .collect();
    (library, keys)
}

/// A subdomain from an application's name: lower case, each run of anything but
/// an ASCII letter or digit one hyphen, none at either end, and no longer than the
/// 63 characters a DNS label may be. `saltcorn-import` for a name with nothing in
/// it to keep.
pub(super) fn subdomain_for(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(63);
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "saltcorn-import".to_owned()
    } else {
        out.to_owned()
    }
}

/// One v1 view, in the shape a Feldspar backup carries a view in. The
/// configuration crosses **unchanged** (§1) but for its `library_id`s: it is
/// v1-shaped on purpose, and translating it would be inventing a second format to
/// keep in step with a file this system does not own.
///
/// Nothing is checked here. Whether the pattern is registered and the table came
/// are questions about the server the backup is restored onto and what was
/// chosen, so the restore asks them and reports each refusal by the view's name.
fn convert_view(view: &Map<String, Json>, library: &LibraryKeys) -> Json {
    let mut attributes = view
        .get("attributes")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    // v1 keeps this beside the attributes; here a view has one place for its
    // sparse settings.
    if let Some(page) = view
        .get("default_render_page")
        .and_then(Json::as_str)
        .filter(|p| !p.is_empty())
    {
        attributes.insert("default_render_page".to_owned(), json!(page));
    }
    // A view over a v1 table provider names it in `exttable_name`. Such a table
    // is never imported, and naming it lets the restore say so by name.
    let table = view
        .get("table")
        .and_then(Json::as_str)
        .or_else(|| view.get("exttable_name").and_then(Json::as_str))
        .filter(|t| !t.is_empty());
    let mut configuration = view
        .get("configuration")
        .filter(|c| c.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    library.rewrite(&mut configuration);
    json!({
        "name": name_of(view),
        "description": text(view, "description"),
        "viewpattern": text(view, "viewtemplate"),
        "table_name": table,
        "configuration": configuration,
        "min_role": role(view, "min_role"),
        "slug": view.get("slug").cloned().unwrap_or(Json::Null),
        "attributes": attributes,
    })
}

/// One v1 page. The layout crosses unchanged but for its `library_id`s and v1's
/// fixed-state fold ([`fold_fixed_states`]); `root_page_for_roles` — what makes
/// `/` resolve — moves into the attributes, which is where a page's sparse
/// settings live here.
fn convert_page(page: &Map<String, Json>, library: &LibraryKeys) -> Json {
    let mut attributes = page
        .get("attributes")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    attributes.insert(
        "root_page_for_roles".to_owned(),
        page.get("root_page_for_roles")
            .filter(|r| r.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    let mut layout = page.get("layout").cloned().unwrap_or_else(|| json!({}));
    // v1's `Page` constructor reads `fixed_states` stored as JSON text too.
    let fixed_states = match page.get("fixed_states") {
        Some(Json::String(text)) => serde_json::from_str(text).unwrap_or(Json::Null),
        other => other.cloned().unwrap_or(Json::Null),
    };
    if let Some(fixed_states) = fixed_states.as_object() {
        fold_fixed_states(&mut layout, fixed_states);
    }
    library.rewrite(&mut layout);
    json!({
        "name": name_of(page),
        "title": text(page, "title"),
        "description": text(page, "description"),
        "layout": layout,
        "min_role": role(page, "min_role"),
        "attributes": attributes,
    })
}

/// v1's fold of a page's legacy `fixed_states` into its `view` segments, from
/// `getEditNormalPage` in v1's `server/routes/pageedit.ts`:
///
/// ```js
/// traverseSync(page.layout, { view(s) {
///   if (s.state === "fixed" && !s.configuration) {
///     const fs = page.fixed_states[s.name];
///     if (fs) s.configuration = fs;
///   } } });
/// ```
///
/// JavaScript's truthiness is kept: a segment whose `configuration` is `{}`
/// already has one and is left alone, and an entry that is `{}` is still
/// folded in. The walk visits every object, which reaches every segment
/// `traverseSync` does.
fn fold_fixed_states(layout: &mut Json, fixed_states: &Map<String, Json>) {
    match layout {
        Json::Object(segment) => {
            if segment.get("type").and_then(Json::as_str) == Some("view")
                && segment.get("state").and_then(Json::as_str) == Some("fixed")
                && segment.get("configuration").is_none_or(js_falsy)
                && let Some(fixed) = segment
                    .get("name")
                    .and_then(Json::as_str)
                    .and_then(|name| fixed_states.get(name))
                    .filter(|fixed| !js_falsy(fixed))
            {
                segment.insert("configuration".to_owned(), fixed.clone());
            }
            segment
                .values_mut()
                .for_each(|child| fold_fixed_states(child, fixed_states));
        }
        Json::Array(items) => items
            .iter_mut()
            .for_each(|item| fold_fixed_states(item, fixed_states)),
        _ => {}
    }
}

/// Whether JavaScript reads `value` as false: `null`, `false`, `0` and `""`.
/// Every object and array is true.
fn js_falsy(value: &Json) -> bool {
    match value {
        Json::Null => true,
        Json::Bool(b) => !b,
        Json::Number(n) => n.as_f64() == Some(0.0),
        Json::String(s) => s.is_empty(),
        Json::Array(_) | Json::Object(_) => false,
    }
}

/// What was taken out of v1's menu, for the notes.
#[derive(Default)]
struct MenuDropped {
    admin_pages: usize,
    user_pages: usize,
    untyped: usize,
    /// Headers left with nothing under them, by label.
    emptied: Vec<String>,
}

/// v1's `menu_items` config, less every `Admin Page` and `User Page` entry
/// (§13) — the one opens v1's admin UI, which is not here, and the other v1's
/// user screens, of which Saltcorn UI has its own Login, Sign up and Logout in
/// the menu already. What remains keeps its `Header`/`subitems` nesting; a header
/// left with nothing under it goes too, since a heading over nothing is not a
/// menu entry. Each kind taken out is a note.
fn convert_menu(entries: &Entries, notes: &mut Vec<String>) -> Vec<Json> {
    let items = entries
        .get("config/menu_items")
        .and_then(|bytes| serde_json::from_slice::<Json>(bytes).ok())
        // v1 wraps every stored config value as `{ "v": … }`.
        .and_then(|value| value.get("v").and_then(Json::as_array).cloned())
        .unwrap_or_default();
    let mut dropped = MenuDropped::default();
    let menu = strip_menu(&items, &mut dropped);
    if dropped.admin_pages > 0 {
        notes.push(format!(
            "{} not imported: they open Saltcorn 1's admin screens, which this system \
             does not have",
            plural(
                dropped.admin_pages,
                "Admin Page menu entry was",
                "Admin Page menu entries were"
            )
        ));
    }
    if dropped.user_pages > 0 {
        notes.push(format!(
            "{} not imported: they open Saltcorn 1's user screens, and Saltcorn UI's menu \
             has its own Login, Sign up and Logout entries",
            plural(
                dropped.user_pages,
                "User Page menu entry was",
                "User Page menu entries were"
            )
        ));
    }
    if dropped.untyped > 0 {
        notes.push(format!(
            "{} not imported: it says neither what it links to nor what kind of entry it is",
            plural(dropped.untyped, "menu entry was", "menu entries were")
        ));
    }
    for header in dropped.emptied {
        notes.push(format!(
            "the menu header `{header}` was not imported: everything under it was"
        ));
    }
    menu
}

fn strip_menu(items: &[Json], dropped: &mut MenuDropped) -> Vec<Json> {
    let mut out = Vec::new();
    for item in items {
        let Some(entry) = item.as_object() else {
            continue;
        };
        match entry.get("type").and_then(Json::as_str) {
            Some("Admin Page") => {
                dropped.admin_pages += 1;
                continue;
            }
            Some("User Page") => {
                dropped.user_pages += 1;
                continue;
            }
            Some(_) => {}
            None => {
                dropped.untyped += 1;
                continue;
            }
        }
        let mut entry = entry.clone();
        if let Some(Json::Array(subitems)) = entry.get("subitems").cloned()
            && !subitems.is_empty()
        {
            let kept = strip_menu(&subitems, dropped);
            if kept.is_empty() {
                dropped.emptied.push(
                    entry
                        .get("label")
                        .or_else(|| entry.get("text"))
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_owned(),
                );
                continue;
            }
            entry.insert("subitems".to_owned(), Json::Array(kept));
        }
        out.push(Json::Object(entry));
    }
    out
}

// --- what is not imported --------------------------------------------------------

/// One note per kind of thing the pack carries that this system has no counterpart
/// for, so the report says what was left rather than leaving the admin to notice.
fn note_what_was_left_out(pack: &Map<String, Json>, notes: &mut Vec<String>) {
    for (key, one, many) in [
        ("page_groups", "page group was", "page groups were"),
        ("tags", "tag was", "tags were"),
        ("models", "model was", "models were"),
        (
            "model_instances",
            "model instance was",
            "model instances were",
        ),
        ("plugins", "plugin was", "plugins were"),
        ("code_pages", "code page was", "code pages were"),
    ] {
        let count = array(pack, key).len();
        if count > 0 {
            notes.push(format!(
                "{} not imported: Saltcorn 1 describes them against its own \
                 server-rendered UI and plugin system, which this system does not have",
                plural(count, one, many)
            ));
        }
    }
}

// --- reading the archive ---------------------------------------------------------

/// The Saltcorn version that wrote the archive, for the manifest to record.
fn v1_version(entries: &Entries) -> String {
    json_entry(entries, BACKUP_INFO)
        .ok()
        .and_then(|info| {
            info.get("saltcorn_version")
                .and_then(Json::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "1".to_owned())
}

/// When the v1 backup was taken, RFC 3339 as v1 writes it, or null.
fn backup_date(entries: &Entries) -> Json {
    json_entry(entries, BACKUP_INFO)
        .ok()
        .and_then(|info| info.get("backup_date").cloned())
        .unwrap_or(Json::Null)
}

/// A v1 table's rows: `tables/<name>.json`, an array. An absent entry is no rows,
/// which is what a table backed up without its data looks like.
fn table_rows(entries: &Entries, table: &str) -> Result<Vec<Json>> {
    let path = format!("tables/{table}.json");
    if !entries.contains_key(&path) {
        return Ok(Vec::new());
    }
    match json_entry(entries, &path)? {
        Json::Array(rows) => Ok(rows),
        _ => Err(Error::invalid(format!("`{path}` is not an array of rows"))),
    }
}

fn json_entry(entries: &Entries, path: &str) -> Result<Json> {
    let bytes = entries
        .get(path)
        .ok_or_else(|| Error::invalid(format!("the backup has no `{path}`")))?;
    serde_json::from_slice(bytes)
        .map_err(|e| Error::invalid(format!("`{path}` in the backup is not valid JSON: {e}")))
}

/// Rows as a rows entry: one compact object per line.
///
/// The values are v1's JSON as [`convert_row`] left it, which is already what
/// the restore reads by column type ([`super::values::column_value`]): text,
/// numbers, ISO timestamps and dates. Saltcorn 1 has no binary column type, so
/// nothing here needs the base64 a Feldspar backup writes bytes in.
fn ndjson(rows: &[Json]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut out, row)
            .map_err(|e| Error::msg(format!("could not write a converted row: {e}")))?;
        out.push(b'\n');
    }
    Ok(out)
}

fn pretty(value: &Json) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(value)
        .map_err(|e| Error::msg(format!("the imported backup could not be written: {e}")))
}

fn array(obj: &Map<String, Json>, key: &str) -> Vec<Json> {
    obj.get(key)
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `1 view was` / `7 views were` — a note that says "1 views were" reads like a
/// bug in the thing that wrote it, and these notes are what an admin judges the
/// import by. The verb is part of the two forms because English puts it there.
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn name_of(obj: &Map<String, Json>) -> String {
    obj.get("name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn text(obj: &Map<String, Json>, key: &str) -> String {
    obj.get(key).and_then(Json::as_str).unwrap_or("").to_owned()
}

fn flag(obj: &Map<String, Json>, key: &str) -> bool {
    flag_value(obj.get(key))
}

fn flag_value(value: Option<&Json>) -> bool {
    value.and_then(Json::as_bool).unwrap_or(false)
}

/// A v1 role number, defaulting to `public` for a value outside the scale — the
/// same 1–100 scale this system uses, so nothing is rescaled.
fn role(obj: &Map<String, Json>, key: &str) -> i64 {
    match obj.get(key).and_then(Json::as_i64) {
        Some(role) if (1..=100).contains(&role) => role,
        _ => 100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bcrypt hash as v1's bcryptjs writes one (the OpenBSD vector for `U*U`).
    const V1_HASH: &str = "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW";

    /// A small Saltcorn 1 archive, shaped exactly like a real one: a pack, a
    /// table's rows beside it, one user, one file, and the site name the store is
    /// named after.
    fn v1_archive() -> Entries {
        let pack = json!({
            "tables": [
                {
                    "name": "users",
                    "min_role_read": 1,
                    "min_role_write": 1,
                    "fields": [
                        { "name": "id", "type": "Integer", "primary_key": true },
                        { "name": "email", "type": "String" },
                        { "name": "role_id", "type": "Integer" },
                        { "name": "nickname", "label": "Nickname", "type": "String" },
                    ],
                },
                {
                    "name": "Books",
                    "description": "On the shelf",
                    "min_role_read": 80,
                    "min_role_write": 1,
                    "fields": [
                        { "name": "id", "label": "ID", "type": "Integer",
                          "primary_key": true, "required": true, "is_unique": true },
                        { "name": "title", "label": "Title", "type": "String",
                          "attributes": { "max_length": 200, "regexp": "",
                                          "options": " hardback, paperback " } },
                        { "name": "published_on", "type": "Date",
                          "attributes": { "day_only": true } },
                        { "name": "read_at", "type": "Date", "attributes": {} },
                        { "name": "borrowed_by", "type": "Key", "reftable_name": "users",
                          "refname": "id", "attributes": { "summary_field": "email" } },
                        { "name": "cover", "type": "File" },
                        { "name": "shelf", "type": "Rating" },
                        { "name": "pages_left", "type": "Integer", "calculated": true,
                          "expression": "500 - pages" },
                    ],
                },
            ],
            "roles": [ { "id": 1, "role": "admin" }, { "id": 100, "role": "public" } ],
            "triggers": [
                { "name": "Trim", "action": "run_js_code", "when_trigger": "Insert",
                  "table_name": "Books", "min_role": 100,
                  "configuration": { "code": "return 1;" } },
                { "name": "Nightly", "action": "run_js_code", "when_trigger": "Daily",
                  "table_name": null, "configuration": { "code": "return 2;" } },
                { "name": "OnPageLoad", "action": "run_js_code",
                  "when_trigger": "PageLoad", "configuration": {} },
                { "name": "Onboard", "action": "Workflow", "when_trigger": "Never",
                  "configuration": {}, "steps": [] },
            ],
            "views": [
                { "name": "BookList", "description": "", "viewtemplate": "List", "table": "Books",
                  "min_role": 80, "slug": null, "attributes": null,
                  "default_render_page": null, "exttable_name": null,
                  "configuration": { "columns": [
                      { "type": "ViewLink", "view": "Own:BookShow", "view_name": "BookShow" },
                  ] } },
                { "name": "Chat", "viewtemplate": "Room", "table": "Books", "min_role": 1,
                  "slug": { "label": "", "steps": [] },
                  "attributes": { "page_title": "Talk" }, "default_render_page": "Home",
                  "configuration": {} },
            ],
            "pages": [
                { "name": "Home", "title": "Welcome", "description": "", "min_role": 100,
                  "layout": { "type": "view", "view": "BookList", "state": "shared" },
                  "attributes": { "no_menu": false }, "root_page_for_roles": [100],
                  "fixed_states": {} },
            ],
            "page_groups": [ { "name": "Responsive" } ],
        });
        let mut entries = Entries::new();
        entries.insert(PACK.to_owned(), serde_json::to_vec(&pack).unwrap());
        entries.insert(
            BACKUP_INFO.to_owned(),
            serde_json::to_vec(&json!({
                "saltcorn_version": "1.7.0",
                "backup_date": "2026-09-12T16:50:42.895Z",
            }))
            .unwrap(),
        );
        entries.insert(
            "config/site_name".to_owned(),
            serde_json::to_vec(&json!({ "v": "BooksDB" })).unwrap(),
        );
        entries.insert(
            "tables/users.json".to_owned(),
            serde_json::to_vec(&json!([
                { "id": 7, "email": "admin@foo.com", "role_id": 1, "disabled": false,
                  "password": V1_HASH, "nickname": "Ada" }
            ]))
            .unwrap(),
        );
        entries.insert(
            "tables/Books.json".to_owned(),
            serde_json::to_vec(&json!([
                { "id": 1, "title": "Moby Dick", "published_on": "2026-09-02T00:00:00.000Z",
                  "read_at": "2026-09-02T11:30:00.000Z", "borrowed_by": 7,
                  "cover": "whale.png", "shelf": 3, "pages_left": 20 },
                { "id": 2, "title": "War and Peace", "published_on": "2026-08-31",
                  "read_at": null, "borrowed_by": 999, "cover": null }
            ]))
            .unwrap(),
        );
        entries.insert(
            "files.csv".to_owned(),
            b"filename,location,uploaded_at,size_kb,id,user_id,mime_super,mime_sub,\
              min_role_read,s3_store,isDirectory\n\
              whale.png,whale.png,2026-09-12T16:49:57.928Z,7,,7,image,png,40,,\n\
              gone.png,gone.png,2026-09-12T16:49:57.928Z,7,,7,image,png,1,,\n"
                .to_vec(),
        );
        entries.insert("files/whale.png".to_owned(), b"\x89PNG".to_vec());
        entries.insert(
            "config/menu_items".to_owned(),
            serde_json::to_vec(&json!({ "v": [
                { "type": "Admin Page", "label": "Tables", "admin_page": "Tables" },
                { "type": "Header", "label": "Library", "subitems": [
                    { "type": "View", "label": "Books", "viewname": "BookList", "min_role": 80 },
                    { "type": "User Page", "label": "Notifications", "user_page": "Notifications" },
                ] },
                { "type": "Header", "label": "Settings", "subitems": [
                    { "type": "Admin Page", "label": "Files", "admin_page": "Files" },
                ] },
                { "type": "Link", "label": "Docs", "url": "https://example.com/docs" },
                { "type": "User Page", "label": "Login", "user_page": "Login" },
            ] }))
            .unwrap(),
        );
        entries
    }

    /// One converted entry, back as JSON.
    fn document(entries: &Entries, path: &str) -> Json {
        serde_json::from_slice(
            entries
                .get(path)
                .unwrap_or_else(|| panic!("no `{path}` was converted")),
        )
        .expect("valid JSON")
    }

    /// A converted rows entry, back as the rows on its lines.
    fn rows_of(entries: &Entries, path: &str) -> Json {
        let bytes = entries
            .get(path)
            .unwrap_or_else(|| panic!("no `{path}` was converted"));
        Json::Array(
            bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).expect("a JSON row per line"))
                .collect(),
        )
    }

    fn notes(entries: &Entries) -> Vec<String> {
        let manifest = document(entries, MANIFEST_FILE);
        notes_of(manifest.as_object().expect("an object"))
    }

    #[test]
    fn a_v1_archive_is_recognised_and_ours_is_not() {
        assert!(is_v1_backup(v1_archive().keys().map(String::as_str)));
        let mut ours = Entries::new();
        ours.insert(MANIFEST_FILE.to_owned(), b"{}".to_vec());
        assert!(!is_v1_backup(ours.keys().map(String::as_str)));
    }

    /// The manifest a converted archive carries: this system's format, and where
    /// the file really came from.
    #[test]
    fn the_manifest_says_it_was_imported_and_from_what() {
        let out = convert(&v1_archive()).expect("a conversion");
        let manifest = document(&out, MANIFEST_FILE);
        assert_eq!(manifest["format"], json!(super::super::FORMAT));
        assert_eq!(manifest["version"], json!(super::super::FORMAT_VERSION));
        assert_eq!(
            manifest["feldspar_version"],
            json!(super::super::PRODUCT_VERSION)
        );
        assert_eq!(manifest["imported_from"]["product"], json!("saltcorn"));
        assert_eq!(manifest["imported_from"]["version"], json!("1.7.0"));
        assert_eq!(manifest["source"], json!("Saltcorn 1.7.0, imported"));
        // The age of the *data*, which is what the dialog's date means.
        assert_eq!(manifest["created_at"], json!("2026-09-12T16:50:42.895Z"));
        // And what there is to choose from, as the dialog reads it.
        let contents = Available::from_json(&manifest["contents"]).expect("contents");
        assert_eq!(contents.tables.len(), 1);
        assert_eq!(contents.tables[0].name, "Books");
        assert_eq!(contents.tables[0].count, Some(2));
        assert_eq!(contents.users, 1);
        assert_eq!(contents.file_stores[0].name, "BooksDB");
        // The two triggers with an event this system has; the workflow and the
        // `PageLoad` one are notes instead.
        assert_eq!(contents.triggers, 2);
    }

    #[test]
    fn a_table_becomes_its_columns_its_settings_and_its_rows() {
        let out = convert(&v1_archive()).expect("a conversion");
        let table = document(&out, "tables/Books/table.json");
        assert_eq!(table["table"]["label"], json!("Books"));
        assert_eq!(table["table"]["description"], json!("On the shelf"));
        assert_eq!(table["table"]["min_role_read"], json!(80));

        let field = |name: &str| -> Json {
            table["fields"]
                .as_array()
                .expect("fields")
                .iter()
                .find(|f| f["name"] == json!(name))
                .unwrap_or_else(|| panic!("no `{name}` column was converted"))
                .clone()
        };
        // The key that numbers itself: primary, so the schema editor gives it an
        // identity generator, and the restore winds it past the restored rows.
        assert_eq!(field("id")["type"], json!("integer"));
        assert_eq!(field("id")["primary_key"], json!(true));
        // A v1 `String` is this system's `string`, with the attributes it can
        // express — and v1's comma-separated options as the array this one takes.
        assert_eq!(field("title")["type"], json!("string"));
        assert_eq!(field("title")["attributes"]["max_length"], json!(200));
        assert_eq!(
            field("title")["attributes"]["options"],
            json!(["hardback", "paperback"])
        );
        // An empty v1 pattern is not a pattern that matches nothing.
        assert_eq!(field("title")["attributes"]["regex"], Json::Null);
        // v1 stores every date as a timestamp; only a day-only one is a `date`.
        assert_eq!(field("published_on")["type"], json!("date"));
        assert_eq!(field("read_at")["type"], json!("timestamp"));
        // A reference keeps its target and its summary field, and takes no type:
        // its storage is its target's, which the schema editor fills in.
        assert_eq!(field("borrowed_by")["kind"]["type"], json!("key"));
        assert_eq!(field("borrowed_by")["kind"]["target_table"], json!("users"));
        assert_eq!(
            field("borrowed_by")["kind"]["summary_field"],
            json!("email")
        );
        assert_eq!(field("borrowed_by")["type"], Json::Null);
        // A v1 file field points into the store this import creates.
        assert_eq!(field("cover")["kind"]["type"], json!("file"));
        assert_eq!(field("cover")["kind"]["store"], json!("BooksDB"));

        let rows = rows_of(&out, "tables/Books/rows.ndjson");
        let rows = rows.as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        // A day-only date loses the time v1 stored it with; a real timestamp
        // keeps it.
        assert_eq!(rows[0]["published_on"], json!("2026-09-02"));
        assert_eq!(rows[0]["read_at"], json!("2026-09-02T11:30:00.000Z"));
        // The columns that were not imported take their values with them: a value
        // with no column is a value nothing could read.
        assert!(rows[0].get("shelf").is_none());
        assert!(rows[0].get("pages_left").is_none());
    }

    /// The `pgvector` plugin's `PGVector` (TODO-agent-skills AS2.6): a vector
    /// field of its `dimensions`, else of its values' length, and its values —
    /// pgvector's text — as arrays. One that says neither is reported.
    #[test]
    fn a_pgvector_field_becomes_a_vector_of_its_dimensions() {
        let mut archive = v1_archive();
        let mut pack: Json = serde_json::from_slice(&archive[PACK]).unwrap();
        pack["tables"].as_array_mut().unwrap().push(json!({
            "name": "Docs",
            "fields": [
                { "name": "id", "type": "Integer", "primary_key": true },
                { "name": "sized", "type": "PGVector", "attributes": { "dimensions": 3 } },
                { "name": "unsized", "type": "PGVector", "attributes": {} },
                { "name": "empty", "type": "PGVector" },
            ],
        }));
        archive.insert(PACK.to_owned(), serde_json::to_vec(&pack).unwrap());
        archive.insert(
            "tables/Docs.json".to_owned(),
            serde_json::to_vec(&json!([
                { "id": 1, "sized": "[0.1,0.2,0.3]", "unsized": null, "empty": null },
                { "id": 2, "sized": null, "unsized": "[1,0]", "empty": null },
            ]))
            .unwrap(),
        );
        let out = convert(&archive).expect("a conversion");
        let table = document(&out, "tables/Docs/table.json");
        let field = |name: &str| {
            table["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == json!(name))
                .cloned()
        };
        assert_eq!(field("sized").unwrap()["type"], json!("vector(3)"));
        assert_eq!(field("unsized").unwrap()["type"], json!("vector(2)"));
        assert!(field("empty").is_none());
        let rows = rows_of(&out, "tables/Docs/rows.ndjson");
        assert_eq!(rows[0]["sized"], json!([0.1, 0.2, 0.3]));
        assert_eq!(rows[1]["unsized"], json!([1, 0]));
        assert!(rows[0].get("empty").is_none());
        let notes = notes(&out);
        assert!(
            notes
                .iter()
                .any(|n| n.contains("`Docs.empty`") && n.contains("dimensions")),
            "{notes:?}"
        );
    }

    /// Two columns v1 has and this system does not: the note is the point, since
    /// a column that quietly vanishes is a column an admin finds out about later.
    #[test]
    fn a_column_with_no_counterpart_is_reported() {
        let out = convert(&v1_archive()).expect("a conversion");
        let notes = notes(&out);
        assert!(
            notes
                .iter()
                .any(|n| n.contains("`Books.shelf`") && n.contains("`Rating`")),
            "{notes:?}"
        );
        // A calculated column is not one of them any more: it crosses as a
        // calculated field ([`formulas`]), for the schema editor to check.
        assert!(
            !notes.iter().any(|n| n.contains("`Books.pages_left`")),
            "{notes:?}"
        );
        let table = document(&out, "tables/Books/table.json");
        let pages_left = table["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .find(|f| f["name"] == json!("pages_left"))
            .expect("the calculated column")
            .clone();
        assert_eq!(
            pages_left["kind"],
            json!({ "type": "calc", "expression": "500 - pages" })
        );
        assert_eq!(pages_left["type"], json!("integer"));
        // Views and pages are imported now; page groups still are not.
        assert!(
            !notes
                .iter()
                .any(|n| n.contains("view was not") || n.contains("page was not")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("1 page group was not imported")),
            "{notes:?}"
        );
    }

    /// The two translations that are not mechanical: a user's key changes, and
    /// every reference to it changes with it.
    #[test]
    fn a_user_keeps_its_v1_number_and_every_reference_is_rewritten() {
        let out = convert(&v1_archive()).expect("a conversion");
        let users = document(&out, "users/users.json");
        let accounts = rows_of(&out, "users/rows.ndjson");
        let user = &accounts[0];
        assert_eq!(user["email"], json!("admin@foo.com"));
        assert_eq!(user["role"], json!(1));
        assert_eq!(user["legacy_id"], json!(7));
        // A column an admin added to v1's users table comes with it…
        assert_eq!(user["nickname"], json!("Ada"));
        // …and the restore is told to add it, along with `legacy_id`.
        let fields: Vec<&str> = users["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert_eq!(fields, vec![LEGACY_ID, "nickname"]);
        // The key is this system's, not v1's.
        let id = user["id"].as_str().expect("an id");
        assert!(Uuid::parse_str(id).is_ok(), "{id}");
        // A v1 password hash is bcrypt, which this system verifies too: it is
        // carried as it is, and the account signs in with the password it had.
        assert_eq!(user[sc_auth::COL_PASSWORD_HASH], json!(V1_HASH));
        assert!(
            !notes(&out).iter().any(|n| n.contains("with no password")),
            "{:?}",
            notes(&out)
        );

        // And the reference: the row that pointed at user 7 points at the UUID
        // user 7 became. One that pointed at a user this backup does not carry
        // points at nothing, which is the only honest answer.
        let rows = rows_of(&out, "tables/Books/rows.ndjson");
        assert_eq!(rows[0]["borrowed_by"], json!(id));
        assert_eq!(rows[1]["borrowed_by"], Json::Null);
        // The roles the users point at travel too.
        assert_eq!(users["roles"][0], json!({ "role": 1, "name": "admin" }));
    }

    /// v1 has one file area per application; this system has named stores, so the
    /// import makes one named after the application.
    #[test]
    fn the_files_become_a_local_store_named_after_the_application() {
        let out = convert(&v1_archive()).expect("a conversion");
        let store = document(&out, "file-stores/BooksDB/store.json");
        assert_eq!(store["definition"]["name"], json!("BooksDB"));
        assert_eq!(
            store["definition"]["backend"],
            json!(sc_files::LOCAL_BACKEND)
        );
        assert_eq!(
            store["definition"]["config"][sc_files::CFG_PATH],
            json!(
                sc_files::suggest_local_dir("BooksDB")
                    .unwrap()
                    .to_string_lossy()
            )
        );
        // Nothing has put files in that directory yet, so the store makes it.
        assert_eq!(
            store["definition"]["config"][sc_files::CFG_CREATE],
            json!(true)
        );

        assert_eq!(store["files"][0]["path"], json!("whale.png"));
        assert_eq!(store["files"][0]["min_role"], json!(40));
        // Who uploaded it, as the user that v1 user became.
        let owner = store["files"][0]["owner"].as_str().expect("an owner");
        let accounts = rows_of(&out, "users/rows.ndjson");
        assert_eq!(json!(owner), accounts[0]["id"]);
        assert_eq!(
            out.get("file-stores/BooksDB/files/whale.png"),
            Some(&b"\x89PNG".to_vec())
        );

        // A file listed with no bytes in the archive is reported rather than
        // restored as an empty file.
        assert!(
            notes(&out).iter().any(|n| n.contains("`gone.png`")),
            "{:?}",
            notes(&out)
        );
    }

    /// v1's actions, under the names they had: where this system has the same
    /// action the settings mean the same thing, and where it does not, the restore
    /// refuses the trigger by name rather than guessing.
    #[test]
    fn actions_become_triggers_and_the_events_it_does_not_have_are_reported() {
        let out = convert(&v1_archive()).expect("a conversion");
        let triggers = document(&out, "triggers.json");
        let triggers = triggers.as_array().expect("triggers");
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0]["name"], json!("Trim"));
        assert_eq!(triggers[0]["when"], json!("insert"));
        assert_eq!(triggers[0]["channel"], json!("Books"));
        assert_eq!(triggers[0]["action"], json!("run_js_code"));
        assert_eq!(triggers[0]["configuration"]["code"], json!("return 1;"));
        assert_eq!(triggers[0]["min_role"], json!(100));
        // A scheduled trigger has no table, and a channel on one would be refused.
        assert_eq!(triggers[1]["when"], json!("daily"));
        assert!(triggers[1].get("channel").is_none());

        let notes = notes(&out);
        assert!(
            notes
                .iter()
                .any(|n| n.contains("`OnPageLoad`") && n.contains("PageLoad")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("`Onboard`") && n.contains("workflow")),
            "{notes:?}"
        );
    }

    /// One application per backup (§13), in exactly the shape a Feldspar backup
    /// carries an application in — which is why it is read back here through the
    /// admin API's own parser.
    #[test]
    fn the_backup_becomes_one_saltcorn_ui_application_named_after_the_site() {
        let out = convert(&v1_archive()).expect("a conversion");
        let document = document(&out, "applications/booksdb.json");
        // No id: the restore matches an import by its name (8.6).
        assert!(document.get("id").is_none(), "{document}");
        let app = crate::handlers::application_from_body(sc_app::AppId::new(), &document)
            .expect("the admin API's parser reads it");
        assert_eq!(app.name, "BooksDB");
        assert_eq!(app.subdomain, "booksdb");
        assert_eq!(app.framework.name, SALTCORN_UI_FRAMEWORK);
        assert_eq!(app.framework.config[CFG_SITE_NAME], json!("BooksDB"));
        sc_viewpattern::check_saltcorn_ui_config(&app.framework.config)
            .expect("settings the framework accepts");
        // Every imported table (not `users`, which is not an ordinary table
        // here), the store, and every trigger the views may name.
        let names = |ids: Vec<String>| ids;
        assert_eq!(
            names(app.tables.iter().map(|t| t.0.clone()).collect()),
            vec!["Books"]
        );
        assert_eq!(
            names(app.file_stores.iter().map(|s| s.0.clone()).collect()),
            vec!["BooksDB"]
        );
        assert_eq!(
            names(app.triggers.iter().map(|t| t.0.clone()).collect()),
            vec!["Trim", "Nightly"]
        );
        assert_eq!(app.csp, saltcorn_ui_csp());

        let manifest = document_of_manifest(&out);
        let contents = Available::from_json(&manifest["contents"]).expect("contents");
        assert_eq!(contents.applications[0].name, "booksdb");
        assert_eq!(contents.applications[0].label, "BooksDB");
        assert_eq!((contents.views, contents.pages), (2, 1));
    }

    fn document_of_manifest(entries: &Entries) -> Json {
        document(entries, MANIFEST_FILE)
    }

    /// §13's "configuration crosses unchanged", and 8.5's `min_role`, `slug`,
    /// `attributes` and `root_page_for_roles`. A pattern this server may not have
    /// (`Room`) is carried all the same: whether it is registered is the restore's
    /// question.
    #[test]
    fn views_and_pages_cross_with_their_configuration_unchanged() {
        let archive = v1_archive();
        let pack: Json = serde_json::from_slice(&archive[PACK]).unwrap();
        let out = convert(&archive).expect("a conversion");
        let id = sc_app::AppId::new();

        let views = document(&out, "applications/booksdb/views.json");
        let views: Vec<sc_viewpattern::View> = views
            .as_array()
            .expect("views")
            .iter()
            .map(|v| {
                crate::handlers::view_from_body(sc_viewpattern::ViewId::new(), id, v)
                    .expect("the parser reads it")
            })
            .collect();
        assert_eq!(views.len(), 2);
        let list = &views[0];
        assert_eq!(list.name, "BookList");
        assert_eq!(list.viewpattern, "List");
        assert_eq!(list.table_name.as_deref(), Some("Books"));
        assert_eq!(list.min_role, 80);
        assert_eq!(
            Json::Object(list.configuration.clone()),
            pack["views"][0]["configuration"]
        );
        // v1's `null`s: no slug, no attributes.
        assert_eq!(list.slug, None);
        assert!(list.attributes.is_empty());
        let chat = &views[1];
        assert_eq!(chat.viewpattern, "Room");
        assert_eq!(chat.slug, Some(json!({ "label": "", "steps": [] })));
        assert_eq!(chat.attributes["page_title"], json!("Talk"));
        assert_eq!(chat.attributes["default_render_page"], json!("Home"));

        let pages = document(&out, "applications/booksdb/pages.json");
        let page = crate::handlers::page_from_body(sc_viewpattern::PageId::new(), id, &pages[0])
            .expect("the parser reads it");
        assert_eq!(page.name, "Home");
        assert_eq!(page.title, "Welcome");
        assert_eq!(page.min_role, 100);
        assert_eq!(page.layout, pack["pages"][0]["layout"]);
        assert_eq!(page.attributes["root_page_for_roles"], json!([100]));
        assert_eq!(page.attributes["no_menu"], json!(false));
        // An empty `fixed_states` is no fixed states.
        assert!(page.attributes.get("fixed_states").is_none());
    }

    /// 8.3: the admin and user pages go, each kind with a note; a header left
    /// empty goes with them; the rest keeps its nesting.
    #[test]
    fn the_menu_loses_v1s_own_screens_and_keeps_its_nesting() {
        let out = convert(&v1_archive()).expect("a conversion");
        let document = document(&out, "applications/booksdb.json");
        assert_eq!(
            document["framework"]["config"][MENU_CONFIG_KEY],
            json!([
                { "type": "Header", "label": "Library", "subitems": [
                    { "type": "View", "label": "Books", "viewname": "BookList", "min_role": 80 },
                ] },
                { "type": "Link", "label": "Docs", "url": "https://example.com/docs" },
            ])
        );
        let notes = notes(&out);
        let noted = |text: &str| notes.iter().any(|n| n.contains(text));
        assert!(
            noted("2 Admin Page menu entries were not imported"),
            "{notes:?}"
        );
        assert!(
            noted("2 User Page menu entries were not imported"),
            "{notes:?}"
        );
        assert!(
            noted("the menu header `Settings` was not imported"),
            "{notes:?}"
        );
        assert!(!noted("`Library`"), "{notes:?}");
    }

    #[test]
    fn a_subdomain_is_derived_from_any_name() {
        assert_eq!(subdomain_for("BooksDB"), "booksdb");
        assert_eq!(subdomain_for("  My Books & Stuff! "), "my-books-stuff");
        assert_eq!(subdomain_for("Café 2"), "caf-2");
        assert_eq!(subdomain_for("日本"), "saltcorn-import");
        let long = subdomain_for(&"a-".repeat(60));
        assert!(long.len() <= 63 && !long.ends_with('-'), "{long}");
    }

    /// An archive with no `site_name` still has to name its store something, and
    /// a v1 event this system shares is spelled the way this system spells it.
    /// [`v1_archive`] with its pack changed by `edit`.
    fn with_pack(edit: impl FnOnce(&mut Json)) -> Entries {
        let mut entries = v1_archive();
        let mut pack: Json = serde_json::from_slice(&entries[PACK]).unwrap();
        edit(&mut pack);
        entries.insert(PACK.to_owned(), serde_json::to_vec(&pack).unwrap());
        entries
    }

    /// The builder 4.1: v1's library entries have no ids, so each gets a UUID by
    /// its position — the serial v1's own restore gives it — and every
    /// `library_id` naming that serial is rewritten, in views, pages and the
    /// items' own layouts. One naming no entry is left as written.
    #[test]
    fn library_items_get_uuids_and_every_placement_is_rewritten() {
        let out = convert(&with_pack(|pack| {
            pack["library"] = json!([
                { "name": "Book header", "icon": "fas fa-book", "layout": { "above": [
                    { "type": "blank", "contents": "Book", "textStyle": "h3" },
                    { "type": "library-slot", "name": "title" },
                    { "type": "library", "library_id": 2, "slots": [] },
                ]}},
                // v1's constructor reads a layout stored as text.
                { "name": "Byline", "icon": "", "layout": "{\"type\":\"blank\",\"contents\":\"by\"}" },
            ]);
            pack["views"][0]["configuration"]["layout"] = json!({ "above": [
                { "type": "library", "library_id": 1, "slots": [
                    { "name": "title", "kind": "field", "field": "title", "fieldview": "as_text" },
                ]},
                { "type": "library", "library_id": 9 },
            ]});
            pack["pages"][0]["layout"] = json!({ "besides": [
                { "type": "library", "library_id": "2" },
                { "type": "view", "view": "BookList", "state": "shared" },
            ]});
        }))
        .expect("a conversion");

        let library = document(&out, "applications/booksdb/library.json");
        let app = sc_app::AppId::new();
        let items: Vec<sc_viewpattern::LibraryItem> = library
            .as_array()
            .expect("items")
            .iter()
            .map(|item| {
                let id = Uuid::parse_str(item["id"].as_str().expect("an id")).expect("a UUID");
                crate::handlers::library_item_from_body(
                    sc_viewpattern::LibraryItemId(id),
                    app,
                    item,
                )
                .expect("the parser reads it")
            })
            .collect();
        assert_eq!(items.len(), 2);
        let (header, byline) = (&items[0], &items[1]);
        assert_eq!(header.name, "Book header");
        assert_eq!(header.icon, "fas fa-book");
        assert_eq!(byline.layout, json!({ "type": "blank", "contents": "by" }));
        let uuid_of = |item: &sc_viewpattern::LibraryItem| json!(item.id.0.to_string());
        // Nested: the header's own placement of serial 2.
        assert_eq!(header.layout["above"][2]["library_id"], uuid_of(byline));
        // The slot is untouched.
        assert_eq!(
            header.layout["above"][1],
            json!({ "type": "library-slot", "name": "title" })
        );

        let views = document(&out, "applications/booksdb/views.json");
        let placed = &views[0]["configuration"]["layout"]["above"];
        assert_eq!(placed[0]["library_id"], uuid_of(header));
        assert_eq!(placed[0]["slots"][0]["field"], json!("title"));
        // A serial no entry had: left as written, for the restore to report.
        assert_eq!(placed[1]["library_id"], json!(9));

        let pages = document(&out, "applications/booksdb/pages.json");
        assert_eq!(
            pages[0]["layout"]["besides"][0]["library_id"],
            uuid_of(byline)
        );

        let notes = notes(&out);
        assert!(!notes.iter().any(|n| n.contains("library")), "{notes:?}");
    }

    /// The builder 4.2: v1's `getEditNormalPage` fold, with JavaScript's
    /// truthiness — and `fixed_states` is not kept.
    #[test]
    fn a_pages_legacy_fixed_states_are_folded_into_its_view_segments() {
        let out = convert(&with_pack(|pack| {
            pack["pages"][0]["layout"] = json!({ "above": [
                { "type": "view", "name": "legacy", "view": "BookList", "state": "fixed" },
                { "type": "view", "name": "nulled", "view": "BookList", "state": "fixed",
                  "configuration": null },
                // `{}` is truthy: it already has a configuration.
                { "type": "view", "name": "modern", "view": "BookList", "state": "fixed",
                  "configuration": {} },
                // Only a fixed-state segment is folded.
                { "type": "view", "name": "shared", "view": "BookList", "state": "shared" },
                { "type": "container", "contents":
                    { "type": "view", "name": "inside", "view": "BookList", "state": "fixed" } },
                { "type": "view", "name": "unlisted", "view": "BookList", "state": "fixed" },
            ]});
            pack["pages"][0]["fixed_states"] = json!({
                "legacy": { "id": 1 }, "nulled": { "id": 2 }, "modern": { "id": 3 },
                "shared": { "id": 4 }, "inside": { "title": "Moby Dick" },
            });
        }))
        .expect("a conversion");
        let pages = document(&out, "applications/booksdb/pages.json");
        let page = &pages[0];
        let above = &page["layout"]["above"];
        assert_eq!(above[0]["configuration"], json!({ "id": 1 }));
        assert_eq!(above[1]["configuration"], json!({ "id": 2 }));
        assert_eq!(above[2]["configuration"], json!({}));
        assert!(above[3].get("configuration").is_none(), "{page}");
        assert_eq!(
            above[4]["contents"]["configuration"],
            json!({ "title": "Moby Dick" })
        );
        assert!(above[5].get("configuration").is_none(), "{page}");
        assert!(page["attributes"].get("fixed_states").is_none(), "{page}");

        // v1 also stores it as text.
        let out = convert(&with_pack(|pack| {
            pack["pages"][0]["layout"] =
                json!({ "type": "view", "name": "a", "view": "BookList", "state": "fixed" });
            pack["pages"][0]["fixed_states"] = json!("{\"a\":{\"id\":7}}");
        }))
        .expect("a conversion");
        let pages = document(&out, "applications/booksdb/pages.json");
        assert_eq!(pages[0]["layout"]["configuration"], json!({ "id": 7 }));
    }

    #[test]
    fn an_unnamed_application_still_names_its_store() {
        let mut entries = v1_archive();
        entries.remove("config/site_name");
        let out = convert(&entries).expect("a conversion");
        assert!(out.contains_key("file-stores/saltcorn-import/store.json"));
        assert_eq!(convert_event("Never"), Some("none"));
        assert_eq!(convert_event("Insert"), Some("insert"));
        assert_eq!(convert_event("Validate"), None);
    }

    /// A real v1 backup with an agent using every skill of v1's agents plugin
    /// (`tests/fixtures/make-v1-agents-fixture.py`).
    fn every_skill_archive() -> Entries {
        let bytes: &[u8] =
            include_bytes!("../../tests/fixtures/saltcorn-v1-feldsparimport-agents.zip");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a zip");
        let mut entries = Entries::new();
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).expect("an entry");
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut data).expect("readable");
            entries.insert(file.name().to_owned(), data);
        }
        entries
    }

    /// TODO-agent-skills AS12.13: every skill crosses, and the one note left
    /// about the agent is the one that is not the importer's to do — copying a
    /// SKILL.md the backup does not carry.
    #[test]
    fn every_v1_agent_skill_crosses_with_no_skill_left_behind() {
        let out = convert(&every_skill_archive()).expect("converted");
        let notes = notes(&out);
        let about_agents: Vec<&String> = notes
            .iter()
            .filter(|n| {
                ["EverySkill", "agent", "skill", "memor", "LLM", "MCP"]
                    .iter()
                    .any(|w| n.contains(w))
            })
            .collect();
        assert_eq!(about_agents.len(), 1, "{about_agents:#?}");
        assert!(
            about_agents[0].contains("External Skill"),
            "{about_agents:#?}"
        );

        let agents = document(&out, "agents.json");
        let agent = agents
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == json!("EverySkill"))
            .expect("the agent");
        let traits: Vec<&str> = agent["traits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["trait"].as_str())
            .collect();
        assert_eq!(
            traits,
            [
                "query_table",
                "insert_row",
                "run_js_code",
                "run_js_code",
                "run_trigger",
                "subagent",
                "http",
                "search_table",
                "similar_rows",
                "preload_rows",
                "preload_code",
                "memory",
                "mcp_server",
                "web_search",
                "generate_js",
                "ask_user",
                "plan_approval",
                "generate_image",
                "speech",
                "skill_file",
                "prompt_picker",
            ]
        );
        assert_eq!(
            agent["attributes"]["pickable_models"],
            json!([ { "provider": "openai", "model": "gpt-5-mini", "label": "Fast" } ])
        );

        // What the skills need outside the agent.
        assert!(!out.contains_key("tables/AgentLongTermMemory/table.json"));
        assert_eq!(
            rows_of(&out, "agent-memory.ndjson")
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let servers = document(&out, "mcp-servers.json");
        assert_eq!(servers[0]["name"], json!("docs"));
        let chunks = document(&out, "tables/chunks/table.json");
        let embedding = chunks["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!("embedding"))
            .unwrap();
        assert_eq!(
            embedding["attributes"]["embed_model"],
            json!("text-embedding-3-small")
        );
        let document_table = document(&out, "tables/document/table.json");
        assert_eq!(
            document_table["constraints"][0]["type"],
            json!("full_text_search")
        );
        let triggers = document(&out, "triggers.json");
        let consolidate = triggers
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!("ConsolidateMemories"))
            .expect("the consolidation");
        assert_eq!(consolidate["action"], json!("consolidate_memory"));
        assert_eq!(
            consolidate["configuration"],
            json!({ "agent": "EverySkill" })
        );
        // The generated images and the skill file need the store.
        assert!(
            out.keys().any(|k| k.ends_with("/store.json")),
            "{:?}",
            out.keys()
        );
    }
}
