//! How a **row action's** formulas are run: over which rows, with which values,
//! and under whose authority.
//!
//! The scope rule itself, the configuration parsers and the event's bindings live
//! one layer down, in `sc-action` — they are the same for every action, including
//! [`Fetch`](crate::Fetch) beside these. What is *here* is what only an action that
//! reads and writes a table needs:
//!
//! - **The bare scope is the target table's row.** `status === "draft"` is the row
//!   being written and `row.status` is the event's; an `insert_row` value has no
//!   such row and is read in `sc_action::EVENT_SCOPE` instead.
//! - **Values are typed by their columns**, not merely by their JSON shape, so an
//!   inlined `user.id` is a uuid a `uuid` column can be compared against in SQL.
//!   (Reified evaluation cannot tell the difference — this is for the translated
//!   path.)
//! - **The `where` predicate's two strategies**, and the per-row prefetching each
//!   matched row needs.
//!
//! ## Authority
//!
//! An action's writes are the *admin's*, not the caller's: a trigger is
//! server-side configuration, and an audit row a user may not insert is the
//! archetype of what a trigger exists to write. So every write runs as a caller
//! at [`ROLE_ADMIN`] (which clears every policy's role floor, exactly as the
//! admin API's own row editor does), still carrying the event's user so a policy
//! that reads `user` sees who caused it.
//!
//! On an RLS-enforced table that caller becomes the `SET LOCAL` GUCs the policies
//! read; off one nothing reads them, but it still travels — because the **event**
//! this write raises has to say who caused it, and it carries the **chain** of
//! triggers that led here, which is what lets `Event::firing` see how deep a
//! cascade already is (§10.2).
//!
//! ## Where the write runs
//!
//! Wherever the step says: [`Scope::executor`] is the transaction a **workflow
//! step** hands the action through `ActionContext::transaction` (§10.3, decision
//! 6), and the pooled path for a trigger's own action body. One line at each call
//! site, and the same line in each of the three — an action does not get to have
//! an opinion about whether it is part of a larger unit of work.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_action::{ActionContext, EventBindings, config_str, required_formula};
use sc_auth::{ROLE_ADMIN, USERS_TABLE};
use sc_catalog::{CallerContext, Catalog, Table, prefetch_bindings};
use sc_error::{Error, Result};
use sc_expr::{
    Ambient, Env, Formula, JsEvaluator, Operation, SchemaShape, TranslateError, UserEnv, translate,
    value_from_json,
};
use sc_query::{Expr, Value};
use sc_types::Attrs;
use serde_json::Value as Json;

use sc_api::rows;

/// The `table` setting every row-writing action takes.
pub(crate) const CFG_TABLE: &str = "table";
/// The predicate setting `update_rows` and `delete_rows` take.
pub(crate) const CFG_WHERE: &str = "where";
/// `insert_row`'s field → formula map.
pub(crate) const CFG_VALUES: &str = "values";
/// `update_rows`' field → formula map.
pub(crate) const CFG_ASSIGNMENTS: &str = "assignments";

/// The `where` predicate, parsed.
pub(crate) fn where_formula(config: &Attrs) -> Result<Formula> {
    required_formula(config, CFG_WHERE)
}

/// The target table setting, resolved against the catalog.
pub(crate) fn target_table(catalog: &Catalog, config: &Attrs) -> Result<Table> {
    catalog.require(&config_str(config, CFG_TABLE)?)
}

/// A field an action may write: one of the table's own, not a calculated one
/// (which has no column to write) and not a collaborative document (which is
/// merged, never overwritten).
pub(crate) fn writable_field(table: &Table, field: &str) -> Result<()> {
    match table.field(field) {
        None => Err(Error::invalid(format!(
            "`{}` has no field `{field}`",
            table.name
        ))),
        Some(f) if f.is_calc() => Err(Error::invalid(format!(
            "`{field}` is a calculated field and cannot be written"
        ))),
        Some(f) if f.is_collab_doc() => Err(Error::invalid(format!(
            "`{field}` is a collaborative document and cannot be written by an action: a \
             whole value would overwrite the edits other people are making to it. Use \
             `live.document` in a code body to edit it"
        ))),
        Some(_) => Ok(()),
    }
}

/// One action run's formula environment: the scope, the values the event puts in
/// it, and the engine.
///
/// Built once per run ([`Scope::of`]) rather than per formula, because a
/// configuration with six value formulas would otherwise rebuild the catalog's
/// shape six times.
pub(crate) struct Scope<'a> {
    trigger: &'a str,
    catalog: &'a Catalog,
    /// The transaction this action's reads and writes belong in, when it has one
    /// — a workflow step's (§10.3, decision 6). The whole of what a row action
    /// has to know about it: it is handed to the row layer as an
    /// [`Executor`](rows::Executor) and everything else follows.
    executor: rows::Executor,
    /// The caller, for the authority a write runs under.
    user_json: Option<&'a Json>,
    /// The triggers that led here, including this one — what a write this action
    /// makes carries, so the event it raises knows how deep it is.
    chain: Vec<String>,
    evaluator: &'a Arc<dyn JsEvaluator>,
    shape: SchemaShape,
    /// `row`/`old` and the caller, typed against their own columns.
    bindings: EventBindings,
}

impl<'a> Scope<'a> {
    /// The scope `ctx`'s formulas are read in.
    ///
    /// Fails when the context has no JavaScript engine, naming the trigger: an
    /// action whose configuration *is* formulas has nothing correct to do without
    /// one, and doing nothing quietly is the failure mode this refuses.
    pub(crate) fn of(ctx: &'a ActionContext<'_>) -> Result<Scope<'a>> {
        let event = ctx.event;
        Ok(Scope {
            trigger: ctx.trigger,
            catalog: ctx.catalog,
            executor: rows::Executor::of(ctx.transaction()),
            user_json: event.user.as_ref(),
            chain: ctx.chain.clone(),
            evaluator: ctx.evaluator()?,
            // The scope, and the values in it, both come off the context: a step
            // of a workflow reads `context` and a trigger's own action body does
            // not, and neither the shape nor the bindings gets to decide that
            // for itself.
            shape: ctx.shape()?,
            bindings: typed_bindings(ctx),
        })
    }

    /// Evaluate one configured formula to a JSON value, with `row`'s values bound
    /// as the bare scope (empty for a formula that ranges over no table).
    ///
    /// Always reified: a value formula's result is a *value*, so there is no SQL
    /// statement for a translation to ride in on. `what` names the setting being
    /// computed, so a throwing formula points at the field it belongs to.
    pub(crate) async fn value(
        &self,
        formula: &Formula,
        row: &BTreeMap<String, Value>,
        op: Operation,
        what: &str,
    ) -> Result<Json> {
        let call = self.bindings.call(formula, op, row);
        self.evaluator
            .eval_value(call)
            .await
            .map_err(|e| self.failed(what, &e))
    }

    /// The rows of `table` the `where` predicate **selects**, each carrying
    /// everything the formulas in `also_bound` (the assignments) will read.
    ///
    /// The two-strategy rule, decided once for `update_rows` and `delete_rows`:
    /// the predicate is translated into the `SELECT`'s `WHERE` when it
    /// translates — the event's row and caller inline as literals, so no new SQL
    /// construct is needed — and falls back to fetch-then-filter through the
    /// reified evaluator when it does not, exactly as an ownership read does. The
    /// fallback costs nothing extra: the rows are being fetched either way,
    /// because each one is then written **by primary key, one at a time**, which
    /// is what makes its own triggers fire with its own row (decision 2).
    ///
    /// An evaluator error here **fails the action**, where the same error in an
    /// ownership check would deny. The contracts differ because the questions
    /// do: "may this caller see this row" has a safe answer when the formula
    /// breaks, and "which rows did the admin mean" does not.
    pub(crate) async fn matching_rows(
        &self,
        table: &Table,
        predicate: &Formula,
        also_bound: &[&Formula],
        op: Operation,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        let user_env = UserEnv::Inline(self.bindings.user.clone());
        let calc = table.calc_formulas();
        let env = Env::new(&user_env)
            .with_ambient(&self.bindings.ambient)
            .with_calc(&calc);
        let filter = match translate(predicate, op, &env, &self.shape, &table.name) {
            Ok(predicate) => Some(predicate),
            Err(TranslateError::Untranslatable(_)) => None,
            Err(TranslateError::Error(e)) => return Err(self.failed(CFG_WHERE, &e)),
        };
        let selected_by_sql = filter.is_some();

        // The bindings each surviving row needs beyond its own columns: a
        // Ⱶ-join path or a Ↄ-relation any formula that will run against it reads.
        // The read path could project a join into the query, but these rows are
        // going to be written one at a time anyway, so a resolution per row is
        // the shape this path already has — and `prefetch_bindings` is the one
        // implementation of it (it is also what a trigger's `only_if` uses).
        let mut analyses = Vec::with_capacity(also_bound.len() + 1);
        if !selected_by_sql {
            analyses.push(
                predicate
                    .validate(&self.shape, &table.name)
                    .map_err(|e| self.failed(CFG_WHERE, &e))?,
            );
        }
        for formula in also_bound {
            analyses.push(formula.validate(&self.shape, &table.name)?);
        }

        let fetched = self.fetch(table, filter).await?;
        let mut out = Vec::with_capacity(fetched.len());
        for mut values in fetched {
            for analysis in &analyses {
                prefetch_bindings(self.catalog, table, analysis, &self.shape, &mut values).await?;
            }
            if selected_by_sql || self.selects(predicate, &values, op).await? {
                out.push(values);
            }
        }
        Ok(out)
    }

    /// The caller an action's write runs as: **admin, in the event's user's
    /// name**, carrying the chain of triggers that led here.
    ///
    /// One context for every write, not only for the RLS ones it started as. The
    /// role and the user are what the policies read on an RLS table (see the
    /// module docs) — and off one they are what the *event* this write raises
    /// reports as its caller, which is the same question answered for the same
    /// write. The chain is what bounds the cascade: an event raised by this write
    /// carries it, so `Event::firing` can see how deep it already is.
    /// Where this action's statements run: the step's transaction when there is
    /// one, and the pooled path when there is not.
    pub(crate) fn executor(&self) -> &rows::Executor {
        &self.executor
    }

    pub(crate) fn authority(&self) -> CallerContext {
        CallerContext::new(ROLE_ADMIN, self.user_json.cloned()).chained(self.chain.clone())
    }

    /// Whether the predicate selects one fetched row — the reified half of
    /// [`matching_rows`].
    async fn selects(
        &self,
        predicate: &Formula,
        values: &BTreeMap<String, Value>,
        op: Operation,
    ) -> Result<bool> {
        let call = self.bindings.call(predicate, op, values);
        self.evaluator
            .eval(call)
            .await
            .map_err(|e| self.failed(CFG_WHERE, &e))
    }

    /// The rows of `table` (all of them when there is no filter), as value maps
    /// including any non-stored calculated field a formula may read — through the
    /// row layer, so the read is routed the same way the writes that follow it are.
    async fn fetch(
        &self,
        table: &Table,
        filter: Option<Expr>,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        let mut rows = rows::select_values_in(
            self.catalog,
            table,
            filter,
            Some(&self.authority()),
            &self.executor,
        )
        .await?;
        // An action is server-side code: its formulas read a secret's plaintext
        // (TODO-imap M2), and a value written back is sealed again on the way.
        for row in &mut rows {
            sc_catalog::secrets::reveal_values(self.catalog, table, row)?;
        }
        Ok(rows)
    }

    /// An action failure attributed to the trigger and the setting that caused it.
    fn failed(&self, what: &str, e: &Error) -> Error {
        Error::invalid(format!("trigger `{}`: {what}: {e}", self.trigger))
    }
}

/// The event's bindings, each field read as the type of the **column** it belongs
/// to where there is one: `row`/`old` against the event's own table, `user`
/// against the users table.
///
/// The *structure* — which objects are in scope, `old` present-but-null on an
/// insert, and the run's `context` when this action is a workflow step — comes
/// from [`ActionContext::bind_values`] one layer down, so it cannot drift from
/// what the actions there bind. Only the reading of each value is this crate's,
/// and only because a translated `where` compares these literals against real
/// columns: `user.id` has to be a uuid, not the string a typeless reading gives.
fn typed_bindings(ctx: &ActionContext<'_>) -> EventBindings {
    let catalog = ctx.catalog;
    let event_table = ctx
        .event
        .channel
        .as_deref()
        .and_then(|name| catalog.get(name).ok().flatten());
    let users = catalog.get(USERS_TABLE).ok().flatten();
    ctx.bind_values(|ambient, field, json| {
        let table = match ambient {
            Ambient::User => users.as_ref(),
            Ambient::Row | Ambient::Old => event_table.as_ref(),
            // Neither a payload nor a run context has columns behind it —
            // whatever its sender, or the step before this one, put in it is
            // read as its own JSON shape.
            Ambient::Payload | Ambient::Context => None,
        };
        typed_value(table, field, json)
    })
}

/// One value coerced to its **column's** type where the table declares one, and
/// read as its own JSON shape where it does not (a calculated field, a row from a
/// table since dropped, a value the column cannot hold — which is the event's
/// problem to report, not this conversion's).
fn typed_value(table: Option<&Table>, field: &str, json: &Json) -> Value {
    table
        .filter(|t| t.field(field).is_some())
        .and_then(|t| rows::column_value(t, field, json).ok())
        .unwrap_or_else(|| value_from_json(json))
}

/// The primary key of a selected row as the string the `rows` layer addresses a
/// row by, coercing back through the same JSON rendering an API response uses.
///
/// The row layer's own [`rows::row_key`], because a code body's `db.…update()`
/// resolves its rows and writes them one at a time exactly as these actions do,
/// and one subtly different spelling of "which row is this" between the two would
/// be a bug in whichever of them was written second.
pub(crate) fn row_id(
    table: &Table,
    pk: &str,
    values: &BTreeMap<String, Value>,
) -> Result<(String, Json)> {
    rows::row_key(table, pk, values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_expr::{AmbientValues, TableShape};

    /// `tasks(id, status, count)` with the event's `row` in scope carrying
    /// `books`' fields — the shape the integration tests' triggers are read in,
    /// built by hand so this test needs no database.
    fn shape() -> SchemaShape {
        SchemaShape::new()
            .table(
                "tasks",
                TableShape::new()
                    .primary_key("id")
                    .field("id")
                    .field("status")
                    .field("count"),
            )
            .ambient_fields(Ambient::Row, Some(["id", "title", "status", "pages"]))
    }

    /// The two `where` spellings the integration test asserts the *same rows*
    /// for really do take the two different strategies. Without this, that test
    /// could be comparing the SQL path against itself and pass while the reified
    /// fallback was never run.
    #[test]
    fn the_two_where_strategies_are_chosen_by_translatability() {
        let shape = shape();
        let user = UserEnv::Inline(None);
        let mut ambient = AmbientValues::new();
        ambient.insert(
            Ambient::Row,
            Some(BTreeMap::from([(
                "status".to_owned(),
                Value::Text("done".to_owned()),
            )])),
        );
        let env = Env::new(&user).with_ambient(&ambient);
        let translated = |source: &str| {
            let formula = Formula::parse(source).expect("parses");
            translate(&formula, Operation::Update, &env, &shape, "tasks")
        };
        assert!(translated("status === row.status").is_ok());
        assert!(matches!(
            translated("[status].some(s => s === row.status)"),
            Err(TranslateError::Untranslatable(_))
        ));
    }

    /// An action may not name a collaborative document as a field to write
    /// (live updates L5.1): saving its configuration says so.
    #[test]
    fn an_action_cannot_write_a_document() {
        use sc_catalog::{DataField, DbId, TableId, TableSource};
        use sc_types::{BasicType, RichTypeRef, TypeRef};
        let doc = TypeRef::rich(RichTypeRef::resolve(sc_types::COLLAB_DOC).unwrap());
        let table = Table {
            id: TableId("notes".into()),
            name: "notes".into(),
            database: DbId::primary(),
            source: TableSource::Database,
            fields: vec![
                DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key(),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("body", doc),
            ],
            primary_key: vec!["id".into()],
            label: "notes".into(),
            description: String::new(),
            access: Default::default(),
            attributes: Default::default(),
            overlay: None,
            ownership: None,
            ownership_error: None,
            rls_enabled: false,
            constraints: Vec::new(),
        };
        assert!(writable_field(&table, "title").is_ok());
        let err = writable_field(&table, "body").unwrap_err().to_string();
        assert!(err.contains("collaborative document"), "{err}");
    }
}
