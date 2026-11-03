# Review D — injection and identifier handling (read-only, 2026-11-03)

Scope: SQL identifiers and values, formula → SQL/JS, HTML/URL/header contexts, deserialization depth.
Method: reading only (grep/sed/git). Nothing was built or run, so "verified" below means
"confirmed by reading the code path end to end". It does not mean a reproduction was run.
Items already in docs/SECURITY_REVIEW_2026-10.md are referenced, not repeated.

Overall: **no SQL injection found.** Every value is bound. Every identifier on the AST path goes
through `SqlDialect::quote_ident`. The 17 `TrustedSql::composed` sites only interpolate constants
or names that are already quoted. Three weaknesses stand out. The safety comes from
**convention, not types**: there is no identifier type, about 550 `&str` table/column
parameters, 7 hand-written quoters, and a raw-SQL path (`SchemaStep::Sql`, `tx.batch`) that
`TrustedSql` does not cover. The one runtime risk found is **unbounded recursion over the formula
AST**, which row data can reach.

---

## Findings

### D1. Medium — formula nesting depth is unbounded; row data can reach it (stack overflow aborts the process)

*Status: reported. The code path is verified by reading; the crash itself was not reproduced.*

Evidence:
- `crates/sc-expr/src/formula.rs:37-77` `Formula::parse` has no limit on source length or nesting depth.
  swc 42 (`Cargo.lock:11159`) uses `stacker`, so the **parser** grows its stack. Everything after
  it is plain recursion:
  - `crates/sc-expr/src/ast.rs:176-200` `lower`: `Paren`, `Bin`, `Unary` and `Cond` each recurse once per level.
  - `crates/sc-expr/src/analyze.rs:195` `walk` (`collect_free_vars`), `translate.rs`, `eval.rs`,
    `normalise.rs`: each walks the tree recursively.
  - The automatic `Drop` of the swc `Box<Expr>` chain and of the `Ast` is also recursive.
- Row data reaches the parser through the v1 re-interpolation path:
  `crates/sc-expr/src/template.rs:436-447` → `Template::parse(&text)` → `token()` (`:376-386`) →
  `Formula::parse(expr)`. `MAX_PASSES` limits how many times a value re-renders. It does not limit
  how deep one formula is.
- A second route: code-body `where: { formula: "<string>" }` (`crates/sc-api/src/code_host/plan.rs:1236-1241`)
  parses any string the code body forwards. That is often request input.

Attack scenario: an admin template contains `{{= note }}`, and `note` is a field any user can
write. A user stores `{{ ((((…200 000×…1…)))) }}`, or a 100 000-term `1+1+…`, which is about
200–400 KB and under the JSON body limit. The next render (email, page) recurses through `lower`
or `Drop` deeper than the 8 MiB worker stack. Rust **aborts the whole process** on stack overflow,
not just the one request, so every application on the node goes down. A restart re-renders the
same row (a scheduled email), which can turn this into a crash loop.

Type-system fix: a parse-once `BoundedFormula`/`Depth` budget. Check depth iteratively **before**
lowering, and give the AST an iterative `Drop`.

```rust
pub const MAX_FORMULA_BYTES: usize = 16 * 1024;
pub const MAX_FORMULA_DEPTH: u16 = 256;

/// Proof that a tree is no deeper than MAX_FORMULA_DEPTH. Only `Depth::check` makes one.
#[derive(Clone, Copy)] pub struct Depth(u16);
impl Depth {
    fn check(e: &swc::Expr) -> Result<Depth> {           // iterative, explicit stack
        let mut stack = vec![(e, 1u16)]; let mut max = 0;
        while let Some((e, d)) = stack.pop() {
            if d > MAX_FORMULA_DEPTH { return Err(Error::invalid("formula nested too deeply")); }
            max = max.max(d);
            for c in swc_children(e) { stack.push((c, d + 1)); }
        }
        Ok(Depth(max))
    }
}
impl Formula {
    pub fn parse(source: &str) -> Result<Formula> {
        if source.len() > MAX_FORMULA_BYTES { return Err(Error::invalid("formula too long")); }
        let expr = parse_swc(source)?;
        let _d = Depth::check(&expr).inspect_err(|_| std::mem::forget(expr))?; // never drop a deep tree recursively
        let ast = lower(&expr)?;                          // now provably ≤256 frames
        …
    }
}
```

Also wrap `lower` and the walkers in `stacker::maybe_grow` as defence in depth. Add a test that
parses 100k nested parens and expects an error, not an abort.

### D2. Low–Medium (structural) — `TrustedSql` covers `Statement::Raw` only; DDL scripts are a second raw-SQL path typed `String`

*Status: verified (by reading).*

- `SchemaStep::Sql(String)` is executed by `tx.batch(sql)` (`crates/sc-catalog/src/catalog.rs:985`).
  `batch` is the **multi-statement** simple-query protocol. It is built in 17 places, for example
  `crates/sc-api/src/csv.rs:688`, `sc-catalog/src/constraint.rs:401-473`,
  `sc-catalog/src/change_log.rs:120,141`, `sc-model/src/draws.rs:103` and
  `sc-api/src/schema_edit.rs:2882-2914`.
- 8 `batch(&format!…)` / `execute(conn, &format!…)` sites (`sc-catalog/src/tx.rs:311-336`,
  `sc-db-sqlite/src/ddl.rs:705-727`, `sc-migrate/src/runner.rs:335`).
- `SchemaChange::CreateIndex { method, on: IndexOn::Expression(String) }` and
  `ColumnGenerator::Default(String)` are passed straight into DDL (`sc-db-postgres/src/ddl.rs:296-319, 186-191`).
  The code describes them as "trusted code fragments" in comments only.

Every current producer is safe: the strings are constants, `quote_ident` output, or
`render_policy_expr` output. Even so, a future caller with a `String` can reach a multi-statement
executor, and the compiler does not object. `TrustedSql` was meant to close exactly this class
(CODE_QUALITY §4.6), but only for `Statement::Raw`.

Fix: put the DDL path behind the same type, and make the fragment-carrying `SchemaChange` fields
typed too.

```rust
pub enum SchemaStep { Change(SchemaChange), Sql(TrustedSql) }       // was String
pub struct SqlFragment(String);   // only made by sc-query renderers / quote helpers / &'static str
pub enum IndexOn { Columns(Vec<Ident>), Expression(SqlFragment) }
pub enum ColumnGenerator { Identity, Default(SqlFragment) }
impl SharedTx { pub async fn batch(&self, sql: &TrustedSql) -> Result<()> }
```

### D3. Low (correctness at the identifier limit) — `<table>_pkey` is computed by concatenation, but Postgres names it by truncation

*Status: verified by reading against Postgres `makeObjectName` semantics; not run.*

- `crates/sc-db-postgres/src/ddl.rs:130-133` (`SetPrimaryKey`) drops `quote_ident(format!("{table}_pkey"))`.
  `:98-102` (`RenameTable`) renames `{from}_pkey` → `{to}_pkey`.
- `check_identifier` (`crates/sc-api/src/schema_edit.rs:3577-3597`) allows names of up to **63** bytes.
- For a table name of 59–63 bytes, Postgres names the key `<first 58 bytes>_pkey`. The name
  Feldspar computes is 64–68 bytes, and Postgres **silently truncates** it to the first 63 bytes,
  which is a different name. `DROP CONSTRAINT IF EXISTS` then does nothing, and
  `ADD PRIMARY KEY` fails with "multiple primary keys". A rename also leaves the key under the old
  name.
- Elsewhere the code already knows about this: `sc-catalog/src/constraint.rs:86, 815-830`
  (`truncate_ident` with a hash).

Admin-only, and no security impact. It shows why identifier **length** belongs in the type.

Fix: give `Ident` a maximum of 63 bytes and a `derived(&self, suffix) -> Ident` that applies
Postgres's truncation rule (or the existing FNV `truncate_ident`). Better still, find the primary
key by `contype = 'p'` instead of by name.

### D4. Low (hardening; extends SECURITY_REVIEW "SQL hardening" item) — inline dollar-quoted blocks rely on a check made two crates away

*Status: verified (by reading).*

- `crates/sc-db-postgres/src/ddl.rs:91-103` (`DO $$ … $$`, rename) and `:213-224` (`DO $sc$`), and
  `crates/sc-api/src/csv.rs:673-685` (`DO $sc$`), inline names. They have no tag check. Contrast
  `sc-catalog/src/constraint.rs:785-791`, which refuses a body that contains its tag.
- They are safe only because `check_identifier` (sc-api) forbids `$`. Neither the DDL renderer nor
  `Catalog::create_table` (`sc-catalog/src/catalog.rs:736-790`) checks names. Tables reach the
  catalog without `check_identifier` by other routes: introspected or adopted external tables
  (where the names are whoever created them), backup restore, and `sc-analytics/src/demo.rs`.
- csv.rs:673 also passes the table name to `format('%I')` **after** doubling `"` itself
  (`table_lit`), which escapes it twice. The result is wrong for any name containing `"`.
  Today no such name passes `check_identifier`.

Fix: the `Ident` type (below). The DDL renderer accepts `&Ident`, so names are checked once at the
type boundary. The `DO` blocks bind names with `format(%I)` from a literal built by one
`quote_literal`, or the code is rewritten to look up the constraint in a separate `SELECT`
instead of a `DO` block.

### D5. Info (structure) — seven identifier quoters and four literal quoters

*Status: verified (by reading).*

Identifier quoters:
- `sc-db-postgres/src/dialect.rs:42`
- `sc-db-sqlite/src/dialect.rs:46`
- `sc-catalog/src/change_log_sqlite.rs:88`
- `sc-migrate/src/ctx.rs:128`
- `sc-api/src/geo_import/gpkg.rs:87`
- hand-built in `sc-api/src/csv.rs:673`
- hand-quoted `\"{log}\"` / `\"{SYNC_KEYS_TABLE}\"` in `sc-catalog/src/sync.rs:179,252` and
  `sc-api/src/sync/push.rs:512`. These sit next to `quote_ident` calls in the same file
  (`push.rs:327,537`).

Literal quoters:
- the `SqlDialect::quote_literal` default (`sc-query/src/dialect.rs:47`)
- `sc-db-postgres/src/ddl.rs:368`
- `sc-catalog/src/constraint.rs:812`
- `sc-catalog/src/change_log_sqlite.rs:92`

All are correct today, but each is a place to forget NUL or length handling.

Public helpers take **SQL fragments** as `&str`, with the same type as a raw name:
`sc_catalog::as_text_sql(expr: &str)` (`change_log.rs:372`), `row_json_sql(alias: &str)` (`:363`)
and `json_texts_sql(bind: &str)` (`:377`). Call sites pass a mix of raw and pre-quoted text, for
example `as_text_sql(&format!("t.{}", quote(..)))` at `sc-api/src/undo.rs:645,685`.

### D6. Low (hardening) — redirect validators: three copies with different rules

*Status: verified (by reading).*

- `sc-server/src/oauth.rs:184` `safe_return_to` and `sc-module/src/consent.rs:266` `is_local_path`
  refuse `/\` only as a **prefix**, and do not encode the value.
- `sc-viewpattern/src/framework.rs:1556` `safe_redirect` refuses any `\` and percent-encodes.

All three are sound today: a path that starts with a single `/` and is not followed by `/` or `\`
cannot change the authority. But they will drift. For the Astro `/_goal` case, see the second
review.

Fix: one `RedirectTarget` in a shared crate (below).

### D7. Info (hardening) — download filenames depend on each producer to sanitise

*Status: verified (by reading).*

`sc-server/src/router.rs:3066-3076` writes `attachment; filename="{file.filename}"` with only
`HeaderValue::from_str` as a guard. That rejects CR/LF, but a `"` or `;` would still pass through
into the header. Its comment says the filename is "server-built". Today that is true:
`posterior::run_filename` (`posterior.rs:152`) sanitises, and the other two producers use fixed
names. The RFC 5987 encoder already exists (`file_response.rs:88`). Use it here, or require a
`DownloadName` newtype.

### D8. Info — metrics: `TableId` has disappeared; identity is `&str` everywhere

*Status: verified (by reading).*

| Measure | Count |
|---|---|
| `table: &str` / `table_name: &str` parameters | **287** |
| `column/col/field(_name): &str` parameters | **267** |
| `TableId` mentions in `crates/*/src` | **2** (doc comments only; the struct is gone, so CODE_QUALITY §3.5 is out of date) |
| `struct Ident` / `TableName` / `ColumnName` | **0** |
| `quote_ident(` call sites | 163 |
| `TrustedSql::composed` | 17 |
| `TrustedSql::admin_authored` | 4 |
| `SchemaStep::Sql(` (untyped raw DDL) | 17 |
| `batch/execute(&format!…)` | 8 |
| hand-quoted `"\"{x}\""` in SQL | 16 |

---

## Proposed type-system fixes (combined)

```rust
// sc-query (or a new tiny sc-ident crate, depended on by sc-db, sc-catalog, drivers)

/// A validated SQL identifier: non-empty, ≤63 bytes, no NUL, and for names Saltcorn
/// *creates*, ^[A-Za-z_][A-Za-z0-9_]*$. `Display` is NOT provided: you
/// must go through a dialect, so a raw name can never be formatted into SQL.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct Ident(Box<str>);

impl Ident {
    /// Names Saltcorn creates (schema_edit, modules, CSV/geo import).
    pub fn new(s: &str) -> Result<Ident, IdentError> { /* charset + len */ }
    /// Names found in a database we did not create (introspection, adoption, restore):
    /// any text but NUL, ≤63 bytes; quoting makes it safe; tagged so DO-blocks refuse it.
    pub fn foreign(s: &str) -> Result<Ident, IdentError> { /* len + no NUL */ }
    pub const fn from_static(s: &'static str) -> Ident { /* const-checked */ }
    /// `<self>_<suffix>` as Postgres would name it (truncate + FNV hash), always ≤63 bytes.
    pub fn derived(&self, suffix: &str) -> Ident { … }
    pub fn as_str(&self) -> &str { &self.0 }
}
impl<'de> Deserialize<'de> for Ident { /* via Ident::foreign, so stored data still loads */ }

pub trait SqlDialect {
    fn quote_ident(&self, ident: &Ident) -> QuotedIdent;   // was &str -> String
    fn quote_literal(&self, s: &str) -> SqlFragment;
}
pub struct QuotedIdent(String);                            // Display OK: already quoted
pub struct SqlFragment(String);                            // renderer / quoter output only

// AST: ColRef { table: Option<Ident>, column: Ident }, Source::Table(Ident),
//      Expr::Lit(Value) stays separate, so a ColumnRef can never be built from a Value.
// Catalog maps: BTreeMap<Ident, Table>; Table.name: Ident; DataField.name: Ident.
// Drop TableId; Ident *is* the table identity.
```

Migration path, keeping wire and schema compatibility:
- `#[serde(transparent)]` plus `Deserialize` via `foreign` means existing stored definitions load
  unchanged.
- Change the dialect signature first. The 163 call sites then fail to compile, and each one shows
  which `&str` needs to become an `Ident`.

Other newtypes:

```rust
/// Same-origin path for Location: starts with one '/', next char not '/' or '\',
/// no control chars, no '\' anywhere; percent-encodes like encodeurl. Display = header-safe.
pub struct RedirectTarget(String);
impl RedirectTarget { pub fn parse(raw: &str) -> Option<Self>; pub fn or_root(raw: Option<&str>) -> Self; }
impl From<RedirectTarget> for http::HeaderValue { … }    // cannot fail → no Redirect::to panic

/// Download filename: built only by `DownloadName::sanitised(&str)`; renders RFC 6266/5987.
pub struct DownloadName(String);
impl DownloadName { pub fn header(&self, attachment: bool) -> HeaderValue; }

/// The formula AST only exists with a depth proof (D1).
pub struct Formula { source: String, ast: Ast, free: FreeVars, _depth: Depth }

/// DDL behind the same capability as Raw (D2).
pub enum SchemaStep { Change(SchemaChange), Sql(TrustedSql) }
```

---

## Checked and sound

- **Values are always bound.** On the AST path, `Renderer::bind` is the only way a value enters
  SQL (`crates/sc-query/src/dialect.rs:143-158`). The inline mode exists only for
  `render_policy_expr` (RLS and row-constraint DDL), and it refuses non-scalars (`:162-176`).
- **JSON path steps** are bound (`-> $n`), never inlined (`sc-query/src/dialect.rs:622-631`).
- **Function, aggregate and window names** are chosen by code from enums.
  - Analytics: `sc-analytics/src/plot/render.rs:942-950, 1652-1660`.
  - Formula translation produces only fixed names (`sc-expr/src/translate.rs:875`, `geo.rs:232`).
    An unknown identifier or JS global is refused (`translate.rs:851-857`).
- **CAST type names**: `is_sql_type_name` (`dialect.rs:976`) is permissive, as already reported.
  The producers are code (`sc-dataset/src/compile.rs:2860` `ty.sql_type()`).
- **LIKE**: user patterns are bound as values (`sc-access/src/filter.rs:158-159, 196-200`). The
  in-memory matchers are linear two-pointer, not exponential (`sc-live/src/filter.rs:369`,
  `sc-catalog/src/inmem.rs:815`). Search terms are escaped with `ESCAPE '\'`
  (`sc-query/src/search.rs:217`).
- **ORDER BY from REST**:
  - The column must be a visible field.
  - Secret columns are refused.
  - Direction is limited to `asc`/`desc`.
  - Extra parts are refused.
  - Evidence: `sc-api/src/query_string.rs:261-318`.
- **Postgres GUCs**: `set_config($1,$2,true)` is bound (`sc-db-postgres/src/transaction.rs:93-95`).
  The role clause inlines only an integer (`sc-catalog/src/rls.rs:449`).
- **search_path**: a connection option, validated to `[A-Za-z0-9_$]` (`sc-db-postgres/src/driver.rs:125-146`).
- **Savepoints**: `&'static str`, validated (`sc-catalog/src/tx.rs:490-502`). CODE_QUALITY's
  `savepoint(name: &str)` item is fixed.
- **RLS DDL**: table names are quoted, and the policy names are constants (`sc-catalog/src/rls.rs:268-299`).
- **Row-constraint trigger**: the body tag is checked (`constraint.rs:785-791`) and the
  full-text language is validated (`constraint.rs:703-712`).
- **Name intake**: `check_identifier` is applied to tables, fields, constraints and renames
  (`schema_edit.rs:1144,1717,2219,2422,2592`). Module tables (`sc-module/src/tables.rs` →
  `schema_edit::apply`), CSV and geo import (`label_to_name` + `schema_edit::apply`), public
  datasets and provided tables all go through it.
- **TrustedSql::composed sites**:
  - Files: history.rs, retention.rs:292, sync.rs:178/288, analytics demo, migrate ctx/runner,
    api history:325, sync/push:335/511/546, undo:650/707, geo_import:461.
  - They interpolate only constants, `quote_ident` output, placeholders and an integer SRID.
  - All data is bound.
- **GraphQL**: depth and complexity limits are set (`sc-api/src/graphql/build.rs:70-71`).
- **JSON**: `serde_json` keeps its default recursion limit of 128 (no `unbounded_depth`), so
  filter-object walkers over request JSON are bounded.
- **Body size**: `DefaultBodyLimit` is applied on every router branch (`sc-server/src/router.rs:503-538`).
  MCP, the MCP proxy and the CSRF form are capped explicitly.
- **Headers**: `HeaderValue::from_str` rejects CR/LF everywhere it is used. `content_disposition`
  is RFC 5987 (`file_response.rs:88`). lettre encodes mail headers.
- **HTML**: the v1 CSRF global is filtered to alphanumerics before the `<script>` is built
  (`sc-viewpattern/src/framework.rs:1404-1417`). Generated TS doc comments escape `*/` for agent
  names (`sc-api/src/typescript.rs:1370`). Other interpolated names are `check_identifier` names.

## Not covered in depth

- The JS side of formula evaluation (V8 throws `RangeError` on deep recursion, so it is not an
  abort risk). Already covered by #23 and #36.
- GeoPackage files are opened with rusqlite (`trusted_schema`, already reported).
- sc-dataset JSON definitions beyond the compile entry.
- The generated-TSX scaffold, which uses Rust `{:?}` as JS string escaping
  (`sc-app/src/scaffold/files.rs:3193`). It is admin-labelled and probably fine, but it is
  fragile.
