//! Tables a module declares (TODO-imap M4): created through the schema editor,
//! brought up to date on every load, never dropped, and — the part the mail
//! module depends on — ordinary tables whose ownership formulas join through
//! each other (M4.4).
//!
//! Most of this needs no module at all: what a declaration *does* is
//! [`sc_module::tables::sync`], and it is called here with the declarations
//! a package would answer. The last test loads a fixture package on a Deno
//! worker, installs it twice and upgrades it, which is the path a server takes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_api::ApiProvider;
use sc_api::code_host::TableHost;
use sc_api::schema_edit::{self, ApplyOptions, Operation};
use sc_catalog::{Catalog, ModuleTables, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{CodeHost, DenoEvaluator, JsEvaluator};
use sc_module::TableDecl;
use sc_module::tables::{claims, parse_declarations, sync};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

const MODULE: &str = "@feldspar/imap";

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_auth::bootstrap(&cat).await?;
    sc_auth::bootstrap_roles(&cat).await?;
    bootstrap_table_meta(&cat).await?;
    bootstrap_field_meta(&cat).await?;
    Ok(cat)
}

fn decls(raw: Json) -> Vec<TableDecl> {
    let (decls, issues) = parse_declarations(raw.as_array().unwrap());
    assert!(issues.is_empty(), "{issues:?}");
    decls
}

/// TODO-imap §3's three ownership rules, with `user.role` (the user object's
/// field) where the design wrote `role_id`. The account's own rule lets anyone
/// at or above `min_role` **read** a shared account, and nobody but its owner
/// write one.
const ACCOUNT_RULE: &str =
    "owner === user.id || (_read && owner === null && user.role <= min_role)";
const MESSAGE_RULE: &str = "accountⱵowner === user.id || \
     (accountⱵowner === null && user.role <= accountⱵmin_role)";
const ATTACHMENT_RULE: &str = "messageⱵaccountⱵowner === user.id || \
     (messageⱵaccountⱵowner === null && user.role <= messageⱵaccountⱵmin_role)";

/// The mail module's tables, cut down to what ownership needs.
fn mail_tables() -> Vec<TableDecl> {
    decls(json!([
        {
            "name": "mail_accounts", "label": "Mail accounts",
            "fields": [
                { "name": "owner", "references": "users" },
                { "name": "min_role", "type": "int", "required": true, "default": 40 },
                { "name": "label", "type": "text" },
                { "name": "display_name", "type": "text" },
                { "name": "imap_password", "secret": true },
            ],
            "ownership": ACCOUNT_RULE, "min_role_read": 1, "min_role_write": 1,
        },
        {
            "name": "mail_folders",
            "fields": [
                { "name": "account", "references": "mail_accounts", "required": true },
                { "name": "path", "type": "text", "required": true },
            ],
            "unique": [["account", "path"]],
            "ownership": MESSAGE_RULE, "min_role_read": 1, "min_role_write": 1,
        },
        {
            "name": "mail_messages",
            "fields": [
                { "name": "account", "references": "mail_accounts", "required": true },
                { "name": "subject", "type": "text" },
                { "name": "seen", "type": "bool" },
            ],
            "indexes": [["account", "subject"]],
            "ownership": MESSAGE_RULE, "min_role_read": 1, "min_role_write": 1,
        },
        {
            "name": "mail_attachments",
            "fields": [
                { "name": "message", "references": "mail_messages", "required": true },
                { "name": "filename", "type": "text" },
            ],
            "ownership": ATTACHMENT_RULE, "min_role_read": 1, "min_role_write": 1,
        },
    ]))
}

#[tokio::test]
async fn declared_tables_are_created_once_and_a_second_sync_changes_nothing() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let tables = mail_tables();

    let first = sync(&cat, MODULE, &tables, &ModuleTables::new()).await;
    assert!(first.issues.is_empty(), "{:?}", first.issues);
    assert_eq!(
        first.tables_created,
        [
            "mail_accounts",
            "mail_folders",
            "mail_messages",
            "mail_attachments"
        ]
    );
    let accounts = cat.require("mail_accounts")?;
    assert_eq!(
        accounts.primary_key,
        ["id"],
        "every module table is keyed by `id`"
    );
    assert_eq!(accounts.label, "Mail accounts");
    assert!(accounts.field("imap_password").unwrap().is_secret());
    assert!(
        accounts.ownership.is_some(),
        "the account rule is live: {:?}",
        accounts.ownership_error
    );
    let folders = cat.require("mail_folders")?;
    assert!(
        folders.constraints.iter().any(|c| matches!(
            &c.kind,
            sc_catalog::ConstraintKind::Unique { fields } if fields == &["account", "path"]
        )),
        "{:?}",
        folders.constraints
    );
    for table in ["mail_messages", "mail_attachments"] {
        let t = cat.require(table)?;
        assert!(t.ownership.is_some(), "{table}: {:?}", t.ownership_error);
    }
    // A declared index (TODO-imap M9.4), read back as one, so the second sync
    // below does not make it again.
    let messages = cat.require("mail_messages")?;
    assert!(
        messages.constraints.iter().any(|c| matches!(
            &c.kind,
            sc_catalog::ConstraintKind::Index { fields, .. } if fields == &["account", "subject"]
        )),
        "{:?}",
        messages.constraints
    );

    // The admin narrows a setting; a second load is a no-op and keeps it.
    schema_edit::apply(
        &cat,
        &[Operation::AlterTable {
            table: "mail_accounts".into(),
            settings: schema_edit::TableSettings {
                label: Some("Our mailboxes".into()),
                ..Default::default()
            },
        }],
        &ApplyOptions::default(),
    )
    .await?;
    let second = sync(&cat, MODULE, &tables, &ModuleTables::new()).await;
    assert!(second.issues.is_empty(), "{:?}", second.issues);
    assert!(second.is_noop(), "{second:?}");
    assert_eq!(cat.require("mail_accounts")?.label, "Our mailboxes");
    Ok(())
}

#[tokio::test]
async fn an_upgrade_adds_fields_keeps_the_admins_and_reports_a_changed_type() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let v1 = decls(json!([
        { "name": "mail_folders", "fields": [
            { "name": "path", "type": "text", "required": true },
            { "name": "uidvalidity", "type": "text" },
        ] },
    ]));
    sync(&cat, MODULE, &v1, &ModuleTables::new()).await;
    db.client()
        .await?
        .batch_execute("INSERT INTO mail_folders (path) VALUES ('INBOX')")
        .await
        .unwrap();
    // An admin's own field on the module's table.
    schema_edit::apply(
        &cat,
        &[Operation::AddField {
            table: "mail_folders".into(),
            field: schema_edit::FieldSpec {
                name: "colour".into(),
                type_name: "text".into(),
                ..Default::default()
            },
        }],
        &ApplyOptions::default(),
    )
    .await?;

    // The next version adds a required field and now wants `uidvalidity` an
    // int. The table has a row, so the new field goes in nullable; the type is
    // reported, not changed.
    let v2 = decls(json!([
        { "name": "mail_folders", "fields": [
            { "name": "path", "type": "text", "required": true },
            { "name": "uidvalidity", "type": "int" },
            { "name": "last_uid", "type": "int", "required": true, "default": 0 },
        ], "unique": [["path"]], "indexes": [["path", "last_uid"]] },
    ]));
    let report = sync(&cat, MODULE, &v2, &ModuleTables::new()).await;
    assert_eq!(report.fields_added, ["mail_folders.last_uid"]);
    assert!(
        report
            .constraints_added
            .iter()
            .any(|c| c.contains("path_last_uid")),
        "the new version's index is added: {:?}",
        report.constraints_added
    );
    assert_eq!(report.issues.len(), 1, "{:?}", report.issues);
    assert!(
        report.issues[0].contains("`mail_folders.uidvalidity` is declared as `int` but is `text`"),
        "{}",
        report.issues[0]
    );
    let folders = cat.require("mail_folders")?;
    assert!(!folders.field("last_uid").unwrap().required);
    assert_eq!(
        folders.field("uidvalidity").unwrap().base.type_.name(),
        "text"
    );
    assert!(folders.field("colour").is_some(), "the admin's field stays");

    // And again: nothing left to do, the same issue.
    let again = sync(&cat, MODULE, &v2, &ModuleTables::new()).await;
    assert!(again.is_noop(), "{again:?}");
    assert_eq!(again.issues, report.issues);
    Ok(())
}

#[tokio::test]
async fn a_declared_table_and_field_cannot_be_dropped_while_claimed() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let tables = mail_tables();
    sync(&cat, MODULE, &tables, &ModuleTables::new()).await;
    schema_edit::apply(
        &cat,
        &[Operation::AddField {
            table: "mail_messages".into(),
            field: schema_edit::FieldSpec {
                name: "handled".into(),
                type_name: "bool".into(),
                ..Default::default()
            },
        }],
        &ApplyOptions::default(),
    )
    .await?;
    cat.set_module_tables(claims([(MODULE, tables.as_slice())]))?;

    let drop = |op: Operation| {
        let cat = &cat;
        async move { schema_edit::apply(cat, &[op], &ApplyOptions::default()).await }
    };
    let refused = drop(Operation::DropField {
        table: "mail_messages".into(),
        field: "subject".into(),
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("@feldspar/imap"), "{refused}");
    let refused = drop(Operation::DropTable {
        table: "mail_attachments".into(),
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("while the module is installed"),
        "{refused}"
    );
    // The admin's own field is theirs to drop.
    drop(Operation::DropField {
        table: "mail_messages".into(),
        field: "handled".into(),
    })
    .await?;

    // Uninstalled: nothing is claimed, and the table is an ordinary one.
    cat.set_module_tables(ModuleTables::new())?;
    drop(Operation::DropTable {
        table: "mail_attachments".into(),
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_table_another_module_declared_is_left_alone() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let mut others = ModuleTables::new();
    others.claim("mail_accounts", "@acme/mail", vec![]);
    let report = sync(&cat, MODULE, &mail_tables()[..1], &others).await;
    assert!(report.tables_created.is_empty());
    assert!(
        report.issues[0].contains("the module @acme/mail already declares"),
        "{:?}",
        report.issues
    );
    assert!(cat.get("mail_accounts")?.is_none());
    Ok(())
}

// --- M4.4: ownership that joins through the module's own tables ----------------

/// A host acting for `user` — `asUser()`, the authority a module action reads
/// an account with — with the formula engine behind it for the writes.
fn as_user<'a>(cat: &'a Catalog, user: &sc_auth::User) -> TableHost<'a> {
    // A formula isolate in a binary that also runs module workers: primed, as a
    // server is (`tests/two_pools.rs`), or the next module worker any other test
    // starts aborts the whole binary.
    #[cfg(feature = "deno-host")]
    sc_expr::set_isolate_prime(sc_module::prime_v8);
    TableHost::new(cat)
        .caused_by(
            user.role,
            Some(json!({ "id": user.id.to_string(), "role": user.role })),
        )
        .with_evaluator(Some(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>))
}

async fn ask(host: &TableHost<'_>, plan: Json) -> Result<Json> {
    host.call(plan).await
}

async fn subjects(host: &TableHost<'_>) -> Vec<String> {
    let rows = ask(
        host,
        json!({ "op": "select", "table": "mail_messages", "authority": "user",
                "select": ["subject"], "order": [{ "field": "id" }] }),
    )
    .await
    .unwrap();
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["subject"].as_str().unwrap().to_owned())
        .collect()
}

async fn count(host: &TableHost<'_>, table: &str) -> i64 {
    let answer = ask(
        host,
        json!({ "op": "aggregate", "table": table, "authority": "user",
                "aggregate": [{ "alias": "n", "fn": "count" }] }),
    )
    .await
    .unwrap();
    answer["n"].as_i64().unwrap()
}

#[tokio::test]
async fn ownership_joins_through_the_module_tables_for_reads_and_writes() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let report = sync(&cat, MODULE, &mail_tables(), &ModuleTables::new()).await;
    assert!(report.issues.is_empty(), "{:?}", report.issues);

    for (role, name) in [(40, "Staff"), (80, "User")] {
        sc_auth::save_role(&cat, &sc_auth::Role::new(role, name)).await?;
    }
    let staff = sc_auth::create_user(&cat, "staff@example.com", "pw-staff-1", 40).await?;
    let ann = sc_auth::create_user(&cat, "ann@example.com", "pw-ann-1", 80).await?;
    let bob = sc_auth::create_user(&cat, "bob@example.com", "pw-bob-1", 80).await?;

    // What the service writes, with the module's authority: a shared account
    // for staff, and ann's personal one, a message and an attachment in each.
    let admin = TableHost::new(&cat);
    let shared = ask(
        &admin,
        json!({ "op": "insert", "table": "mail_accounts",
                "values": { "label": "orders@", "min_role": 40 } }),
    )
    .await?["id"]
        .clone();
    let anns = ask(
        &admin,
        json!({ "op": "insert", "table": "mail_accounts",
                "values": { "label": "ann@", "owner": ann.id.to_string(), "min_role": 1 } }),
    )
    .await?["id"]
        .clone();
    for (account, subject) in [(&shared, "Order 17"), (&anns, "Dinner?")] {
        let message = ask(
            &admin,
            json!({ "op": "insert", "table": "mail_messages",
                    "values": { "account": account, "subject": subject } }),
        )
        .await?["id"]
            .clone();
        ask(
            &admin,
            json!({ "op": "insert", "table": "mail_attachments",
                    "values": { "message": message, "filename": format!("{subject}.pdf") } }),
        )
        .await?;
    }

    // Reads: one join for the messages, two for the attachments.
    let staff_host = as_user(&cat, &staff);
    let ann_host = as_user(&cat, &ann);
    let bob_host = as_user(&cat, &bob);
    assert_eq!(
        subjects(&staff_host).await,
        ["Order 17"],
        "shared, at min_role"
    );
    assert_eq!(
        subjects(&ann_host).await,
        ["Dinner?"],
        "her own, not the shared one"
    );
    assert!(
        subjects(&bob_host).await.is_empty(),
        "below min_role, owns nothing"
    );
    assert_eq!(count(&staff_host, "mail_attachments").await, 1);
    assert_eq!(count(&ann_host, "mail_attachments").await, 1);
    assert_eq!(count(&bob_host, "mail_attachments").await, 0);
    assert_eq!(
        count(&staff_host, "mail_accounts").await,
        1,
        "reads the shared account"
    );
    assert_eq!(count(&bob_host, "mail_accounts").await, 0);

    // Writes on the account itself: only rows the writer owns.
    ask(
        &ann_host,
        json!({ "op": "insert", "table": "mail_accounts", "authority": "user",
                "values": { "label": "ann-2@", "owner": ann.id.to_string(), "min_role": 1 } }),
    )
    .await?;
    for values in [
        json!({ "label": "shared@", "min_role": 80 }),
        json!({ "label": "bobs@", "owner": bob.id.to_string(), "min_role": 1 }),
    ] {
        let refused = ask(
            &ann_host,
            json!({ "op": "insert", "table": "mail_accounts", "authority": "user",
                    "values": values }),
        )
        .await;
        assert!(refused.is_err(), "ann may not create {values}");
    }
    // Staff may read the shared account but not change it.
    let renamed = ask(
        &staff_host,
        json!({ "op": "update", "table": "mail_accounts", "authority": "user",
                "where": { "id": shared }, "values": { "label": "mine now" } }),
    )
    .await;
    assert!(
        renamed.is_err() || renamed.as_ref().unwrap()["updated"] == json!(0),
        "{renamed:?}"
    );
    // Nor may ann take her account to bob.
    let given = ask(
        &ann_host,
        json!({ "op": "update", "table": "mail_accounts", "authority": "user",
                "where": { "id": anns }, "values": { "owner": bob.id.to_string() } }),
    )
    .await;
    assert!(given.is_err(), "{given:?}");

    // Writes through the join: ann files a message into her account, not into
    // the shared one or into an account she cannot see.
    ask(
        &ann_host,
        json!({ "op": "insert", "table": "mail_messages", "authority": "user",
                "values": { "account": anns, "subject": "Draft" } }),
    )
    .await?;
    let refused = ask(
        &ann_host,
        json!({ "op": "insert", "table": "mail_messages", "authority": "user",
                "values": { "account": shared, "subject": "Sneaky" } }),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");

    // The question §3 leaves to M4.4: a formula grants rows, not fields. Ann
    // may change a flag on her own message — which is what the module's
    // actions re-check and the service overwrites on its next sync.
    let flagged = ask(
        &ann_host,
        json!({ "op": "update", "table": "mail_messages", "authority": "user",
                "where": { "subject": "Dinner?" }, "values": { "seen": true } }),
    )
    .await?;
    assert_eq!(flagged["updated"], json!(1));
    Ok(())
}

// --- M4.5: a declared file field's store ---------------------------------------

#[tokio::test]
async fn a_file_fields_store_is_created_when_missing_and_reused_when_there() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    sc_catalog::bootstrap_file_stores(&cat).await?;

    // An admin made `mail_files` already, somewhere of their choosing.
    let dir = std::env::temp_dir().join(format!("sc-module-mail-files-{}", std::process::id()));
    let mut theirs = sc_module::tables::local_store("mail_files")?;
    theirs
        .config
        .insert("path".into(), json!(dir.display().to_string()));
    sc_catalog::save_file_store(&cat, &theirs).await?;
    sc_catalog::connect_file_store_def(&cat, &theirs)?;

    let tables = decls(json!([{ "name": "mail_attachments", "fields": [
        { "name": "file", "file_store": "mail_files", "file_folder": "/attachments/" },
    ] }]));
    let report = sync(&cat, MODULE, &tables, &ModuleTables::new()).await;
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert!(
        report.stores_created.is_empty(),
        "the admin's store is used"
    );
    let field = cat
        .require("mail_attachments")?
        .field("file")
        .cloned()
        .unwrap();
    assert!(
        matches!(
            &field.kind,
            sc_catalog::DataFieldKind::File { store, folder, .. }
                if store.0 == "mail_files" && folder.as_deref() == Some("attachments")
        ),
        "{:?}",
        field.kind
    );
    let _ = std::fs::remove_dir_all(&dir);

    // None of that name: a local one in the data directory, named after it.
    let def = sc_module::tables::local_store("mail_files_2")?;
    assert_eq!(def.backend, "local");
    assert_eq!(def.config["create"], json!(true));
    assert!(
        def.config["path"]
            .as_str()
            .unwrap()
            .ends_with("mail_files_2"),
        "{:?}",
        def.config
    );
    Ok(())
}

// --- the path a server takes: a package, loaded, twice, then upgraded ----------

#[cfg(feature = "deno-host")]
#[tokio::test]
async fn a_module_installed_twice_changes_nothing_and_an_upgrade_adds_a_field() {
    use crate::common::{fixture, have_npm, surfaces, temp_root};
    use sc_action::ActionRegistry;
    use sc_module::{Installer, Module, ModuleHost, ModuleSet, ModuleSource, save_module};

    skip_without!(have_npm(), "npm is not on the PATH");
    let db = TestDb::new().await.unwrap();
    let cat = catalog(&db).await.unwrap();
    sc_module::bootstrap_modules(&cat).await.unwrap();
    let root = temp_root("declared-tables");
    let installer = Installer::new(&root);
    let host = Arc::new(ModuleHost::new(&root));

    let install = |dir: &'static str| {
        let (installer, cat) = (&installer, &cat);
        async move {
            let package = installer
                .install(ModuleSource::Local, &fixture(dir).display().to_string())
                .await
                .unwrap();
            let mut module = match sc_module::load_module_by_name(cat, &package.name)
                .await
                .unwrap()
            {
                Some(module) => module,
                None => Module::new(&package.name, ModuleSource::Local, dir),
            };
            module.version = Some(package.version);
            save_module(cat, &module).await.unwrap();
        }
    };
    let load = || async {
        ModuleSet::load(
            &cat,
            &host,
            &installer,
            &surfaces(),
            &mut ActionRegistry::new(),
        )
        .await
        .unwrap()
    };

    install("tables-module").await;
    let set = load().await;
    assert!(set.issues().is_empty(), "{:?}", set.issues());
    let loaded = set.get("@saltcorn-test/tables").unwrap();
    assert_eq!(loaded.table_names(), ["tm_notebooks", "tm_notes"]);
    let notes = cat.require("tm_notes").unwrap();
    assert!(notes.ownership.is_some(), "{:?}", notes.ownership_error);
    assert!(set.module_tables().declares_field("tm_notes", "notebook"));

    let row_count = |table: &'static str| {
        let db = &db;
        async move {
            db.client()
                .await
                .unwrap()
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await
                .unwrap()
                .get::<_, i64>(0)
        }
    };
    db.client()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO tm_notebooks (title) VALUES ('Ideas'); \
             INSERT INTO tm_notes (notebook, body) VALUES (1, 'first')",
        )
        .await
        .unwrap();

    // Installed again, the same version: the same tables, the same rows.
    let fields_before = cat.require("tm_notes").unwrap().fields.len();
    install("tables-module").await;
    let set = load().await;
    assert!(set.issues().is_empty(), "{:?}", set.issues());
    assert_eq!(cat.require("tm_notes").unwrap().fields.len(), fields_before);
    assert_eq!(row_count("tm_notes").await, 1);

    // Upgraded: a field, a constraint and a table more, the row kept.
    install("tables-module-v2").await;
    let set = load().await;
    assert!(set.issues().is_empty(), "{:?}", set.issues());
    let notes = cat.require("tm_notes").unwrap();
    assert!(notes.field("pinned").is_some());
    assert!(cat.get("tm_tags").unwrap().is_some());
    assert_eq!(row_count("tm_notes").await, 1);
    assert_eq!(
        set.get("@saltcorn-test/tables").unwrap().table_names(),
        ["tm_notebooks", "tm_notes", "tm_tags"]
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}

/// The account rule lets an owner write their own row: the write path checks
/// the same rule as the read with `_read` false, so `owner === user.id` alone
/// grants it — for a single field, and for the whole row sent back as the GET
/// answered it (the password as its sentinel), through the row layer and the
/// REST API. The field report of 2026-10-22 saw "not found" for such a PUT;
/// this is the case it describes, and it passes.
#[tokio::test]
async fn an_owner_may_update_their_own_account() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = Arc::new(catalog(&db).await?);
    let report = sync(&cat, MODULE, &mail_tables(), &ModuleTables::new()).await;
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    sc_auth::save_role(&cat, &sc_auth::Role::new(80, "User")).await?;
    let ann = sc_auth::create_user(&cat, "ann@example.com", "pw-ann-1", 80).await?;
    let bob = sc_auth::create_user(&cat, "bob@example.com", "pw-bob-1", 80).await?;
    let admin = TableHost::new(&cat);
    let anns = ask(
        &admin,
        json!({ "op": "insert", "table": "mail_accounts",
                "values": { "label": "ann@", "owner": ann.id.to_string(), "min_role": 1,
                            "imap_password": "hunter2" } }),
    )
    .await?["id"]
        .clone();
    let id = anns.to_string();
    #[cfg(feature = "deno-host")]
    sc_expr::set_isolate_prime(sc_module::prime_v8);
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let accounts = cat.require("mail_accounts")?;
    let update = |user: &sc_auth::User, body: Json| {
        let (accounts, evaluator, id, user) = (&accounts, &evaluator, &id, user.clone());
        let cat = &cat;
        async move {
            sc_api::update_row_as(
                cat,
                accounts,
                id,
                &body,
                user.role,
                Some(&user),
                Some(evaluator),
                &[],
                &sc_api::rows::Executor::Pooled,
            )
            .await
        }
    };
    let renamed = update(&ann, json!({ "label": "Ann's mail" })).await?;
    assert_eq!(renamed["label"], json!("Ann's mail"));
    let whole = update(
        &ann,
        json!({ "id": anns, "label": "Ann", "owner": ann.id.to_string(), "min_role": 1,
                "imap_password": "••••••••" }),
    )
    .await?;
    assert_eq!(whole["label"], json!("Ann"));
    assert!(update(&bob, json!({ "label": "Bob's now" })).await.is_err());

    // The same through the REST API, as an application's page saves it.
    let rest = sc_api::RestProvider::project("/api", std::slice::from_ref(&accounts))
        .with_evaluator(evaluator.clone());
    let path = format!("/api/mail_accounts/{id}");
    let listed = rest
        .handle(
            sc_api::ApiRequest::get("/api/mail_accounts"),
            &cat,
            Some(&ann),
        )
        .await?;
    let mut row = listed.body[0].clone();
    row["display_name"] = json!("Ann A.");
    for body in [json!({ "label": "Ann via REST" }), row] {
        let saved = rest
            .handle(
                sc_api::ApiRequest::new(sc_api::Method::Put, &path).body(body.clone()),
                &cat,
                Some(&ann),
            )
            .await?;
        assert_eq!(saved.status, 200, "{body} → {}", saved.body);
    }
    let refused = rest
        .handle(
            sc_api::ApiRequest::new(sc_api::Method::Put, &path).body(json!({ "label": "x" })),
            &cat,
            Some(&bob),
        )
        .await;
    assert!(refused.is_err() || refused.unwrap().status >= 400);
    Ok(())
}
