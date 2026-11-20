//! A generated client's filter may be a list of comparisons on one column.
//!
//! The server has always read `?published=gte.…&published=lt.…` as a range,
//! but the client's `Filters` held one string per column, so a date range read
//! through it had to fetch more and filter in the browser. A list under one key
//! now sends one pair per element, and a single string still sends one pair.
//!
//! This **runs** the generated module under Node with a stubbed `fetch`, like
//! `generated_client_csrf.rs`. Skips when `node` is absent.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (the server binary, git, npm, a browser) directly"
)]
use std::process::Command;

use sc_api::{ApiProvider, RestProvider};
use sc_catalog::{AccessRules, DataField, DbId, Table, TableSource};
use sc_types::{BasicType, TypeRef};

const DRIVER_TS: &str = r#"
import { createClient } from "./client.ts";

const urls: string[] = [];
(globalThis as any).fetch = async (url: string, _init: any) => {
  urls.push(String(url));
  return new Response("[]", { status: 200, headers: { "content-type": "application/json" } });
};

const api = createClient();
await api.posts.list({
  filter: { published: ["gte.2020-01-01", "lt.2021-01-01"], status: "in.(open,pending)" },
});
await api.posts.list({ filter: { status: "eq.open", published: undefined } });
console.log(JSON.stringify(urls));
"#;

fn posts() -> Table {
    Table {
        name: "posts".to_owned(),
        database: DbId::primary(),
        source: TableSource::Database,
        fields: vec![
            DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key(),
            DataField::plain("status", TypeRef::Basic(BasicType::Text)),
            DataField::plain("published", TypeRef::Basic(BasicType::Date)),
        ],
        primary_key: vec!["id".to_owned()],
        label: "posts".to_owned(),
        description: String::new(),
        access: AccessRules::default(),
        attributes: Default::default(),
        overlay: None,
        ownership: None,
        ownership_error: None,
        rls_enabled: false,
        constraints: Vec::new(),
    }
}

#[test]
fn a_list_of_comparisons_on_one_column_is_sent_as_the_column_repeated() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    let provider = RestProvider::project("/api", &[posts()]);
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));

    let dir = std::env::temp_dir().join(format!("sc-api-client-filters-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("client.ts"),
        client_ts.replace("from \"./helper\"", "from \"./helper.ts\""),
    )?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER_TS)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running the generated client failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let urls: Vec<String> =
        serde_json::from_slice(&output.stdout).expect("the driver prints the URLs it was asked");
    let query = |url: &str| {
        url.split_once('?')
            .map(|(_, q)| q.to_owned())
            .unwrap_or_default()
    };
    assert_eq!(
        query(&urls[0]),
        "published=gte.2020-01-01&published=lt.2021-01-01&status=in.%28open%2Cpending%29",
        "{urls:?}"
    );
    // A single comparison is one pair, and an unset column sends nothing.
    assert_eq!(query(&urls[1]), "status=eq.open", "{urls:?}");
    Ok(())
}
