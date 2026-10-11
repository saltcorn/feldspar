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
/// page can show. And the two topic kinds (TODO.md "Live updates" L2.10): a
/// per-user stream is subscribed naming nothing, and a per-row one cannot be
/// subscribed without the row's key.
const STREAM_USAGE_TS: &str = r#"
import {
  createClient,
  LiveConnection,
  type BoardActivityEnvelope,
  type BoilerEnvelope,
  type JobStatusEnvelope,
  type LiveError,
  type LiveMember,
  type LiveRowStream,
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

  // A per-user stream: the subscriber's own topic, never named.
  api.live.job_status.subscribe({
    element(envelope: JobStatusEnvelope) {
      const message: string = envelope.value.message;
      const progress: number | null = envelope.value.progress;
      void message; void progress;
    },
  });
  // A per-row stream: subscribed naming the row, by number or by text, or as
  // one row's own stream — the same object for the same key.
  const board: LiveRowStream<BoardActivityEnvelope> = api.live.board_activity;
  board.subscribe(7, { element: (e) => void e.value.message }).close();
  board.subscribe("7", { element: (e) => void e.value.message }).close();
  const seven: LiveStream<BoardActivityEnvelope> = board.topic(7);
  seven.subscribe({ element: (e) => void e.value.message }).close();

  // @ts-expect-error a per-row stream cannot be subscribed without naming the row
  api.live.board_activity.subscribe({ element: () => {} });
  // @ts-expect-error a per-user stream's topic is the subscriber's own, never named
  api.live.job_status.subscribe(7, { element: () => {} });
  // @ts-expect-error `pressure` is not a declared key of this element type
  api.live.boiler.subscribe({ element: (e) => void e.value.pressure });
  // @ts-expect-error a text element's value is a string, not an object
  api.live.syslog.subscribe({ element: (e) => void e.value.line });
  // @ts-expect-error a stream the application does not expose has no accessor
  void api.live.meter;

  // L4.4: a stream with presence is joined; one pages may publish on takes a
  // value of its element type.
  const room = api.live.board_presence.join(7, {
    presence(members: LiveMember[]) {
      const first: string | null = members[0].user_id;
      void first;
    },
  });
  const members: LiveMember[] = room.members;
  const me: string | undefined = room.me;
  const allowed: boolean = room.canPublish;
  room.setState({ dragging: 3 });
  room.publish({ message: "moved", progress: null });
  room.leave();
  void members; void me; void allowed;
  api.live.board_presence.publish(7, { message: "moved", progress: 0.5 });
  // @ts-expect-error `nope` is not a declared key of the element type
  api.live.board_presence.publish(7, { nope: 1 });
  // @ts-expect-error a per-row stream is joined naming the row
  api.live.board_presence.join({});
  // @ts-expect-error a stream without presence cannot be joined
  api.live.board_activity.join(7);
  // @ts-expect-error a stream pages may not publish on has no `publish`
  api.live.board_activity.publish(7, { message: "x", progress: null });
  // @ts-expect-error a per-user stream never takes a page's publish, whatever its settings
  api.live.job_status.publish({ message: "x", progress: null });
}
"#;

#[test]
fn generated_stream_client_type_checks() -> std::io::Result<()> {
    use sc_api::{StreamExport, StreamTopics, StructField, TypeSchema, ValueType};

    let message = || {
        TypeSchema::struct_of([
            StructField::new("message", TypeSchema::value(ValueType::Text)),
            StructField::new(
                "progress",
                TypeSchema::optional(TypeSchema::value(ValueType::Float)),
            ),
        ])
    };
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
            topics: StreamTopics::Single,
            presence: false,
            client_publish: false,
        },
        StreamExport {
            name: "syslog".to_owned(),
            path: "/api/live".to_owned(),
            value: TypeSchema::text(),
            topics: StreamTopics::Single,
            presence: false,
            client_publish: false,
        },
        StreamExport {
            name: "frames".to_owned(),
            path: "/api/live".to_owned(),
            // Bytes travel as base64, which is a string on the wire.
            value: TypeSchema::value(ValueType::Bytes),
            topics: StreamTopics::Single,
            presence: false,
            client_publish: false,
        },
        StreamExport {
            name: "job_status".to_owned(),
            path: "/api/live".to_owned(),
            value: message(),
            topics: StreamTopics::User,
            presence: false,
            // Never offered on a user topic, whatever the settings say.
            client_publish: true,
        },
        StreamExport {
            name: "board_activity".to_owned(),
            path: "/api/live".to_owned(),
            value: message(),
            topics: StreamTopics::Row,
            presence: false,
            client_publish: false,
        },
        StreamExport {
            name: "board_presence".to_owned(),
            path: "/api/live".to_owned(),
            value: message(),
            topics: StreamTopics::Row,
            presence: true,
            client_publish: true,
        },
    ];
    // An application with no API provider still exposes streams: the client is
    // sockets and nothing else, and it has to compile.
    let client_ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams);
    type_check("streams", &client_ts, STREAM_USAGE_TS)
}

/// A usage module for an application that reads `posts` and exposes its
/// changes (TODO.md "Live updates" L3.10–L3.11): the subscription's filter is
/// the read's `Filters`, a change's row is the read's row, the table's object
/// carries the stream as `changes`, and a `useLiveRows`-shaped function infers
/// its rows from the `select` the way `list` does.
const CHANGES_USAGE_TS: &str = r#"
import {
  createClient,
  LiveRows,
  type LiveChangesStream,
  type LiveRowsOptions,
  type LiveTable,
  type PostsLiveEnvelope,
  type PostsRow,
  type PostsSchema,
  type Selected,
  type TableSchema,
} from "./client";

// `useLiveRows`'s signature (the scaffold's `live-react.ts`), without React.
declare function useLiveRows<Sch extends TableSchema, Key, S extends string = "">(
  table: LiveTable<Sch, Key>,
  options?: LiveRowsOptions<Sch["row"], S>,
): { rows: Array<Selected<Sch, S>> };

export function exercise(): void {
  const api = createClient({ baseUrl: "https://blog.example.com" });

  const stream: LiveChangesStream<PostsSchema, number> = api.live.posts_live;
  const same: LiveChangesStream<PostsSchema, number> = api.posts.changes;
  void same;
  stream
    .subscribe(
      { where: { author: "eq.7", published: "is_null.false" } },
      {
        element(envelope: PostsLiveEnvelope) {
          const change = envelope.value;
          const key: number = change.key;
          const row: PostsRow | undefined = change.row;
          if (change.op === "leave" || change.op === "delete") return void key;
          const title: string | undefined = change.row?.title;
          void row; void title;
        },
      },
    )
    .close();

  const all: PostsRow[] = useLiveRows(api.posts, { where: { author: "eq.7" }, orderBy: "published.desc" }).rows;
  const titles = useLiveRows(api.posts, { select: "title,author(name)" }).rows;
  const title: string = titles[0].title;
  const name: string = titles[0].author!.name;
  const kept = new LiveRows<PostsRow>(api.posts as unknown as LiveTable);
  kept.close();

  // @ts-expect-error `nope` is not a column of posts
  stream.subscribe({ where: { nope: "eq.1" } }, { element() {} });
  // @ts-expect-error a filter value names a comparison, as in a read
  useLiveRows(api.posts, { where: { title: "hello" } });
  // @ts-expect-error a selected row has only what was selected
  void useLiveRows(api.posts, { select: "id" }).rows[0].title;
  // @ts-expect-error a row stream has no topic to name
  stream.subscribe(7, { element() {} });
  // @ts-expect-error `authors` exposes no changes, so it is not a live table
  useLiveRows(api.authors);

  void all; void title; void name;
}
"#;

#[test]
fn generated_changes_client_type_checks() -> std::io::Result<()> {
    use sc_api::{StreamExport, StreamTopics, TypeSchema};

    let provider = RestProvider::project("/api", &blog_tables());
    let streams = [StreamExport {
        name: "posts_live".to_owned(),
        path: "/api/live".to_owned(),
        value: TypeSchema::json(),
        topics: StreamTopics::Changes {
            table: "posts".to_owned(),
        },
        presence: false,
        client_publish: false,
    }];
    let client_ts =
        sc_api::generate_client_with_streams(ApiProvider::endpoints(&provider), &streams);
    type_check("changes", &client_ts, CHANGES_USAGE_TS)
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
