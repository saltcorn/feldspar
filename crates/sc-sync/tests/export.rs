//! The export of a live installation (S1.2, S1.4, S1.5): deterministic, every
//! kind present, and nothing in APP_SYNC_PROPOSAL §3.4 — rows, users, local
//! paths, keys, instance-only settings, secrets — anywhere in it.

use serde_json::json;

use sc_error::Result;
use sc_types::{Role, SECRET_SENTINEL};

use crate::fixture::{Installation, all_text, code_app, entity, put};

/// A small helpdesk: a table with a row, a user, an application with a view,
/// a trigger, a role, settings of every kind, and one secret of each kind the
/// definitions hold.
pub async fn helpdesk(inst: &Installation) -> Result<()> {
    let catalog = &inst.catalog;
    inst.table("tickets", &["title", "status"]).await?;
    inst.sql("INSERT INTO tickets (id, title, status) VALUES (1, 'ROW-MARKER-7731', 'open')")
        .await?;
    sc_auth::save_role(catalog, &sc_auth::RoleDef::new(Role::of(40), "staff")).await?;
    sc_auth::create_user(
        catalog,
        "someone@example.com",
        "hunter2-PASSWORD",
        Role::of(40),
    )
    .await?;

    let (_, code) = inst.local_store("code", |d| d).await?;
    put(&code, "web/index.ts", b"export const x = 1;\n");
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = inst.application(body).await?;
    let mut view = sc_viewpattern::View::new(app.id, "list_tickets", "List", "tickets");
    view.configuration = json!({ "columns": [{ "type": "Field", "field_name": "title" }] })
        .as_object()
        .cloned()
        .unwrap();
    sc_viewpattern::save_view(catalog, &view).await?;

    // An insert trigger whose action holds an SMTP password.
    let trigger = sc_backup::codec::trigger_from_body(
        sc_action::TriggerId::new(),
        &json!({
            "name": "notify",
            "when": "insert",
            "channel": "tickets",
            "action": "send_email",
            "configuration": {
                "to": "desk@example.com",
                "subject": "New ticket",
                "text": "A ticket was opened.",
                "smtp_host": "relay.example.net",
                "smtp_password": "SMTP-SECRET-1",
            },
        }),
    )?;
    sc_action::save_trigger(catalog, &inst.registries.actions, &trigger).await?;

    // A database connection's password.
    let connection = sc_backup::codec::db_connection_from_body(
        sc_catalog::DbConnectionId::new(),
        &json!({
            "name": "reporting",
            "backend": "postgres",
            "host": "db.example.com",
            "port": 5432,
            "database": "reports",
            "username": "reader",
            "password": "DB-SECRET-2",
        }),
        None,
    )?;
    sc_catalog::save_db_connection(catalog, &connection).await?;

    // A stream's broker password.
    let stream = sc_backup::codec::stream_from_body(
        json!({
            "name": "sensors",
            "provider": "mqtt",
            "configuration": {
                "host": "broker.example.com",
                "topic": "sensors/#",
                "username": "feldspar",
                "password": "STREAM-SECRET-3",
                "payload": "text",
            },
        })
        .as_object()
        .unwrap(),
    )?;
    sc_stream::save_stream(catalog, &inst.registries.streams, &stream).await?;

    // An LLM provider's API key.
    let provider = sc_backup::codec::llm_provider_from_body(
        sc_llm::LlmProviderDefId::new(),
        &json!({
            "name": "claude",
            "backend": "anthropic",
            "config": { "api_key": "LLM-SECRET-4" },
        }),
    )?;
    sc_llm::save_llm_provider(catalog, &provider).await?;

    // An MCP server's header.
    let server = sc_backup::codec::server_from_body(
        sc_mcp::McpServerDefId::new(),
        &json!({
            "name": "tools",
            "url": "https://tools.example.com/mcp",
            "transport": "streamable_http",
            "headers": [{ "name": "Authorization", "value": "Bearer MCP-SECRET-5" }],
        }),
        &Default::default(),
    )?;
    sc_mcp::save_mcp_server(catalog, &server).await?;

    // Settings: two portable sections and two of this instance's own.
    sc_config::set_config(catalog, sc_config::DEFAULT_LOCALE, json!("fr")).await?;
    sc_config::set_config(
        catalog,
        sc_config::MAP_STYLE,
        json!("https://tiles.example.com/s"),
    )
    .await?;
    sc_config::set_config(catalog, sc_config::SMTP_HOST, json!("smtp.example.com")).await?;
    sc_config::set_config(catalog, sc_config::SMTP_PASSWORD, json!("EMAIL-SECRET-6")).await?;
    Ok(())
}

#[tokio::test]
async fn exporting_twice_gives_the_same_tree() -> Result<()> {
    let inst = Installation::new().await?;
    helpdesk(&inst).await?;
    let first = inst.tree().await?;
    let second = inst.tree().await?;
    assert_eq!(first.to_files(), second.to_files());
    assert_eq!(first.digest(), second.digest());

    // Every kind the fixture made is there, at the path §3.1 names.
    for path in [
        "feldspar-structure.json",
        "roles.json",
        "modules.json",
        "users.json",
        "settings/localisation.json",
        "settings/maps.json",
        "tables/tickets.json",
        "db-connections/reporting.json",
        "file-stores/code.json",
        "file-stores/code/files/web/index.ts",
        "triggers/notify.json",
        "streams/sensors.json",
        "llm-providers/claude.json",
        "mcp-servers/tools.json",
        "applications/helpdesk/application.json",
        "applications/helpdesk/views/list_tickets.json",
    ] {
        assert!(
            first.get(path).is_some(),
            "{path} missing from {:?}",
            first.paths().collect::<Vec<_>>()
        );
    }

    // A table carries its id, its settings and its fields in column order,
    // each with its own id — and none of what the screen works out on a read.
    let tickets = entity(&first, "tables/tickets.json");
    assert!(tickets["id"].is_string(), "{tickets}");
    let names: Vec<&str> = tickets["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["id", "title", "status"]);
    assert!(
        tickets["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["id"].is_string())
    );
    for derived in [
        "configured",
        "rls_available",
        "sync_available",
        "module_fields",
    ] {
        assert!(tickets.get(derived).is_none(), "{derived} in {tickets}");
    }

    // Bookkeeping is left out of the application and the trigger.
    let app = entity(&first, "applications/helpdesk/application.json");
    for derived in ["targets", "builds", "source", "has_views"] {
        assert!(app.get(derived).is_none(), "{derived} in {app}");
    }
    let trigger = entity(&first, "triggers/notify.json");
    for kept_out in [
        "last_run_at",
        "error",
        "system_approval",
        "workflow_version",
    ] {
        assert!(trigger.get(kept_out).is_none(), "{kept_out} in {trigger}");
    }
    Ok(())
}

#[tokio::test]
async fn nothing_instance_bound_is_exported() -> Result<()> {
    let inst = Installation::new().await?;
    helpdesk(&inst).await?;
    let tree = inst.tree().await?;
    let text = all_text(&tree);

    // No rows, no accounts, no password hashes.
    assert!(!text.contains("ROW-MARKER-7731"), "a row was exported");
    assert!(!text.contains("someone@example.com"), "a user was exported");
    assert!(!text.contains("password_hash"), "{text}");
    // No local store directory.
    assert!(
        !text.contains(&inst.dir.to_string_lossy().into_owned()),
        "a path was exported"
    );
    assert!(
        entity(&tree, "file-stores/code.json")["config"]
            .get("path")
            .is_none()
    );
    // No settings section that is this instance's own.
    for section in ["ssl", "email", "development", "authentication"] {
        assert!(
            tree.get(&format!("settings/{section}.json")).is_none(),
            "{section}"
        );
    }
    assert!(
        !text.contains("smtp.example.com"),
        "an email setting was exported"
    );
    // The users table's system columns are the system's.
    let users = entity(&tree, "users.json");
    assert_eq!(users["fields"], json!([]), "{users}");
    Ok(())
}

#[tokio::test]
async fn no_secret_is_exported() -> Result<()> {
    let inst = Installation::new().await?;
    helpdesk(&inst).await?;
    let tree = inst.tree().await?;
    let text = all_text(&tree);
    for secret in [
        "SMTP-SECRET-1",
        "DB-SECRET-2",
        "STREAM-SECRET-3",
        "LLM-SECRET-4",
        "MCP-SECRET-5",
        "EMAIL-SECRET-6",
    ] {
        assert!(!text.contains(secret), "{secret} was exported:\n{text}");
    }
    // Each is the sentinel, where the definition holds it.
    assert_eq!(
        entity(&tree, "db-connections/reporting.json")["password"],
        json!(SECRET_SENTINEL)
    );
    assert_eq!(
        entity(&tree, "streams/sensors.json")["configuration"]["password"],
        json!(SECRET_SENTINEL)
    );
    // The stream's other settings are not secret, and are kept.
    assert_eq!(
        entity(&tree, "streams/sensors.json")["configuration"]["host"],
        json!("broker.example.com")
    );
    assert_eq!(
        entity(&tree, "llm-providers/claude.json")["config"]["api_key"],
        json!(SECRET_SENTINEL)
    );
    assert_eq!(
        entity(&tree, "mcp-servers/tools.json")["headers"][0]["value"],
        json!(SECRET_SENTINEL)
    );
    let trigger = entity(&tree, "triggers/notify.json");
    assert_eq!(
        trigger["configuration"]["smtp_password"],
        json!(SECRET_SENTINEL)
    );
    assert_eq!(trigger["configuration"]["subject"], json!("New ticket"));
    Ok(())
}

#[tokio::test]
async fn only_portable_settings_are_exported() -> Result<()> {
    let inst = Installation::new().await?;
    helpdesk(&inst).await?;
    let tree = inst.tree().await?;
    let sections: Vec<&str> = tree
        .paths()
        .filter_map(|p| p.strip_prefix("settings/"))
        .collect();
    assert_eq!(sections, ["localisation.json", "maps.json"]);
    // What is stored, and only that: not the declared default of the enabled
    // languages, which the receiving build supplies itself.
    assert_eq!(
        entity(&tree, "settings/localisation.json"),
        &json!({ "default_locale": "fr" })
    );
    assert_eq!(
        entity(&tree, "settings/maps.json"),
        &json!({ "map_style": "https://tiles.example.com/s" })
    );
    Ok(())
}
