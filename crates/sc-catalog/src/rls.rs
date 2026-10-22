//! Postgres row-level security for ownership formulas (§7.3, TODO Phase 6).
//!
//! When a table has `rls_enabled`, the *same* ownership formula the runtime
//! path (§5) evaluates is enforced by the database instead: generated RLS
//! policies decide every row, and the application's own checks are switched off
//! (GOALS: "then you no longer have to check"). Two halves live here:
//!
//! - **Caller context** ([`CallerContext`], [`run_in_context`]): every row
//!   operation on an RLS table runs inside a transaction that first `SET LOCAL`s
//!   the caller's role and identity as GUCs. The policies read them with
//!   `current_setting(name, true)`; a transaction that forgets to set them sees
//!   `NULL`, which every policy treats as "no access", so a forgotten context
//!   **fails closed** — the property is a shape of the policy, not a hope.
//! - **Policy generation** ([`enable_rls`], [`disable_rls`]): the formula's
//!   `sc_query::Expr` (translated with [`UserEnv::Guc`]) rendered — literals
//!   inlined, because `CREATE POLICY` is DDL and takes no binds — into four
//!   per-operation policies, atop `ENABLE`/`FORCE ROW LEVEL SECURITY`.

use sc_db::Row;
use sc_error::{Context, Error, Result};
use sc_expr::{Formula, Operation, SchemaShape, USER_GUC, UserEnv};
use sc_query::{SqlDialect, Statement, render_policy_expr};

use crate::caller::CallerContext;
use crate::catalog::Catalog;
use crate::changeset::{self, CHANGESET_GUC};
use crate::projection::SchemaProjection;
use crate::table::Table;

/// The GUC carrying the caller's role number on the 1–100 scale. Policies read
/// `current_setting('sc.role', true)::int`; the runtime `SET LOCAL`s it.
pub const ROLE_GUC: &str = "sc.role";

/// Run `stmt` inside a transaction that first sets the caller-context GUCs,
/// returning the statement's rows. This is the one path an RLS table's row
/// operations take: the GUCs and the statement share a connection and a
/// transaction, and `SET LOCAL` reverts both on commit.
///
/// A policy violation surfaces from Postgres as SQLSTATE `42501`
/// (`insufficient_privilege`); it is mapped to a [`NotFound`](Error::not_found)
/// so a denied write is indistinguishable from one that matched no row — the
/// same probe-free rule the runtime path keeps.
pub async fn run_in_context(
    catalog: &Catalog,
    context: &CallerContext,
    stmt: &Statement,
) -> Result<Vec<Row>> {
    run_in_context_mode(catalog, context, stmt, Access::ReadWrite).await
}

/// Whether a caller-context transaction may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The default: the transaction commits whatever the statement did.
    ReadWrite,
    /// `READ ONLY`: the database refuses any write the statement attempts.
    ///
    /// What a custom SQL query behind a `GET` runs under (§13.4). The method is
    /// the admin's choice, but the read-only guarantee is not theirs to give
    /// away: an `UPDATE` reached by a `GET` is a mutation a cache, a crawler or
    /// a prefetch can cause, so it fails loudly here rather than quietly
    /// happening.
    ReadOnly,
}

/// Set the caller-context GUCs on a transaction **the caller is holding open**,
/// so statements run on it reach the policies with the right caller.
///
/// [`run_in_context`] is the one-statement form: it opens a transaction, sets
/// these, runs, commits. A caller that needs many statements under one
/// transaction — a CSV import with its foreign keys deferred to commit (§13.1) —
/// cannot use it, and must not be left to `SET LOCAL` by hand: forgetting is
/// silent on a table without policies and fails *closed* on one with them, which
/// is the confusing half of the pair.
pub async fn set_caller_context(
    tx: &mut dyn sc_db::Transaction,
    context: &CallerContext,
) -> Result<()> {
    // **Both**, always. A transaction that runs one statement takes the whole
    // pair from this one caller, and one that runs many ([`SharedTx`], §10.3's
    // step transaction) hands each writer the pair *it* means: a `SET LOCAL`
    // outlives its statement, so leaving one of the two alone would let the last
    // writer's identity decide the next writer's policy. Anonymous is spelled
    // `''` rather than left unset, which both policy clauses fold to `NULL`
    // (`NULLIF(current_setting(…), '')`) — so it reaches them as no access.
    //
    // [`SharedTx`]: crate::SharedTx
    tx.set_local(ROLE_GUC, &context.role.to_string()).await?;
    tx.set_local(USER_GUC, context.user_json().as_deref().unwrap_or(""))
        .await?;
    // The changeset, for the change-log trigger (§14.12), on the same terms:
    // always set, so one writer's changeset is never the next one's.
    tx.set_local(CHANGESET_GUC, &context.changeset_json())
        .await?;
    Ok(())
}

/// Clear the caller-context GUCs on a transaction the caller is holding open, so
/// the next statement on it reaches the policies as **no caller**.
///
/// The other half of [`set_caller_context`], and it exists because a `SET LOCAL`
/// outlives its statement: on a shared transaction ([`SharedTx`](crate::SharedTx))
/// a statement with no caller must not inherit the last writer's identity. The
/// empty string rather than a role nobody has — both policy clauses fold `''` to
/// `NULL` (`NULLIF(current_setting(…), '')`), and `NULL` is what every one of
/// them treats as no access, while a *number* would be a role and role 0 would be
/// more privileged than the admin.
///
/// The changeset is the exception: a write with no caller still belongs to the
/// ambient changeset ([`changeset::current`]), when there is one.
pub async fn clear_caller_context(tx: &mut dyn sc_db::Transaction) -> Result<()> {
    tx.set_local(ROLE_GUC, "").await?;
    tx.set_local(USER_GUC, "").await?;
    let ambient = changeset::current()
        .map(|c| c.guc_value(None))
        .unwrap_or_default();
    tx.set_local(CHANGESET_GUC, &ambient).await?;
    Ok(())
}

/// Whether `driver`'s transactions take the caller GUCs: it has row-level
/// security for the role and user to reach, or change-log triggers for the
/// changeset to reach. Both are Postgres's; asking SQLite for a
/// transaction-local setting would fail a statement that is safe to run.
pub(crate) fn takes_caller_gucs(driver: &dyn sc_db::DatabaseDriver) -> bool {
    let capabilities = driver.capabilities();
    capabilities.row_level_security || capabilities.change_triggers
}

/// [`run_in_context`] in a `READ ONLY` transaction — see [`Access::ReadOnly`].
pub async fn run_in_context_read_only(
    catalog: &Catalog,
    context: &CallerContext,
    stmt: &Statement,
) -> Result<Vec<Row>> {
    run_in_context_mode(catalog, context, stmt, Access::ReadOnly).await
}

async fn run_in_context_mode(
    catalog: &Catalog,
    context: &CallerContext,
    stmt: &Statement,
    access: Access,
) -> Result<Vec<Row>> {
    let driver = catalog.primary();
    let mut tx = driver.begin().await?;
    // Before anything else, including the GUCs: `SET TRANSACTION` may only be
    // issued before the transaction's first statement, so a read-only
    // transaction that set its caller context first would not be one.
    if access == Access::ReadOnly {
        tx.set_read_only().await?;
    }
    // The caller context exists **for the policies**, which is the whole of what
    // reads it. A backend that has no row-level security has nothing to hand it
    // to, and asking for a transaction-local setting there would fail a
    // statement that is perfectly safe to run — the authorization it would have
    // enforced is enforced above the database on such a backend (§7).
    if takes_caller_gucs(driver.as_ref()) {
        set_caller_context(tx.as_mut(), context).await?;
    }
    let outcome = match tx.query(stmt).await {
        Ok(stream) => stream.try_collect().await,
        Err(e) => Err(e),
    };
    match outcome {
        Ok(rows) => {
            tx.commit().await?;
            Ok(rows)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(map_policy_violation(e))
        }
    }
}

/// A policy violation becomes a not-found, so a denied write is
/// indistinguishable from one that matched no row; any other database error
/// passes through unchanged.
///
/// A `USING` policy filters rows silently (an update/delete simply affects
/// none), so the only error Postgres raises is a **`WITH CHECK` violation** on
/// an insert or update — SQLSTATE `42501`, whose message names "row-level
/// security policy". Either spelling is matched: the driver now carries the
/// SQLSTATE in the text it formats (§5's constraint work needed the code for the
/// same reason this did), and the message is kept as the fallback for a driver
/// that does not.
pub(crate) fn map_policy_violation(e: Error) -> Error {
    let chain = sc_error::format_chain(&e);
    if chain.contains("row-level security") || chain.contains("42501") {
        Error::not_found("no such row")
    } else {
        e
    }
}

/// Enable RLS on `table` and (re)create its four policies from the ownership
/// formula, in one transaction — so a table is never left half-configured.
///
/// Emits, in order: `ALTER TABLE … ENABLE ROW LEVEL SECURITY`, then **`FORCE`**
/// (the server connects as the table's owner, whom RLS otherwise exempts —
/// without `FORCE` the policies would be decoration), a `DROP POLICY IF EXISTS`
/// for each of the four names (so this is idempotent and doubles as "recreate"
/// when the formula changes), and one `CREATE POLICY` per operation:
/// SELECT/DELETE with `USING`, INSERT with `WITH CHECK`, UPDATE with both.
///
/// Every policy also carries the **role floor**, so the role half of the access
/// rule moves into the database too:
/// `current_setting('sc.role', true)::int <= <min_role_op> OR (<formula>)`.
pub async fn enable_rls(catalog: &Catalog, table: &Table) -> Result<()> {
    let sql = enable_rls_sql(
        catalog.primary().dialect(),
        &SchemaProjection::live(catalog)?,
        table,
    )?;
    run_ddl(catalog, &sql).await
}

/// Disable RLS on `table`: drop its four policies and the `ENABLE`/`FORCE`
/// flags, idempotently. A dropped table takes its policies with it, so this is
/// only for a table that still exists and is being un-secured.
pub async fn disable_rls(catalog: &Catalog, table_name: &str) -> Result<()> {
    let sql = disable_rls_sql(catalog.primary().dialect(), table_name);
    run_ddl(catalog, &sql).await
}

/// The DDL that [`enable_rls`] runs, as one script — pure so it is unit-tested
/// without a database, **and** so a batch of schema changes can put it in its own
/// transaction rather than running it in a second one (Phase 7).
///
/// Everything it needs off the schema comes from the [`SchemaProjection`], not
/// the catalog: enabling RLS at the end of a batch that added the field the
/// formula names must see that field, and the cache will not have it until the
/// batch commits and reloads.
pub fn enable_rls_sql(
    dialect: &dyn SqlDialect,
    projection: &SchemaProjection,
    table: &Table,
) -> Result<String> {
    let formula = table.ownership.as_ref().ok_or_else(|| {
        Error::invalid(format!(
            "cannot enable row-level security on `{}`: it has no live ownership formula",
            table.name
        ))
    })?;
    let shape = projection.shape();
    // A policy on `table` that queries a child table (a Ↄ-aggregation, Phase 7)
    // whose own policy queries back would make Postgres raise "infinite
    // recursion detected in policy" at query time — refuse at enablement,
    // naming the cycle, instead (principle 5, no silent failures).
    check_no_policy_cycle(projection, table, &shape)?;
    let env = UserEnv::Guc {
        field_types: projection.user_field_types(),
    };
    let ident = dialect.quote_ident(&table.name);

    let mut out = String::new();
    out.push_str(&format!("ALTER TABLE {ident} ENABLE ROW LEVEL SECURITY;\n"));
    out.push_str(&format!("ALTER TABLE {ident} FORCE ROW LEVEL SECURITY;\n"));
    for (op, _) in POLICY_OPS {
        out.push_str(&format!(
            "DROP POLICY IF EXISTS {} ON {ident};\n",
            policy_name(*op)
        ));
    }
    for (op, command) in POLICY_OPS {
        out.push_str(&policy_clause(
            dialect, &shape, &env, table, formula, *op, command,
        )?);
        out.push('\n');
    }
    Ok(out)
}

/// The DDL that [`disable_rls`] runs — public for the same reason
/// [`enable_rls_sql`] is.
pub fn disable_rls_sql(dialect: &dyn SqlDialect, table_name: &str) -> String {
    let ident = dialect.quote_ident(table_name);
    let mut out = String::new();
    for (op, _) in POLICY_OPS {
        out.push_str(&format!(
            "DROP POLICY IF EXISTS {} ON {ident};\n",
            policy_name(*op)
        ));
    }
    out.push_str(&format!(
        "ALTER TABLE {ident} NO FORCE ROW LEVEL SECURITY;\n"
    ));
    out.push_str(&format!(
        "ALTER TABLE {ident} DISABLE ROW LEVEL SECURITY;\n"
    ));
    out
}

/// Refuse to enable RLS on `table` if doing so would close a policy-reference
/// cycle among RLS-enforced tables.
///
/// Only RLS-enabled tables carry policies, so those are the graph's nodes (with
/// `table` treated as about-to-be-enabled). An edge `A → B` means A's policy
/// *queries* B — through a Ↄ-aggregation or a Ⱶ-join path — and thus would run
/// B's policy. A cycle reachable from `table` back to `table` is what Postgres
/// reports as infinite recursion; here it is a named, refused error with the
/// standard fix in the message.
fn check_no_policy_cycle(
    projection: &SchemaProjection,
    table: &Table,
    shape: &SchemaShape,
) -> Result<()> {
    // The nodes: every RLS-enforced table, plus `table` being enabled now.
    let mut enforced: std::collections::BTreeMap<String, Table> = projection
        .tables()
        .iter()
        .filter(|t| t.rls_enabled || t.name == table.name)
        .map(|t| (t.name.clone(), t.clone()))
        .collect();
    enforced.insert(table.name.clone(), table.clone());

    // References restricted to enforced tables (a reference to an unenforced
    // table cannot recurse — it has no policy).
    let refs_of = |t: &Table| -> Result<Vec<String>> {
        let Some(formula) = &t.ownership else {
            return Ok(Vec::new());
        };
        let analysis = formula.validate(shape, &t.name)?;
        let mut out: Vec<String> = analysis
            .agg_uses
            .iter()
            .map(|a| a.child_table.clone())
            .collect();
        // A Ⱶ-join path's first segment resolves to the table it queries.
        for path in &analysis.join_paths {
            if let Some(first) = path.segments.first()
                && let Some(ts) = shape.tables.get(&t.name)
                && let Some(fs) = ts.fields.get(first)
                && let Some(key) = &fs.key
            {
                out.push(key.target_table.clone());
            }
        }
        out.retain(|name| enforced.contains_key(name) && *name != t.name);
        out.sort();
        out.dedup();
        Ok(out)
    };

    // DFS from `table`; a path back to it is a cycle. `table`'s own references
    // are the starting edges.
    let start = refs_of(&enforced[&table.name])?;
    let mut stack: Vec<(String, Vec<String>)> = vec![(table.name.clone(), start)];
    let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut path = vec![table.name.clone()];
    while let Some((_here, refs)) = stack.last_mut() {
        let Some(next) = refs.pop() else {
            stack.pop();
            path.pop();
            continue;
        };
        if next == table.name {
            path.push(next);
            return Err(Error::invalid(format!(
                "cannot enable row-level security on `{}`: its policy would query a table \
                 whose policy queries back, a cycle ({}) Postgres would reject as infinite \
                 recursion. Exempt the child table from RLS, or use a SECURITY DEFINER helper.",
                table.name,
                path.join(" → ")
            )));
        }
        if !visited.insert(next.clone()) {
            continue;
        }
        let next_refs = refs_of(&enforced[&next])?;
        path.push(next.clone());
        stack.push((next, next_refs));
    }
    Ok(())
}

/// The four SQL commands an RLS table needs a policy for, and the [`Operation`]
/// whose flags fold into each.
const POLICY_OPS: &[(Operation, PolicyCommand)] = &[
    (Operation::Read, PolicyCommand::Using("SELECT")),
    (Operation::Insert, PolicyCommand::WithCheck("INSERT")),
    (Operation::Update, PolicyCommand::Both),
    (Operation::Delete, PolicyCommand::Using("DELETE")),
];

/// How a command's policy applies the predicate: `USING` gates the rows it
/// reads/removes, `WITH CHECK` the rows it writes, and `UPDATE` needs both.
enum PolicyCommand {
    Using(&'static str),
    WithCheck(&'static str),
    Both,
}

/// A stable, table-scoped policy name per command — fixed so the
/// drop-then-create in [`enable_rls_sql`] finds the previous one.
fn policy_name(op: Operation) -> String {
    let suffix = match op {
        Operation::Read => "select",
        Operation::Insert => "insert",
        Operation::Update => "update",
        Operation::Delete => "delete",
    };
    format!("sc_owner_{suffix}")
}

/// One `CREATE POLICY` statement: the role floor OR the translated formula.
fn policy_clause(
    dialect: &dyn SqlDialect,
    shape: &SchemaShape,
    env: &UserEnv,
    table: &Table,
    formula: &Formula,
    op: Operation,
    command: &PolicyCommand,
) -> Result<String> {
    // Inline any calculated field the formula names as its defining expression
    // (Phase 8) — the policy has no column to reference, only an expression to
    // substitute; an untranslatable definition surfaces here as the reason RLS
    // cannot be enabled (§4 already refuses the whole formula the same way).
    let calc = table.calc_formulas();
    let pred = sc_expr::translate(
        formula,
        op,
        &sc_expr::Env::new(env).with_calc(&calc),
        shape,
        &table.name,
    )
    .map_err(Error::from)?;
    let floor = match op {
        Operation::Read => table.access.min_role_read,
        _ => table.access.min_role_write,
    };
    // A lower role number is more privileged, so "at or above the floor" is
    // `<= floor`. `NULLIF(…, '')` folds both an unset GUC and the empty-string
    // default a set-once custom GUC leaves on a reused connection to NULL, and
    // `NULL <= n` is NULL → not granted: fail closed on the role half too.
    let role_clause = format!("NULLIF(current_setting('{ROLE_GUC}', true), '')::int <= {floor}");
    let formula_sql = render_policy_expr(dialect, &pred)?;
    let expr = format!("({role_clause} OR ({formula_sql}))");
    let ident = dialect.quote_ident(&table.name);
    let name = policy_name(op);
    Ok(match command {
        PolicyCommand::Using(cmd) => {
            format!("CREATE POLICY {name} ON {ident} FOR {cmd} USING ({expr});")
        }
        PolicyCommand::WithCheck(cmd) => {
            format!("CREATE POLICY {name} ON {ident} FOR {cmd} WITH CHECK ({expr});")
        }
        PolicyCommand::Both => {
            format!(
                "CREATE POLICY {name} ON {ident} FOR UPDATE USING ({expr}) WITH CHECK ({expr});"
            )
        }
    })
}

/// Run a multi-statement DDL script in one transaction.
async fn run_ddl(catalog: &Catalog, sql: &str) -> Result<()> {
    let mut tx = catalog.primary().begin().await?;
    match tx.batch(sql).await {
        Ok(()) => tx.commit().await,
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e).context("applying row-level-security policies")
        }
    }
}
