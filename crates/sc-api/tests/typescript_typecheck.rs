//! The generated TypeScript client must type-check against the declared
//! endpoints (design §13.1; Phase 6 "typed client" tasks).
//!
//! This exercises the full contract for both API surfaces the crate generates a
//! client for: the fixed admin endpoint set, and a [`sc_api::RestProvider`]
//! projection of an application's tables. Each generates the client, writes it
//! next to a strict-mode usage file that
//! calls a representative set of endpoints with correctly-typed arguments, and
//! runs the TypeScript compiler in `--noEmit --strict` mode. A drift between a
//! declared endpoint and the generated call (wrong arg shape, wrong return type)
//! is a `tsc` error, so a green run proves the client is genuinely typed rather
//! than `any`.
//!
//! `tsc` is not always present, so the test resolves a compiler from (in order)
//! the `SC_TSC` env var or a `node_modules/.bin/tsc` under the crate, and
//! **skips** (rather than fails) when none is available — mirroring how the
//! DB-backed tests gate on a configured database. Run it locally with, e.g.:
//!
//! ```text
//! npm install typescript@5 --no-save --prefix crates/sc-api
//! cargo test -p sc-api --test typescript_typecheck
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_api::{ApiProvider, RestProvider};
use sc_catalog::{
    AccessRules, DataField, DataFieldKind, DbId, FieldId, FileStoreId, Table, TableId, TableSource,
};
use sc_db::ColumnGenerator;
use sc_types::{BasicType, TypeRef};

/// A usage module that calls a cross-section of the admin endpoints with
/// correctly-typed arguments and consumes their typed results. It fails to
/// compile if the generated client drifts from the declared endpoints.
const USAGE_TS: &str = r#"
import { createClient, LoginResponse } from "./client";

export async function exercise(): Promise<void> {
  const api = createClient({ baseUrl: "http://localhost:3032" });

  // No-body GET returning a struct with a nested optional.
  const status = await api.authStatus();
  const exists: boolean = status.any_user_exists;

  // POST with a typed body and typed response.
  const user: LoginResponse = await api.login({ email: "a@b.c", password: "pw" });
  const role: number = user.role;

  // Path-parameter endpoints.
  const rows = await api.listRows("mytable");
  await api.updateRow("mytable", "some-id", { any: "json" });
  await api.deleteRow("mytable", "some-id");

  void exists; void role; void rows;
}
"#;

/// A usage module for an application's REST client. The app declares two
/// tables — `posts`, with a key into `authors` and a file column — so the
/// generated client must hang one object off each, type its rows from its own
/// columns, and read the shape of a `select` out of the string it was written
/// with. Every `@ts-expect-error` below is an assertion in the other direction:
/// the line *must* fail to compile, or this test fails.
const APP_USAGE_TS: &str = r#"
import { createClient, type PostsRow, type AuthorsRow, type PostsQuery } from "./client";

export async function exercise(): Promise<void> {
  const api = createClient({ baseUrl: "https://blog.example.com" });

  // A table is one object with methods, not four loose methods.
  const posts: PostsRow[] = await api.posts.list();
  const title: string = posts[0].title;
  // A nullable column is nullable; a `NOT NULL` one is not.
  const published: string | null = posts[0].published;

  // A read's query string is typed against this table's own columns (§13.4).
  const query: PostsQuery<"title"> = {
    select: "title",
    order: "published.desc",
    limit: 20,
    filter: { published: "gte.2020-01-01" },
  };
  const titles = await api.posts.list(query);
  const justTitle: string = titles[0].title;

  // The `select` decides the shape of the answer, and the compiler reads that
  // shape out of the string: embeds nest, aliases rename, and a key that may be
  // null answers a row that may be null.
  const embedded = await api.posts.list({ select: "id,author(name,country)" });
  const authorName: string = embedded[0].author!.name;
  const aliased = await api.posts.list({ select: "who:author(label:name)" });
  const label: string | undefined = aliased[0].who?.label;

  // One row by key, and the same select typing on it. Without a select it is
  // the whole row; with one it is what the select asked for.
  const whole: PostsRow | undefined = await api.posts.get(1);
  const one = await api.posts.get(1, { select: "title" });
  const oneTitle: string | undefined = one?.title;

  // Writes are typed from the columns too: a required column with no default
  // must be given, the key that numbers itself must not.
  const created = await api.posts.create({ title: "hello", author: 1 });
  const updated = await api.posts.update(created.id, { title: "goodbye" });
  const gone = await api.posts.delete(updated.id);
  const wasDeleted: true = gone.deleted;

  // A file column carries its own two calls.
  const cover = await api.posts.cover.download(1);
  await api.posts.cover.upload(1, "cover.png", cover);

  // The second table is its own object, with its own row type.
  const authors: AuthorsRow[] = await api.authors.list();

  // A select assembled at runtime cannot be read by the compiler, so the answer
  // degrades to the whole row rather than to a lie about it.
  const dynamic: string = (await api.posts.list({ select: String(1) }))[0].title;

  // @ts-expect-error `nope` is not a column of posts, so it cannot be filtered on
  await api.posts.list({ filter: { nope: "eq.1" } });
  // @ts-expect-error a filter value names a comparison
  await api.posts.list({ filter: { title: "hello" } });
  // @ts-expect-error `sideways` is not an ordering direction
  await api.posts.list({ order: "title.sideways" });
  // @ts-expect-error a selected row has only what was selected
  void (await api.posts.list({ select: "id" }))[0].title;
  // @ts-expect-error the key is a number, and a string is not one
  await api.posts.delete("1");
  // @ts-expect-error a required column with no default is not optional
  await api.posts.create({ author: 1 });
  // @ts-expect-error a calculated column is read-only
  await api.posts.update(1, { word_count: 3 });

  void published; void justTitle; void authorName; void label; void oneTitle;
  void wasDeleted; void authors; void dynamic; void whole;
}
"#;

/// A usage module for an endpoint set that declares query parameters. It proves
/// the three shapes are genuinely typed: the options object is omissible when
/// every parameter is optional, required when one is not, a repeated parameter
/// is an array, and each parameter carries the type it was declared with.
const QUERY_USAGE_TS: &str = r#"
import { createClient, ListBooksQuery } from "./client";

export async function exercise(): Promise<void> {
  const api = createClient({});

  // All-optional parameters: the argument itself may be left out.
  await api.listBooks();
  const query: ListBooksQuery = {
    select: "title,author(name)",
    limit: 20,
    published: ["gte.2020-01-01", "lt.2024-01-01"],
  };
  const books = await api.listBooks(query);

  // A required parameter makes the object required, and it is typed.
  const found = await api.searchBooks({ q: "dune" });

  void books; void found;
}
"#;

/// A usage module for an application that exposes three streams — one of each
/// [`ElementType`](sc_stream::ElementType) shape (TODO "Streams" §10; TODO.md
/// "Live updates" L1.9).
///
/// What it asserts is the whole point of typing a subscription from the element
/// type: a declared key arrives as the type it was declared with, a text stream
/// is a `string`, a binary one is base64 in a `string`, and a key nobody
/// declared is a compile error rather than an `undefined` at three in the
/// morning. And that every stream shares one `LiveConnection`, whose status a
/// page can show.
const STREAM_USAGE_TS: &str = r#"
import {
  createClient,
  LiveConnection,
  type BoilerEnvelope,
  type LiveError,
  type LiveStatus,
  type LiveStream,
  type LiveSubscription,
} from "./client";

export function exercise(): void {
  const api = createClient({ baseUrl: "https://blog.example.com", live: { minDelay: 100 } });

  // A `json` element: the declared keys, each typed, `value` an object.
  const boiler: LiveSubscription = api.live.boiler.subscribe({
    ready(info) {
      const replayed: number = info.replayed;
      void replayed;
    },
    element(envelope: BoilerEnvelope) {
      const temperature: number = envelope.value.temperature;
      const label: string | null = envelope.value.label;
      const seen: string = envelope.received_at;
      const topic: string | undefined = envelope.topic;
      void temperature; void label; void seen; void topic;
    },
    // §7 reaching the client: a consumer that cannot keep up is told.
    lagged(dropped: number) {
      void dropped;
    },
    // A reconnect: what was built from earlier elements may be stale.
    resync() {},
    error(error: LiveError) {
      const code: string = error.code;
      void code;
    },
  });
  boiler.close();

  // A `text` element is a string, and a `binary` one is base64 in a string.
  api.live.syslog.subscribe({ element: (e) => { const line: string = e.value; void line; } });
  api.live.frames.subscribe({ element: (e) => { const b64: string = e.value; void b64; } });

  // Every accessor is over the one connection, and it says where it is.
  const accessor: LiveStream<BoilerEnvelope> = api.live.boiler;
  const connection: LiveConnection = accessor.connection;
  const status: LiveStatus = connection.status;
  const stop = connection.onStatus((next: LiveStatus) => void next);
  stop();
  void status;

  // @ts-expect-error `pressure` is not a declared key of this element type
  api.live.boiler.subscribe({ element: (e) => void e.value.pressure });
  // @ts-expect-error a text element's value is a string, not an object
  api.live.syslog.subscribe({ element: (e) => void e.value.line });
  // @ts-expect-error a stream the application does not expose has no accessor
  void api.live.meter;
}
"#;

#[test]
fn generated_stream_client_type_checks() -> std::io::Result<()> {
    use sc_api::{StreamExport, StructField, TypeSchema, ValueType};

    let streams = vec![
        StreamExport {
            name: "boiler".to_owned(),
            path: "/api/live".to_owned(),
            value: TypeSchema::struct_of([
                // Required: the key is always there, so it is not nullable.
                StructField::new("temperature", TypeSchema::value(ValueType::Float)),
                // Optional: a declared key that is absent is `null` (§4).
                StructField::new(
                    "label",
                    TypeSchema::optional(TypeSchema::value(ValueType::Text)),
                ),
            ]),
        },
        StreamExport {
            name: "syslog".to_owned(),
            path: "/api/live".to_owned(),
            value: TypeSchema::text(),
        },
        StreamExport {
            name: "frames".to_owned(),
            path: "/api/live".to_owned(),
            // Bytes travel as base64, which is a string on the wire.
            value: TypeSchema::value(ValueType::Bytes),
        },
    ];
    // An application with no API provider still exposes streams: the client is
    // sockets and nothing else, and it has to compile.
    let client_ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams);
    type_check("streams", &client_ts, STREAM_USAGE_TS)
}

#[test]
fn generated_query_parameter_client_type_checks() -> std::io::Result<()> {
    use sc_api::{Endpoint, EndpointSet, Method, PathSpec, QueryParam, TypeSchema, ValueType};

    let set = EndpointSet::new()
        .with(
            Endpoint::new("listBooks", Method::Get, PathSpec::root().lit("api/books"))
                .query([
                    QueryParam::new("select", ValueType::Text),
                    QueryParam::new("limit", ValueType::Int),
                    QueryParam::new("published", ValueType::Text).repeated(),
                ])
                .output(TypeSchema::array(TypeSchema::json())),
        )
        .with(
            Endpoint::new(
                "searchBooks",
                Method::Get,
                PathSpec::root().lit("api/search"),
            )
            .query([
                QueryParam::new("q", ValueType::Text).required(),
                QueryParam::new("limit", ValueType::Int),
            ])
            .output(TypeSchema::array(TypeSchema::json())),
        );
    let client_ts = sc_api::generate_client(&set);
    type_check("query-params", &client_ts, QUERY_USAGE_TS)
}

#[test]
fn generated_admin_client_type_checks() -> std::io::Result<()> {
    let client_ts = sc_api::generate_client(&sc_api::admin_endpoints());
    type_check("admin", &client_ts, USAGE_TS)
}

#[test]
fn generated_app_rest_client_type_checks() -> std::io::Result<()> {
    // A projection needs only `Table` values, so this stays a unit-speed test
    // with no database: the client an app gets is a pure function of its tables.
    let provider = RestProvider::project("/api", &blog_tables());
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));
    type_check("app-rest", &client_ts, APP_USAGE_TS)
}

/// A two-table blog: `posts` keyed into `authors`, with a required column, a
/// nullable one, a calculated one and a file column — one of everything the
/// generated row types have to distinguish between.
fn blog_tables() -> Vec<Table> {
    let mut author = DataField::plain("author", TypeRef::Basic(BasicType::Int)).required();
    author.kind = DataFieldKind::Key {
        target_table: TableId("authors".to_owned()),
        target_field: FieldId("id".to_owned()),
        summary_field: None,
    };
    let mut cover = DataField::plain("cover", TypeRef::Basic(BasicType::Text));
    cover.kind = DataFieldKind::File {
        store: FileStoreId("uploads".to_owned()),
        folder: None,
        mime_allow: Vec::new(),
    };
    let mut word_count = DataField::plain("word_count", TypeRef::Basic(BasicType::Int));
    word_count.kind = DataFieldKind::Calc {
        expression: "1".to_owned(),
    };

    vec![
        table(
            "posts",
            vec![
                key_column("id"),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
                DataField::plain("published", TypeRef::Basic(BasicType::Date)),
                author,
                cover,
                word_count,
            ],
        ),
        table(
            "authors",
            vec![
                key_column("id"),
                DataField::plain("name", TypeRef::Basic(BasicType::Text)).required(),
                DataField::plain("country", TypeRef::Basic(BasicType::Text)),
            ],
        ),
    ]
}

/// An identity primary key: `NOT NULL`, but the database fills it in, so a
/// generated insert type must leave it out rather than demand it.
fn key_column(name: &str) -> DataField {
    DataField::plain(name, TypeRef::Basic(BasicType::Int))
        .primary_key()
        .required()
        .generated(ColumnGenerator::Identity)
}

fn table(name: &str, fields: Vec<DataField>) -> Table {
    Table {
        id: TableId(name.to_owned()),
        name: name.to_owned(),
        database: DbId::primary(),
        source: TableSource::Database,
        fields,
        primary_key: vec!["id".to_owned()],
        label: name.to_owned(),
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

/// Type-check `client_ts` against `usage_ts` with `tsc --noEmit --strict`,
/// skipping when no compiler is available.
fn type_check(tag: &str, client_ts: &str, usage_ts: &str) -> std::io::Result<()> {
    let Some(tsc) = resolve_tsc() else {
        eprintln!(
            "skipping: no TypeScript compiler found. Set SC_TSC=/path/to/tsc or run \
             `npm install typescript@5 --no-save --prefix crates/sc-api`."
        );
        return Ok(());
    };

    let dir = std::env::temp_dir().join(format!("sc-api-tsc-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), client_ts)?;
    // The generic half of the client, which the client imports: every emitter
    // writes the pair, so type-checking one without the other would be checking
    // something nobody ships.
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("usage.ts"), usage_ts)?;

    let output = Command::new(&tsc)
        .args([
            "--noEmit",
            "--strict",
            "--lib",
            "es2020,dom",
            "--moduleResolution",
            "node",
            "--module",
            "esnext",
        ])
        .arg(dir.join("client.ts"))
        .arg(dir.join(sc_api::CLIENT_HELPER_FILE))
        .arg(dir.join("usage.ts"))
        .output()
        .unwrap_or_else(|e| panic!("failed to run tsc at {}: {e}", tsc.display()));

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "generated {tag} TypeScript failed to type-check:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- client.ts ---\n{client_ts}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Resolve a TypeScript compiler: `SC_TSC`, else a `node_modules/.bin/tsc`
/// installed under the crate. Returns `None` when neither is present.
fn resolve_tsc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SC_TSC") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("node_modules/.bin/tsc");
    local.exists().then_some(local)
}
