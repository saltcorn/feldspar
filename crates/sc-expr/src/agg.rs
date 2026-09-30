//! Aggregation over *incoming* keys — the [`docs/AGG_EXPRS.md`] "proposal G"
//! design (TODO Phase 7).
//!
//! An **inverse relation** is spelled with the Claudian antisigma `Ↄ` (U+2183,
//! category Lu — a valid JavaScript identifier character, exactly like the
//! forward-join `Ⱶ`): `order_linesↃorder` is the array of `order_lines` rows
//! whose `order` key points at the current row. A **curated method chain**
//! aggregates it — native `filter`/`map`/`some`/`every`/`length`/`includes`/
//! `join`, plus the invented `sum`/`min`/`max`/`avg`/`distinct` and the ordered
//! `maxBy`/`minBy`.
//!
//! This module is the shared front end for both evaluators: it recognises an
//! aggregation chain in the lowered [`Ast`], checks it against the curated
//! grammar, and resolves the relation against a [`SchemaShape`]. The symbolic
//! translator (Phase 2) turns a [`Chain`] into one correlated subquery; the
//! reified evaluator (Phase 3) runs the same chain natively with a prelude that
//! implements the invented methods.

use sc_error::{Error, Result};

use crate::ast::{Ast, MemberProp};
use crate::shape::SchemaShape;

/// The inverse-relation operator (U+2183, ROMAN NUMERAL REVERSED ONE HUNDRED —
/// the Claudian antisigma). One character, category Lu: a valid JavaScript
/// identifier character, so `order_linesↃorder` is a single identifier to every
/// parser and engine involved. Splitting on it is this module's job alone. Its
/// forward companion is [`JOIN`](crate::analyze::JOIN).
pub const INVERSE: char = 'Ↄ';

/// Whether `name` is an inverse-relation identifier (contains [`INVERSE`]).
pub(crate) fn is_relation_ident(name: &str) -> bool {
    name.contains(INVERSE)
}

/// A resolved aggregation use, recorded on the [`Analysis`](crate::Analysis):
/// the child table and key field an inverse relation followed, the child fields
/// its value expression reads, and the child fields its filters read. Now it is
/// a prefetch plan (which child rows to fetch); with stored calculated fields
/// (Phase 8) it becomes the dependency record for recomputation triggers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AggUse {
    /// The child table the relation reads.
    pub child_table: String,
    /// The child key field pointing back at the formula's table.
    pub key_field: String,
    /// Child fields read by the value expression (selector / `map` / terminal).
    pub value_fields: Vec<String>,
    /// Child fields read by the `filter` predicates.
    pub filter_fields: Vec<String>,
}

/// An arrow argument to a chain method: its single parameter (bound to the
/// child row) and its body expression.
pub(crate) struct Arrow<'a> {
    pub param: &'a str,
    pub body: &'a Ast,
}

/// What to aggregate: a constant field-name string, or an arrow over the child
/// row. A computed string is refused at validation — a field *name* must never
/// blur with a String field's *value*.
pub(crate) enum Selector<'a> {
    /// A constant string literal naming a child field.
    Field(&'a str),
    /// An arrow over the child row.
    Arrow(Arrow<'a>),
}

/// The terminal method of a chain — the one that produces the scalar (or, for
/// `maxBy`/`minBy`, the row whose member is then accessed).
pub(crate) enum Terminal<'a> {
    /// `.length` — row count.
    Length,
    /// `.sum(sel?)`.
    Sum(Option<Selector<'a>>),
    /// `.avg(sel?)`.
    Avg(Option<Selector<'a>>),
    /// `.min(sel?)`.
    Min(Option<Selector<'a>>),
    /// `.max(sel?)`.
    Max(Option<Selector<'a>>),
    /// `.some(pred)`.
    Some(Arrow<'a>),
    /// `.every(pred)`.
    Every(Arrow<'a>),
    /// `.includes(x)` — membership over a preceding `map`.
    Includes(&'a Ast),
    /// `.join(sep)` — string aggregation over a preceding `map`.
    Join(&'a Ast),
    /// `.maxBy(sel)` — the row with the greatest selector value.
    MaxBy(Selector<'a>),
    /// `.minBy(sel)` — the row with the least selector value.
    MinBy(Selector<'a>),
}

impl Terminal<'_> {
    pub(crate) fn is_ordered_selection(&self) -> bool {
        matches!(self, Terminal::MaxBy(_) | Terminal::MinBy(_))
    }

    /// Whether this terminal yields a boolean — the ones that make sense as a
    /// bare predicate (`some`/`every`/`includes`).
    pub(crate) fn is_boolean(&self) -> bool {
        matches!(
            self,
            Terminal::Some(_) | Terminal::Every(_) | Terminal::Includes(_)
        )
    }
}

/// A parsed aggregation chain, borrowing from the [`Ast`]. Purely syntactic —
/// [`resolve`](Chain::resolve) checks it against a [`SchemaShape`].
pub(crate) struct Chain<'a> {
    /// The root relation identifier, e.g. `"order_linesↃorder"`.
    pub relation: &'a str,
    /// `.filter(pred)` steps, in order.
    pub filters: Vec<Arrow<'a>>,
    /// A single `.map(arrow)`, if present.
    pub map: Option<Arrow<'a>>,
    /// Whether `.distinct()` is applied (its optional selector is the value).
    pub distinct: bool,
    /// The distinct selector, when `.distinct(sel)` carried one.
    pub distinct_selector: Option<Selector<'a>>,
    /// The terminal method.
    pub terminal: Terminal<'a>,
    /// A trailing member access — only valid after `maxBy`/`minBy`.
    pub member: Option<&'a str>,
}

/// A resolved relation: which child table, key field, and the parent column the
/// key targets (the correlation column).
pub(crate) struct Relation {
    pub child_table: String,
    pub key_field: String,
    /// The parent column the child key references (the correlation target).
    pub parent_field: String,
    /// The child table's primary key, when known (for `length`/`maxBy` tie).
    pub child_pk: Option<String>,
}

/// Try to read `ast` as an aggregation chain.
///
/// Returns `None` when `ast` is not rooted at an inverse-relation identifier —
/// the caller then treats it as ordinary syntax. Returns `Some(Err)` when it
/// *is* so rooted but breaks the curated grammar, with a message naming the
/// problem and the alternative.
pub(crate) fn parse_chain(ast: &Ast) -> Option<Result<Chain<'_>>> {
    // Collect the outer→root steps by descending the object/callee spine.
    let mut steps: Vec<Step<'_>> = Vec::new();
    let mut node = ast;
    let relation = loop {
        match node {
            Ast::Ident(name) if is_relation_ident(name) => break name.as_str(),
            Ast::Ident(_) => return None,
            Ast::Member {
                obj,
                prop: MemberProp::Static(name),
                ..
            } => {
                steps.push(Step::Property(name));
                node = obj;
            }
            Ast::Call {
                callee,
                args,
                optional: false,
            } => match &**callee {
                Ast::Member {
                    obj,
                    prop: MemberProp::Static(name),
                    ..
                } => {
                    steps.push(Step::Method(name, args));
                    node = obj;
                }
                _ => return None,
            },
            _ => return None,
        }
    };
    steps.reverse();
    Some(build_chain(relation, &steps))
}

/// One decoded step of a method chain (in root→outer order once reversed).
enum Step<'a> {
    /// `.name(args…)`.
    Method(&'a str, &'a [Ast]),
    /// `.name` — a property access (`.length`, or the trailing member on a
    /// `maxBy`/`minBy` result).
    Property(&'a str),
}

fn build_chain<'a>(relation: &'a str, steps: &[Step<'a>]) -> Result<Chain<'a>> {
    let mut filters = Vec::new();
    let mut map = None;
    let mut distinct = false;
    let mut distinct_selector = None;
    let mut terminal: Option<Terminal<'a>> = None;
    let mut member = None;

    for step in steps {
        // Nothing may follow the terminal except a single member access, and
        // only for the ordered selections.
        if let Some(t) = &terminal {
            let Step::Property(name) = step else {
                return Err(err(relation, "nothing may follow the aggregate"));
            };
            if !t.is_ordered_selection() {
                return Err(err(
                    relation,
                    &format!(
                        "`.{name}` cannot follow this aggregate (only `maxBy`/`minBy` \
                         return a row to read a field from)"
                    ),
                ));
            }
            if member.is_some() {
                return Err(err(relation, "only one field may be read from the row"));
            }
            member = Some(*name);
            continue;
        }
        match step {
            Step::Property(name) if *name == "length" => terminal = Some(Terminal::Length),
            Step::Property(name) => {
                return Err(err(
                    relation,
                    &format!("`.{name}` is not an aggregate (did you mean `.length`?)"),
                ));
            }
            Step::Method(name, args) => {
                terminal = build_method(
                    relation,
                    name,
                    args,
                    &mut filters,
                    &mut map,
                    &mut distinct,
                    &mut distinct_selector,
                )?;
            }
        }
    }

    let terminal = terminal.ok_or_else(|| {
        err(
            relation,
            "an aggregation needs a terminal method \
             (e.g. `.length`, `.sum(…)`, `.some(…)`)",
        )
    })?;

    if member.is_some() && !terminal.is_ordered_selection() {
        return Err(err(relation, "only `maxBy`/`minBy` produce a row"));
    }

    Ok(Chain {
        relation,
        filters,
        map,
        distinct,
        distinct_selector,
        terminal,
        member,
    })
}

/// Decode one method step. On a transformer (`filter`/`map`/`distinct`) it
/// records into the accumulators and returns `Ok(None)` — represented here by
/// leaving `terminal` unset; on a terminal it returns `Ok(Some(terminal))`.
#[allow(clippy::too_many_arguments)]
fn build_method<'a>(
    relation: &'a str,
    name: &'a str,
    args: &'a [Ast],
    filters: &mut Vec<Arrow<'a>>,
    map: &mut Option<Arrow<'a>>,
    distinct: &mut bool,
    distinct_selector: &mut Option<Selector<'a>>,
) -> Result<Option<Terminal<'a>>> {
    let after_pipeline = |what: &str| {
        // filter/map/distinct must precede the terminal, and the ordering
        // filter* → map → distinct is the normal form the translator relies on.
        err(relation, what)
    };
    match name {
        "filter" => {
            if map.is_some() || *distinct {
                return Err(after_pipeline("`filter` must come before `map`/`distinct`"));
            }
            filters.push(one_arrow(relation, name, args)?);
            Ok(None)
        }
        "map" => {
            if map.is_some() {
                return Err(after_pipeline("`map` may be used at most once"));
            }
            if *distinct {
                return Err(after_pipeline("`map` must come before `distinct`"));
            }
            *map = Some(one_arrow(relation, name, args)?);
            Ok(None)
        }
        "distinct" => {
            if *distinct {
                return Err(after_pipeline("`distinct` may be used at most once"));
            }
            *distinct = true;
            *distinct_selector = optional_selector(relation, name, args)?;
            Ok(None)
        }
        "sum" => Ok(Some(Terminal::Sum(optional_selector(
            relation, name, args,
        )?))),
        "avg" => Ok(Some(Terminal::Avg(optional_selector(
            relation, name, args,
        )?))),
        "min" => Ok(Some(Terminal::Min(optional_selector(
            relation, name, args,
        )?))),
        "max" => Ok(Some(Terminal::Max(optional_selector(
            relation, name, args,
        )?))),
        "some" => Ok(Some(Terminal::Some(one_arrow(relation, name, args)?))),
        "every" => Ok(Some(Terminal::Every(one_arrow(relation, name, args)?))),
        "includes" => Ok(Some(Terminal::Includes(one_value_arg(
            relation, name, args,
        )?))),
        "join" => Ok(Some(Terminal::Join(one_value_arg(relation, name, args)?))),
        "maxBy" => Ok(Some(Terminal::MaxBy(required_selector(
            relation, name, args,
        )?))),
        "minBy" => Ok(Some(Terminal::MinBy(required_selector(
            relation, name, args,
        )?))),
        "reduce" => Err(err(
            relation,
            "`reduce` is not available in formulas — use `sum()`, `min()`, `max()` or `some()`",
        )),
        "sort" | "reverse" | "slice" | "at" | "flat" | "flatMap" | "concat" => Err(err(
            relation,
            &format!(
                "`{name}` is not available in formulas (relations are unordered sets; \
                 use `maxBy`/`minBy` to select by a named order)"
            ),
        )),
        "find" | "findIndex" | "indexOf" | "forEach" => Err(err(
            relation,
            &format!("`{name}` is not available in formulas"),
        )),
        other => Err(err(
            relation,
            &format!("`{other}` is not an aggregation method"),
        )),
    }
}

fn one_arrow<'a>(relation: &str, method: &str, args: &'a [Ast]) -> Result<Arrow<'a>> {
    if args.len() != 1 {
        return Err(err(
            relation,
            &format!("`{method}` takes exactly one argument (an arrow)"),
        ));
    }
    as_arrow(relation, method, &args[0])
}

/// A single value-position argument (`includes(x)`, `join(sep)`), which is an
/// ordinary formula expression over the parent scope — never an arrow.
fn one_value_arg<'a>(relation: &str, method: &str, args: &'a [Ast]) -> Result<&'a Ast> {
    if args.len() != 1 {
        return Err(err(
            relation,
            &format!("`{method}` takes exactly one argument"),
        ));
    }
    if matches!(&args[0], Ast::Arrow { .. }) {
        return Err(err(
            relation,
            &format!("`{method}` takes a value, not an arrow"),
        ));
    }
    Ok(&args[0])
}

fn as_arrow<'a>(relation: &str, method: &str, ast: &'a Ast) -> Result<Arrow<'a>> {
    match ast {
        Ast::Arrow { params, body } if params.len() == 1 => Ok(Arrow {
            param: &params[0],
            body,
        }),
        Ast::Arrow { .. } => Err(err(
            relation,
            &format!("the `{method}` arrow takes exactly one parameter (the child row)"),
        )),
        _ => Err(err(
            relation,
            &format!("`{method}` needs an arrow argument"),
        )),
    }
}

/// A selector that must be present (`maxBy`/`minBy`).
fn required_selector<'a>(relation: &str, method: &str, args: &'a [Ast]) -> Result<Selector<'a>> {
    optional_selector(relation, method, args)?.ok_or_else(|| {
        err(
            relation,
            &format!("`{method}` needs a selector (a field name or arrow)"),
        )
    })
}

/// An optional selector argument: absent, a constant field-name string, or an
/// arrow. A computed (non-literal) string is refused by name.
fn optional_selector<'a>(
    relation: &str,
    method: &str,
    args: &'a [Ast],
) -> Result<Option<Selector<'a>>> {
    match args {
        [] => Ok(None),
        [one] => Ok(Some(match one {
            Ast::Str(field) => Selector::Field(field),
            Ast::Arrow { .. } => Selector::Arrow(as_arrow(relation, method, one)?),
            _ => {
                return Err(err(
                    relation,
                    &format!(
                        "the `{method}` selector must be a constant field-name string \
                         or an arrow (a computed name is refused)"
                    ),
                ));
            }
        })),
        _ => Err(err(
            relation,
            &format!("`{method}` takes at most one selector argument"),
        )),
    }
}

impl<'a> Chain<'a> {
    /// Resolve the relation against `shape`: the child table must be in the
    /// shape and its key field a Key targeting `parent_table`.
    pub(crate) fn resolve(&self, shape: &SchemaShape, parent_table: &str) -> Result<Relation> {
        let mut parts = self.relation.split(INVERSE);
        let child_table = parts.next().unwrap_or_default();
        let key_field = parts.next().unwrap_or_default();
        if child_table.is_empty() || key_field.is_empty() || parts.next().is_some() {
            return Err(err(
                self.relation,
                "a relation is spelled `childTableↃkeyField` (one Ↄ, no empty side)",
            ));
        }
        let child = shape.tables.get(child_table).ok_or_else(|| {
            err(
                self.relation,
                &format!("child table `{child_table}` is not in the schema"),
            )
        })?;
        let field = child.fields.get(key_field).ok_or_else(|| {
            err(
                self.relation,
                &format!("`{key_field}` is not a field of `{child_table}`"),
            )
        })?;
        let key = field.key.as_ref().ok_or_else(|| {
            err(
                self.relation,
                &format!("`{key_field}` on `{child_table}` is not a Key field"),
            )
        })?;
        // A parent whose rows are another table's (a dataset stage, see
        // `RowsOf`) correlates on the column holding that table's identifying
        // field, not on the field itself, which it need not have.
        let parent_field = if key.target_table == parent_table {
            key.target_field.clone()
        } else {
            match shape
                .tables
                .get(parent_table)
                .and_then(|p| p.rows_of.as_ref())
            {
                Some(rows) if rows.table == key.target_table && rows.field == key.target_field => {
                    rows.column.clone()
                }
                Some(rows) => {
                    return Err(err(
                        self.relation,
                        &format!(
                            "`{key_field}` on `{child_table}` points at `{}`, and these rows are \
                             rows of `{}`",
                            key.target_table, rows.table
                        ),
                    ));
                }
                None => {
                    return Err(err(
                        self.relation,
                        &format!(
                            "`{key_field}` on `{child_table}` points at `{}`, not `{parent_table}`",
                            key.target_table
                        ),
                    ));
                }
            }
        };
        Ok(Relation {
            child_table: child_table.to_string(),
            key_field: key_field.to_string(),
            parent_field,
            child_pk: child.primary_key.clone(),
        })
    }

    /// Validate the chain's field references against the resolved child table:
    /// every constant selector string names a real child field, `map`/`includes`
    /// pairings are respected, and a trailing member (after `maxBy`/`minBy`)
    /// names a child field or a forward Ⱶ-path on the child.
    pub(crate) fn validate_fields(&self, shape: &SchemaShape, rel: &Relation) -> Result<()> {
        let child = shape.tables.get(&rel.child_table).ok_or_else(|| {
            err(
                self.relation,
                &format!("child table `{}` is not in the schema", rel.child_table),
            )
        })?;
        let check_field = |field: &str| -> Result<()> {
            if child.fields.contains_key(field) {
                Ok(())
            } else {
                Err(err(
                    self.relation,
                    &format!("`{field}` is not a field of `{}`", rel.child_table),
                ))
            }
        };
        let check_selector = |sel: &Option<Selector<'a>>| -> Result<()> {
            if let Some(Selector::Field(f)) = sel {
                check_field(f)?;
            }
            Ok(())
        };
        check_selector(&self.distinct_selector)?;
        match &self.terminal {
            Terminal::Sum(s) | Terminal::Avg(s) | Terminal::Min(s) | Terminal::Max(s) => {
                check_selector(s)?;
                // A bare aggregate needs something to aggregate.
                if s.is_none() && self.map.is_none() {
                    return Err(err(
                        self.relation,
                        "this aggregate needs a selector or a preceding `map`",
                    ));
                }
            }
            Terminal::MaxBy(Selector::Field(f)) | Terminal::MinBy(Selector::Field(f)) => {
                check_field(f)?;
            }
            Terminal::Includes(_) | Terminal::Join(_) if self.map.is_none() => {
                return Err(err(
                    self.relation,
                    "`includes`/`join` need a preceding `map` (they aggregate values, \
                     not whole rows)",
                ));
            }
            _ => {}
        }
        // Some/every/maxBy/minBy do not compose with map/distinct.
        if matches!(
            &self.terminal,
            Terminal::Some(_) | Terminal::Every(_) | Terminal::MaxBy(_) | Terminal::MinBy(_)
        ) && (self.map.is_some() || self.distinct)
        {
            return Err(err(
                self.relation,
                "this terminal does not combine with `map`/`distinct`",
            ));
        }
        if self.terminal.is_ordered_selection() && self.member.is_none() {
            return Err(err(
                self.relation,
                "`maxBy`/`minBy` return a row — read a field from it, \
                 e.g. `.maxBy(\"ts\").value`",
            ));
        }
        if let Some(m) = self.member {
            let first = m.split(crate::analyze::JOIN).next().unwrap_or(m);
            check_field(first)?;
        }
        Ok(())
    }

    /// The child fields this chain's value expression reads (selector / `map` /
    /// terminal), for the [`AggUse`] record. Best-effort: only the direct
    /// `param.field` accesses are collected (the first Ⱶ-segment of a join
    /// path counts as the child field the relation depends on).
    fn value_fields(&self) -> Vec<String> {
        let mut out = Vec::new();
        let push_sel = |sel: &Option<Selector<'a>>, out: &mut Vec<String>| {
            if let Some(sel) = sel {
                collect_selector_fields(sel, out);
            }
        };
        if let Some(map) = &self.map {
            collect_child_fields(map.param, map.body, &mut out);
        }
        if let Some(sel) = &self.distinct_selector {
            collect_selector_fields(sel, &mut out);
        }
        match &self.terminal {
            Terminal::Sum(s) | Terminal::Avg(s) | Terminal::Min(s) | Terminal::Max(s) => {
                push_sel(s, &mut out);
            }
            Terminal::MaxBy(s) | Terminal::MinBy(s) => collect_selector_fields(s, &mut out),
            Terminal::Some(a) | Terminal::Every(a) => {
                collect_child_fields(a.param, a.body, &mut out)
            }
            Terminal::Length | Terminal::Includes(_) | Terminal::Join(_) => {}
        }
        if let Some(m) = self.member {
            out.push(
                m.split(crate::analyze::JOIN)
                    .next()
                    .unwrap_or(m)
                    .to_string(),
            );
        }
        dedup(out)
    }

    /// The child fields the filters read, for the [`AggUse`] record.
    fn filter_fields(&self) -> Vec<String> {
        let mut out = Vec::new();
        for f in &self.filters {
            collect_child_fields(f.param, f.body, &mut out);
        }
        dedup(out)
    }

    /// The chain's sub-expressions in parent scope — arrow bodies and value
    /// arguments — for a recursive walk that may find further aggregations
    /// nested inside them.
    pub(crate) fn sub_expressions(&self) -> Vec<&'a Ast> {
        let mut out: Vec<&'a Ast> = Vec::new();
        for f in &self.filters {
            out.push(f.body);
        }
        if let Some(m) = &self.map {
            out.push(m.body);
        }
        let push_sel = |sel: &Option<Selector<'a>>, out: &mut Vec<&'a Ast>| {
            if let Some(Selector::Arrow(a)) = sel {
                out.push(a.body);
            }
        };
        push_sel(&self.distinct_selector, &mut out);
        match &self.terminal {
            Terminal::Sum(s) | Terminal::Avg(s) | Terminal::Min(s) | Terminal::Max(s) => {
                push_sel(s, &mut out);
            }
            Terminal::MaxBy(Selector::Arrow(a)) | Terminal::MinBy(Selector::Arrow(a)) => {
                out.push(a.body);
            }
            Terminal::Some(a) | Terminal::Every(a) => out.push(a.body),
            Terminal::Includes(e) | Terminal::Join(e) => out.push(e),
            _ => {}
        }
        out
    }

    /// The [`AggUse`] record for this resolved chain.
    pub(crate) fn agg_use(&self, rel: &Relation) -> AggUse {
        AggUse {
            child_table: rel.child_table.clone(),
            key_field: rel.key_field.clone(),
            value_fields: self.value_fields(),
            filter_fields: self.filter_fields(),
        }
    }
}

fn collect_selector_fields(sel: &Selector<'_>, out: &mut Vec<String>) {
    match sel {
        Selector::Field(f) => out.push((*f).to_string()),
        Selector::Arrow(a) => collect_child_fields(a.param, a.body, out),
    }
}

/// Collect `param.field` child-field accesses under `body`. A join path
/// (`param.fkⱵx`) contributes its first segment (`fk`), the child field the
/// relation actually depends on.
fn collect_child_fields(param: &str, body: &Ast, out: &mut Vec<String>) {
    match body {
        Ast::Member {
            obj,
            prop: MemberProp::Static(name),
            ..
        } => {
            if matches!(&**obj, Ast::Ident(id) if id == param) {
                let field = name.split(crate::analyze::JOIN).next().unwrap_or(name);
                out.push(field.to_string());
            } else {
                collect_child_fields(param, obj, out);
            }
        }
        Ast::Member { obj, prop, .. } => {
            collect_child_fields(param, obj, out);
            if let MemberProp::Computed(e) = prop {
                collect_child_fields(param, e, out);
            }
        }
        Ast::Call { callee, args, .. } => {
            collect_child_fields(param, callee, out);
            for a in args {
                collect_child_fields(param, a, out);
            }
        }
        Ast::Unary { expr, .. } => collect_child_fields(param, expr, out),
        Ast::Binary { l, r, .. } => {
            collect_child_fields(param, l, out);
            collect_child_fields(param, r, out);
        }
        Ast::Cond { test, cons, alt } => {
            collect_child_fields(param, test, out);
            collect_child_fields(param, cons, out);
            collect_child_fields(param, alt, out);
        }
        Ast::Array(elems) => elems
            .iter()
            .for_each(|e| collect_child_fields(param, e, out)),
        Ast::Template { exprs, .. } => exprs
            .iter()
            .for_each(|e| collect_child_fields(param, e, out)),
        Ast::Arrow { body, .. } => collect_child_fields(param, body, out),
        Ast::Ident(_) | Ast::Str(_) | Ast::Num(_) | Ast::Bool(_) | Ast::Null => {}
    }
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn err(relation: &str, msg: &str) -> Error {
    Error::invalid(format!("aggregation `{relation}`: {msg}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sc_error::Result;

    use crate::formula::Formula;
    use crate::shape::{SchemaShape, TableShape};
    use crate::{AggUse, Analysis};

    /// orders(id, customer→customers.id); order_lines(id, order→orders.id, qty,
    /// price, status, product→products.id); products(id, name).
    fn shape() -> SchemaShape {
        SchemaShape::new()
            .table(
                "orders",
                TableShape::new().primary_key("id").field("id").key_field(
                    "customer",
                    "customers",
                    "id",
                ),
            )
            .table(
                "order_lines",
                TableShape::new()
                    .primary_key("id")
                    .field("id")
                    .key_field("order", "orders", "id")
                    .field("qty")
                    .field("price")
                    .field("status")
                    .key_field("product", "products", "id"),
            )
            .table(
                "products",
                TableShape::new()
                    .primary_key("id")
                    .field("id")
                    .field("name"),
            )
            .table("customers", TableShape::new().primary_key("id").field("id"))
    }

    fn validate(src: &str) -> Result<Analysis> {
        Formula::parse(src).unwrap().validate(&shape(), "orders")
    }

    #[test]
    fn incoming_lists_the_keys_pointing_at_a_table() {
        let s = shape();
        assert_eq!(s.incoming("orders"), vec![("order_lines", "order")]);
        assert_eq!(s.incoming("products"), vec![("order_lines", "product")]);
        assert_eq!(s.incoming("order_lines"), Vec::<(&str, &str)>::new());
    }

    #[test]
    fn a_simple_aggregation_records_its_agg_use() {
        let a = validate("order_linesↃorder.sum(\"qty\") > 100").unwrap();
        assert_eq!(
            a.agg_uses,
            BTreeSet::from([AggUse {
                child_table: "order_lines".into(),
                key_field: "order".into(),
                value_fields: vec!["qty".into()],
                filter_fields: vec![],
            }])
        );
    }

    #[test]
    fn filters_and_selectors_record_their_child_fields() {
        let a = validate(
            "order_linesↃorder.filter(r => r.status === \"shipped\").sum(r => r.qty * r.price) > 0",
        )
        .unwrap();
        let use_ = a.agg_uses.first().unwrap();
        assert_eq!(use_.filter_fields, vec!["status".to_string()]);
        assert_eq!(
            use_.value_fields,
            vec!["price".to_string(), "qty".to_string()]
        );
    }

    #[test]
    fn length_and_some_and_maxby_all_parse() {
        assert!(validate("order_linesↃorder.length > 0").is_ok());
        assert!(validate("order_linesↃorder.some(r => r.qty > 5)").is_ok());
        assert!(validate("order_linesↃorder.every(r => r.qty > 0)").is_ok());
        assert!(validate("order_linesↃorder.maxBy(\"qty\").status === \"x\"").is_ok());
        assert!(validate("order_linesↃorder.map(r => r.status).includes(\"x\")").is_ok());
        assert!(validate("order_linesↃorder.map(r => r.status).join(\", \") === \"\"").is_ok());
        assert!(validate("order_linesↃorder.distinct(\"product\").length > 1").is_ok());
    }

    #[test]
    fn an_unknown_child_field_in_a_selector_is_refused() {
        let err = validate("order_linesↃorder.sum(\"nope\") > 0")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`nope` is not a field of `order_lines`"),
            "got: {err}"
        );
    }

    #[test]
    fn a_computed_selector_is_refused() {
        let err = validate("order_linesↃorder.sum(`q` + `ty`) > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("constant field-name string"), "got: {err}");
    }

    #[test]
    fn reduce_is_refused_by_name_with_the_alternative() {
        let err = validate("order_linesↃorder.reduce((a, b) => a + b) > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`reduce` is not available"), "got: {err}");
        assert!(err.contains("sum()"), "got: {err}");
    }

    #[test]
    fn an_unresolvable_relation_is_refused() {
        // No table `widgets` in the shape.
        let err = validate("widgetsↃorder.length > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("child table `widgets`"), "got: {err}");
        // A key that does not point back at `orders`.
        let err = validate("order_linesↃproduct.length > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not `orders`"), "got: {err}");
    }

    #[test]
    fn a_bare_relation_without_a_terminal_is_refused() {
        let err = validate("order_linesↃorder === 1").unwrap_err().to_string();
        assert!(err.contains("needs a terminal"), "got: {err}");
    }

    #[test]
    fn a_field_after_a_non_ordered_aggregate_is_refused() {
        let err = validate("order_linesↃorder.sum(\"qty\").foo === 1")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cannot follow") || err.contains("maxBy"),
            "got: {err}"
        );
    }

    #[test]
    fn a_bare_value_aggregate_needs_a_selector_or_map() {
        let err = validate("order_linesↃorder.sum() > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("selector or a preceding `map`"), "got: {err}");
        // …and with a map it is fine.
        assert!(validate("order_linesↃorder.map(r => r.qty).sum() > 0").is_ok());
    }
}
