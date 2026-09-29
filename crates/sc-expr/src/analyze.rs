//! Free-variable analysis and validation against a [`SchemaShape`].
//!
//! Collection is pure syntax and happens once at parse time: every identifier
//! not bound by an arrow parameter is free, and member accesses on an **ambient
//! object** ([`Ambient`] — `user`, `row`, `old`, `payload`, `context`) are
//! recorded so `user.x` can be checked against the user table and `row.x`
//! against the triggering table.
//! Classification — is this identifier a field, a Ⱶ-join path, an ambient
//! object, an operation flag, a whitelisted global, or a mistake — needs a
//! shape, so it happens in [`Formula::validate`], which the admin API calls on
//! save and the catalog merge calls on load.

use std::collections::{BTreeMap, BTreeSet};

use sc_error::{Error, Result};

use crate::agg::{self, AggUse, INVERSE};
use crate::ast::{Ast, MemberProp};
use crate::formula::Formula;
use crate::shape::SchemaShape;

/// The Ⱶ join operator (U+2C75). One character, category Lu — which is the
/// whole trick: it is a valid JavaScript identifier character, so a join path
/// is a single identifier to every parser and engine involved. Splitting on it
/// is this module's job alone.
pub const JOIN: char = 'Ⱶ';

/// Globals a formula may reference without them being fields. These exist in
/// the reified evaluator (they are JavaScript's own), so validation must not
/// reject them; the symbolic translator simply reports them untranslatable. A
/// field with one of these names shadows the global — consistent with Phase 3,
/// where row fields are bound over the global scope.
pub(crate) const GLOBALS: &[&str] = &[
    "undefined",
    "NaN",
    "Infinity",
    "Math",
    "Number",
    "String",
    "Boolean",
    "Array",
    "JSON",
    "Date",
];

/// An **ambient object**: an identifier that binds to an object-or-null which
/// the formula does not *range over*.
///
/// The scope rule the whole formula language follows is that bare identifiers
/// name fields of the row a formula is about; everything else it can see is
/// ambient and spelled as an object. `user` was the first (§7.3) and is always in
/// scope, because every formula has a caller. `row` and `old` are the triggering
/// event's row and its pre-update state, in scope only where a
/// [`SchemaShape`](crate::SchemaShape) declares them — a trigger's `only_if` or
/// an action's configuration — so an ownership formula naming `row` is still the
/// unknown identifier it always was.
///
/// All three share one set of semantics, which is why they are one enum rather
/// than three special cases: object-or-null, member access null-guarded (a null
/// object yields null, never a throw), and members checked against the fields of
/// whatever table the object stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ambient {
    /// `user` — the caller's fields, or null when nobody is logged in.
    User,
    /// `row` — the row the triggering event is about.
    Row,
    /// `old` — that row as it was before an update; null on any other event.
    Old,
    /// `payload` — the event's free-form detail: a directly-run trigger's posted
    /// body, an error event's `{kind, message, …}`. Null where the event has
    /// none, and **fieldless**: unlike `row` and `user`, nothing declares what is
    /// in it, so `payload.anything` resolves and reads null when it is not there.
    Payload,
    /// `context` — a **workflow run's** accumulated context (§10.3): what the
    /// steps before this one returned, keyed by their names, plus whatever a
    /// `Set` step wrote.
    ///
    /// In scope only where there *is* a run, which is what makes it a new
    /// ambient object rather than a redefinition of the bare scope: bare
    /// identifiers already mean "the row this formula ranges over", and quietly
    /// making them mean the context inside a workflow would make one language
    /// mean two things. Fieldless like `payload`, and for the same reason —
    /// nothing declares what a run has accumulated, so `context.x` resolves and
    /// reads null before the step that writes it has run.
    Context,
}

impl Ambient {
    /// Every ambient object, in scope-declaration order.
    pub const ALL: [Ambient; 5] = [
        Ambient::User,
        Ambient::Row,
        Ambient::Old,
        Ambient::Payload,
        Ambient::Context,
    ];

    /// The identifier this object is spelled with.
    pub fn as_str(self) -> &'static str {
        match self {
            Ambient::User => "user",
            Ambient::Row => "row",
            Ambient::Old => "old",
            Ambient::Payload => "payload",
            Ambient::Context => "context",
        }
    }

    /// The ambient object an identifier names, if it names one. Whether it is
    /// *in scope* is a separate question the shape answers
    /// ([`SchemaShape::declares_ambient`](crate::SchemaShape::declares_ambient)).
    pub fn from_ident(name: &str) -> Option<Ambient> {
        Ambient::ALL.into_iter().find(|a| a.as_str() == name)
    }
}

impl std::fmt::Display for Ambient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The operation-flag variables (§ GOALS Authorization): which access operation
/// a formula is being evaluated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpFlag {
    /// `_read` — the operation is a select.
    Read,
    /// `_insert`.
    Insert,
    /// `_update`.
    Update,
    /// `_delete`.
    Delete,
    /// `_write` — insert, update or delete.
    Write,
}

impl OpFlag {
    /// The flag for a variable name, if it is one.
    pub(crate) fn from_ident(name: &str) -> Option<OpFlag> {
        match name {
            "_read" => Some(OpFlag::Read),
            "_insert" => Some(OpFlag::Insert),
            "_update" => Some(OpFlag::Update),
            "_delete" => Some(OpFlag::Delete),
            "_write" => Some(OpFlag::Write),
            _ => None,
        }
    }
}

/// The free variables of a formula, collected once at parse time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreeVars {
    /// Every free identifier, ambient objects and flags included.
    pub idents: BTreeSet<String>,
    /// Property names statically accessed on each ambient object
    /// (`user.id` → `User` ↦ `id`).
    pub ambient_props: BTreeMap<Ambient, BTreeSet<String>>,
    /// Ambient objects indexed with a computed expression (`user[x]`), which
    /// membership checks cannot see through.
    pub ambient_dynamic: BTreeSet<Ambient>,
}

/// Collect the free variables of a lowered AST.
pub(crate) fn collect_free_vars(ast: &Ast) -> FreeVars {
    let mut out = FreeVars::default();
    let mut locals: Vec<String> = Vec::new();
    walk(ast, &mut locals, &mut out);
    out
}

fn walk(ast: &Ast, locals: &mut Vec<String>, out: &mut FreeVars) {
    match ast {
        Ast::Ident(name) => {
            if !locals.iter().any(|l| l == name) {
                out.idents.insert(name.clone());
            }
        }
        Ast::Str(_) | Ast::Num(_) | Ast::Bool(_) | Ast::Null => {}
        Ast::Member { obj, prop, .. } => {
            // `user.x` / `row.x` is recorded for validation; the object itself is
            // still a free identifier like any other.
            if let Ast::Ident(name) = &**obj
                && let Some(amb) = Ambient::from_ident(name)
                && !locals.iter().any(|l| l == name)
            {
                match prop {
                    MemberProp::Static(p) => {
                        out.ambient_props.entry(amb).or_default().insert(p.clone());
                    }
                    MemberProp::Computed(_) => {
                        out.ambient_dynamic.insert(amb);
                    }
                }
            }
            walk(obj, locals, out);
            if let MemberProp::Computed(e) = prop {
                walk(e, locals, out);
            }
        }
        Ast::Call { callee, args, .. } => {
            walk(callee, locals, out);
            for a in args {
                walk(a, locals, out);
            }
        }
        Ast::Unary { expr, .. } => walk(expr, locals, out),
        Ast::Binary { l, r, .. } => {
            walk(l, locals, out);
            walk(r, locals, out);
        }
        Ast::Cond { test, cons, alt } => {
            walk(test, locals, out);
            walk(cons, locals, out);
            walk(alt, locals, out);
        }
        Ast::Array(elems) => {
            for e in elems {
                walk(e, locals, out);
            }
        }
        Ast::Template { exprs, .. } => {
            for e in exprs {
                walk(e, locals, out);
            }
        }
        Ast::Arrow { params, body } => {
            let depth = locals.len();
            locals.extend(params.iter().cloned());
            walk(body, locals, out);
            locals.truncate(depth);
        }
    }
}

/// One argument of a **hoisted module function call** (§4b).
///
/// Only two shapes, and the narrowness is the design rather than an unfinished
/// edge. A hoisted call is resolved *before the formula runs*, by
/// `sc_catalog::prefetch_bindings`, which has the row's values and no evaluator
/// — so what an argument may be is what can be **read** there, not what can be
/// computed. §4b's "or pure expressions over those" is refused instead, for a
/// reason worth stating plainly: computing `notes + "!"` outside the evaluator
/// would mean a second implementation of this language's semantics in Rust,
/// with JavaScript's own coercion in it, and two implementations that can
/// silently disagree is exactly what the Ⱶ-join prefetch exists as one copy to
/// avoid. A refusal names the call; a disagreement would name nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModuleArg {
    /// A literal, as its JSON text (`"hi"`, `3`, `true`, `null`).
    Literal(String),
    /// A value in the formula's own row scope: a column, or a Ⱶ-join
    /// identifier that the same prefetch resolved a moment earlier.
    Binding(String),
}

impl ModuleArg {
    /// The text this argument contributes to a call's key.
    pub fn text(&self) -> &str {
        match self {
            ModuleArg::Literal(text) | ModuleArg::Binding(text) => text,
        }
    }
}

/// One module function call a formula makes, hoisted out of it (§4b).
///
/// A static fact of the syntax, exactly as a Ⱶ-join path is: collected here at
/// validation, resolved by the caller before the formula runs, and bound into
/// the evaluator's scope as an ordinary value. The evaluator does no I/O and
/// gains nothing from this.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModuleCall {
    /// What the value binds under — [`hoisted_call_key`]'s text, which the
    /// renderer computes again from the same AST node, so the two sides agree
    /// by construction rather than by convention.
    pub key: String,
    /// The package that supplies the function. Resolved here, because a
    /// formula has no spelling for "the one from that module" and an ambiguous
    /// name is refused on save.
    pub module: String,
    /// The function's own name, as the formula wrote it.
    pub function: String,
    /// The arguments, in order.
    pub args: Vec<ModuleArg>,
}

/// The key a hoistable call node binds under, or `None` when the node is not
/// one.
///
/// Computed from the **AST alone**, which is what lets the renderer
/// ([`crate::normalise`]) find the binding without knowing what a module is:
/// `md_to_html(notes)` and `pad("x",3)`, spelled from the canonical forms of
/// the callee and each argument. A column cannot collide with one — a column
/// name is an identifier and this text is not.
pub fn hoisted_call_key(ast: &Ast) -> Option<String> {
    let Ast::Call { callee, args, .. } = ast else {
        return None;
    };
    let Ast::Ident(function) = &**callee else {
        return None;
    };
    let mut parts = Vec::with_capacity(args.len());
    for arg in args {
        parts.push(literal_or_binding(arg)?.text().to_owned());
    }
    Some(module_call_key(function, &parts))
}

/// The key text for a call, from its function name and its rendered arguments.
fn module_call_key(function: &str, args: &[String]) -> String {
    format!("{function}({})", args.join(","))
}

/// Classify one argument expression, or `None` when it is not hoistable.
///
/// Deliberately syntax and nothing else: an identifier is a binding whatever it
/// turns out to name, and whether it is really a column, a Ⱶ-join or an arrow
/// parameter is decided by the caller, which has the shape and the local scope.
fn literal_or_binding(ast: &Ast) -> Option<ModuleArg> {
    Some(match ast {
        Ast::Str(text) => {
            ModuleArg::Literal(serde_json::to_string(text).unwrap_or_else(|_| "null".to_owned()))
        }
        // Spelled the way `crate::normalise` spells a number, so `3` is `3`
        // rather than `3.0` — the key is read by a person when a call is
        // refused, and it is still valid JSON either way.
        Ast::Num(n) => ModuleArg::Literal(number_literal(*n)),
        Ast::Bool(b) => ModuleArg::Literal(if *b { "true" } else { "false" }.to_owned()),
        Ast::Null => ModuleArg::Literal("null".to_owned()),
        Ast::Ident(name) => ModuleArg::Binding(name.clone()),
        _ => return None,
    })
}

/// A numeric literal as JSON text, integral where it can be.
fn number_literal(n: f64) -> String {
    const MAX_EXACT_INT: f64 = 9_007_199_254_740_992.0; // 2^53
    if !n.is_finite() {
        // Not reachable from a parsed literal, and `null` rather than a token
        // JSON cannot read if it ever is.
        return "null".to_owned();
    }
    if n.fract() == 0.0 && n.abs() <= MAX_EXACT_INT {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// The formula language's one built-in that reaches a fitted model:
/// `predict("House prices")` (milestone 31 §4).
///
/// A global of the language rather than a module function, so it wins over a
/// module function of the same name (a body still reaches that one as
/// `modfn("…").predict`), and a **column** called `predict` shadows it, which
/// is the scope rule's one rule.
pub const PREDICT: &str = "predict";

/// One `predict("…")` a formula makes, hoisted out of it exactly as a
/// [`ModuleCall`] is: collected here at validation, resolved by
/// `sc_catalog::prefetch_bindings` before the formula runs, and bound into the
/// evaluator's scope as a plain value.
///
/// There is no row argument, and no fit: the row is the one the formula is
/// evaluated over, and which fit answers is what the model's `active` flag is
/// for — a formula naming a fit id would break the day the fit was deleted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelCall {
    /// What the value binds under — [`hoisted_call_key`]'s text,
    /// `predict("House prices")`.
    pub key: String,
    /// The model's name, as the formula wrote it.
    pub model: String,
}

/// A Ⱶ-join path, resolved link by link through Key fields.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct JoinPath {
    /// The identifier as written, Ⱶ and all (`publisherⱵname`) — the name the
    /// reified evaluator binds and error messages quote.
    pub ident: String,
    /// The segments: each but the last names a Key field to traverse; the last
    /// names a field on the final target table. Always at least two.
    pub segments: Vec<String>,
}

/// What a validated formula refers to — the classified counterpart of
/// [`FreeVars`], and what the later phases plan from: Phase 2's translator
/// turns `join_paths` into correlated subselects, Phase 5's runtime prefetches
/// them for the reified evaluator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Plain fields of the formula's own table that the formula reads.
    pub fields: BTreeSet<String>,
    /// Every Ⱶ-join path, resolved against the shape.
    pub join_paths: BTreeSet<JoinPath>,
    /// Operation flags used.
    pub flags: BTreeSet<OpFlag>,
    /// The ambient objects the formula reads, with what it reads from each. An
    /// object absent from the map is one the formula never names.
    pub ambient: BTreeMap<Ambient, AmbientUse>,
    /// Every aggregation over an incoming key (Phase 7), resolved against the
    /// shape — the prefetch plan now, the stored-calc trigger dependencies
    /// later.
    pub agg_uses: BTreeSet<AggUse>,
    /// Every **module function call** the formula makes (§4b), resolved against
    /// the shape and hoistable by construction: a call that could not be
    /// hoisted was refused on save rather than recorded here.
    pub module_calls: BTreeSet<ModuleCall>,
    /// Every `predict("…")` the formula makes (milestone 31 §4), hoisted like
    /// a module call. What the model is, and whether it predicts this table's
    /// rows, is the save check's to ask: models are rows, not schema.
    pub model_calls: BTreeSet<ModelCall>,
}

/// What a formula reads from one ambient object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmbientUse {
    /// Property names statically accessed (`user.id` → `id`) — the set validated
    /// against the object's fields.
    pub props: BTreeSet<String>,
    /// True when the object is indexed dynamically (`user[x]`), which membership
    /// checks cannot see through. Recorded, not rejected: it is legitimate
    /// reified and simply untranslatable symbolically.
    pub dynamic: bool,
}

impl Analysis {
    /// Whether the formula names `ambient` at all.
    pub fn uses(&self, ambient: Ambient) -> bool {
        self.ambient.contains_key(&ambient)
    }

    /// The properties the formula reads from `ambient` (empty when unused).
    pub fn ambient_props(&self, ambient: Ambient) -> impl Iterator<Item = &str> {
        self.ambient
            .get(&ambient)
            .into_iter()
            .flat_map(|use_| use_.props.iter().map(String::as_str))
    }

    /// The first module function the formula calls, for a caller that allows
    /// none.
    ///
    /// **Ownership formulas allow none** (§4b), and the reason is
    /// [`JsEvaluator`](crate::JsEvaluator)'s contract that `Err` is deny: a rule
    /// calling `geocode_lat` turns a Nominatim outage into "nobody may read
    /// anything", and every row read waits on a third party. Calculated fields,
    /// `only_if` and the rest take the hoist; the one that decides
    /// authorization does not.
    pub fn first_module_call(&self) -> Option<&ModuleCall> {
        self.module_calls.first()
    }

    /// The first `predict("…")` the formula makes, for a caller that allows
    /// none — an ownership formula, for [`first_module_call`]'s reason: a rule
    /// that waits on a provider makes every read wait on it, and its `Err` is
    /// deny.
    ///
    /// [`first_module_call`]: Analysis::first_module_call
    pub fn first_model_call(&self) -> Option<&ModelCall> {
        self.model_calls.first()
    }

    /// The first ambient object used that is **not** in `allowed` — what a caller
    /// with a narrower scope refuses by name.
    ///
    /// A calculated field allows none of them, an ownership formula only `user`,
    /// a trigger formula all three; each is one call rather than three copies of
    /// the same walk.
    pub fn ambient_outside(&self, allowed: &[Ambient]) -> Option<Ambient> {
        self.ambient
            .keys()
            .copied()
            .find(|amb| !allowed.contains(amb))
    }
}

impl Formula {
    /// Check every free variable of this formula against `shape`, classifying
    /// each as a field of `table`, a Ⱶ-join path, an in-scope [`Ambient`] object,
    /// an operation flag or a global — anything else is an error naming the
    /// identifier and the table. The admin API runs this on save; the catalog
    /// merge runs it on load (an already-stored formula that stops validating is
    /// reported and grants nothing).
    ///
    /// Which ambient objects are in scope is the shape's to declare: `user`
    /// always is, `row`/`old` only where a trigger's shape says so, so a formula
    /// naming `row` in an ownership setting is refused as the unknown identifier
    /// it is rather than quietly evaluating to null.
    pub fn validate(&self, shape: &SchemaShape, table: &str) -> Result<Analysis> {
        let table_shape = shape
            .tables
            .get(table)
            .ok_or_else(|| invalid(table, format_args!("unknown table `{table}`")))?;
        let free = self.free_vars();
        let mut analysis = Analysis::default();
        for (amb, props) in &free.ambient_props {
            if shape.declares_ambient(*amb) {
                analysis.ambient.entry(*amb).or_default().props = props.clone();
            }
        }
        for amb in &free.ambient_dynamic {
            if shape.declares_ambient(*amb) {
                analysis.ambient.entry(*amb).or_default().dynamic = true;
            }
        }
        // Aggregations first: an inverse-relation identifier is only meaningful
        // as the root of a curated chain, so the whole chain is validated here
        // and its relation identifier is *skipped* in the free-variable loop.
        walk_aggregations(self.ast(), shape, table, &mut analysis)?;
        // Module function calls (§4b), collected beside the join paths and
        // classified here at parse time: a call that cannot be hoisted is
        // **refused on save**, naming the call and why, rather than silently
        // evaluating to something else. `called` is what the identifier loop
        // below needs — a module function's own name is a free identifier, and
        // it is a legitimate one exactly where it was called.
        let mut called = BTreeSet::new();
        let predict_shadowed = table_shape.fields.contains_key(PREDICT);
        if !predict_shadowed {
            walk_model_calls(self.ast(), table, &mut Vec::new(), &mut analysis)?;
            if !analysis.model_calls.is_empty() {
                called.insert(PREDICT.to_owned());
            }
        }
        walk_module_calls(
            self.ast(),
            shape,
            table,
            &mut Vec::new(),
            &mut called,
            &mut analysis,
        )?;
        for ident in &free.idents {
            if let Some(flag) = OpFlag::from_ident(ident) {
                analysis.flags.insert(flag);
            } else if let Some(amb) =
                Ambient::from_ident(ident).filter(|amb| shape.declares_ambient(*amb))
            {
                analysis.ambient.entry(amb).or_default();
            } else if table_shape.fields.contains_key(ident) {
                // Field names win over globals, matching the reified scope
                // where row fields are bound over JavaScript's own globals.
                analysis.fields.insert(ident.clone());
            } else if ident.contains(INVERSE) {
                // An inverse relation, validated by `walk_aggregations` above.
            } else if ident.contains(JOIN) {
                analysis
                    .join_paths
                    .insert(resolve_join_path(shape, table, ident)?);
            } else if GLOBALS.contains(&ident.as_str()) {
                // Fine reified, untranslatable symbolically; nothing to record.
            } else if ident == PREDICT {
                // Named but not called with a model's name: `xs.map(predict)`.
                // Every malformed *call* was refused by `walk_model_calls`.
                if !called.contains(ident) {
                    return Err(invalid(
                        table,
                        format_args!(
                            "`predict` is called with the model's name, as \
                             `predict(\"House prices\")`, and cannot be passed as a value"
                        ),
                    ));
                }
            } else if !shape.modules_supplying(ident).is_empty() {
                // A module function named but not *called* here: passed as a
                // value (`items.map(md_to_html)`), where the arity is not known
                // until the formula runs and there is nothing to hoist. Refused
                // by name rather than resolving to a function the evaluator does
                // not have.
                if !called.contains(ident) {
                    return Err(invalid(
                        table,
                        format_args!(
                            "`{ident}` is a module function, and a formula may only call one \
                             directly with its arguments — as `{ident}(a_column)`. Passing it as \
                             a value has nothing to hoist; use a code body instead"
                        ),
                    ));
                }
            } else {
                return Err(invalid(table, format_args!("unknown identifier `{ident}`")));
            }
        }
        // Every ambient member checked against the fields of whatever the object
        // stands for, where the caller has declared them. An undeclared field set
        // means "the caller does not know", which skips the check rather than
        // rejecting every access.
        for (amb, use_) in &analysis.ambient {
            let Some(fields) = shape.ambient_field_set(*amb) else {
                continue;
            };
            for prop in &use_.props {
                if !fields.contains(prop) {
                    return Err(invalid(
                        table,
                        format_args!("`{amb}.{prop}`: `{amb}` has no field `{prop}`"),
                    ));
                }
            }
        }
        Ok(analysis)
    }
}

/// Walk `ast` collecting and validating every **module function call** (§4b).
///
/// `locals` is the arrow-parameter scope, and it is the whole of what
/// distinguishes the two refusals: `md_to_html(notes)` hoists because `notes` is
/// a column of the row, and `items.map(x => md_to_html(x))` cannot because `x`
/// does not exist until the formula runs.
///
/// A call inside a conditional hoists like any other — both branches are
/// resolved, which is wasted work for a pure function and never a wrong answer.
fn walk_module_calls(
    ast: &Ast,
    shape: &SchemaShape,
    table: &str,
    locals: &mut Vec<String>,
    called: &mut BTreeSet<String>,
    analysis: &mut Analysis,
) -> Result<()> {
    if let Ast::Call { callee, args, .. } = ast
        && let Ast::Ident(function) = &**callee
        && !locals.iter().any(|l| l == function)
        // `predict` is the language's own, and wins over a module function of
        // that name; `walk_model_calls` has it.
        && function != PREDICT
        // A column of this table wins, exactly as it wins over a global: the
        // scope rule is one rule.
        && !shape
            .tables
            .get(table)
            .is_some_and(|t| t.fields.contains_key(function))
    {
        let modules = shape.modules_supplying(function);
        if let Some(module) = modules.first() {
            if modules.len() > 1 {
                return Err(invalid(
                    table,
                    format_args!(
                        "`{function}` is supplied by more than one module ({}), and a formula \
                         has no way to say which one it means. Call it from a code body, where \
                         `modfn(\"{module}\").{function}(…)` names the module",
                        modules.join(", ")
                    ),
                ));
            }
            let mut classified = Vec::with_capacity(args.len());
            for (index, arg) in args.iter().enumerate() {
                let position = index + 1;
                let Some(argument) = literal_or_binding(arg) else {
                    return Err(invalid(
                        table,
                        format_args!(
                            "`{function}` is a module function, so it is called before this \
                             formula runs and its arguments must be values that can be read \
                             then — a column, a Ⱶ-join value or a literal. Argument {position} \
                             is computed by the formula itself; compute it in a code body \
                             instead"
                        ),
                    ));
                };
                if let ModuleArg::Binding(name) = &argument
                    && locals.iter().any(|l| l == name)
                {
                    return Err(invalid(
                        table,
                        format_args!(
                            "`{function}` is called inside a `=>` function, where argument \
                             {position} (`{name}`) does not exist until the formula runs. A \
                             module function in a formula is called before it starts, so there \
                             is nothing to hoist; use a code body instead"
                        ),
                    ));
                }
                classified.push(argument);
            }
            let key = module_call_key(
                function,
                &classified
                    .iter()
                    .map(|a| a.text().to_owned())
                    .collect::<Vec<_>>(),
            );
            called.insert(function.clone());
            analysis.module_calls.insert(ModuleCall {
                key,
                module: module.clone(),
                function: function.clone(),
                args: classified,
            });
            // The arguments are literals and identifiers; the identifiers are
            // free variables the loop above classifies as columns or Ⱶ-paths,
            // and there is nothing under them to descend into.
            return Ok(());
        }
    }
    if let Ast::Arrow { params, body } = ast {
        let depth = locals.len();
        locals.extend(params.iter().cloned());
        walk_module_calls(body, shape, table, locals, called, analysis)?;
        locals.truncate(depth);
        return Ok(());
    }
    for child in child_nodes(ast) {
        walk_module_calls(child, shape, table, locals, called, analysis)?;
    }
    Ok(())
}

/// Walk `ast` collecting and validating every `predict("…")` (milestone 31 §4).
///
/// Only called when no column of the table is called `predict`, which would
/// shadow it. The shape is exactly one argument, a string literal; anything
/// else is refused naming the call, in the module calls' style. Whether the
/// model exists and predicts this table's rows is not asked here — models are
/// rows, and this is pure syntax against a shape — but by the save check.
fn walk_model_calls(
    ast: &Ast,
    table: &str,
    locals: &mut Vec<String>,
    analysis: &mut Analysis,
) -> Result<()> {
    if let Ast::Call { callee, args, .. } = ast
        && let Ast::Ident(function) = &**callee
        && function == PREDICT
        && !locals.iter().any(|l| l == function)
    {
        if !locals.is_empty() {
            return Err(invalid(
                table,
                format_args!(
                    "`predict` is called inside a `=>` function. A prediction in a formula is \
                     made for the formula's own row before the formula runs, so it cannot \
                     depend on the function's parameters; use a code body's \
                     `(await models.get(…)).predict(rows)` instead"
                ),
            ));
        }
        let [Ast::Str(model)] = args.as_slice() else {
            let what = if args.len() == 1 {
                "its argument is computed by the formula"
            } else if args.is_empty() {
                "it has no argument"
            } else {
                "it has more than one argument"
            };
            return Err(invalid(
                table,
                format_args!(
                    "`predict` takes one argument, the model's name as a string literal \
                     (`predict(\"House prices\")`), and {what}. It predicts the formula's own \
                     row with the model's active fit; to predict other rows, or with another \
                     fit, use a code body"
                ),
            ));
        };
        // A call with one literal argument always has a key; the `else` is
        // unreachable, but an error rather than a panic if that ever changes.
        let Some(key) = hoisted_call_key(ast) else {
            return Err(invalid(
                table,
                format_args!("`predict(\"{model}\")` could not be read as a hoisted call"),
            ));
        };
        analysis.model_calls.insert(ModelCall {
            key,
            model: model.clone(),
        });
        return Ok(());
    }
    if let Ast::Arrow { params, body } = ast {
        let depth = locals.len();
        locals.extend(params.iter().cloned());
        walk_model_calls(body, table, locals, analysis)?;
        locals.truncate(depth);
        return Ok(());
    }
    for child in child_nodes(ast) {
        walk_model_calls(child, table, locals, analysis)?;
    }
    Ok(())
}

/// Walk `ast` collecting and validating every aggregation chain. At each node
/// that reads as a chain (rooted at an inverse-relation identifier) the chain is
/// resolved and field-checked and its [`AggUse`] recorded; the walk then
/// descends only into the chain's *sub-expressions* (arrow bodies, value
/// arguments — parent-scope syntax that may hold further aggregations), not its
/// method spine. Non-chain nodes are walked child by child.
fn walk_aggregations(
    ast: &Ast,
    shape: &SchemaShape,
    table: &str,
    analysis: &mut Analysis,
) -> Result<()> {
    if let Some(chain) = agg::parse_chain(ast) {
        let chain = chain?;
        let rel = chain.resolve(shape, table)?;
        chain.validate_fields(shape, &rel)?;
        analysis.agg_uses.insert(chain.agg_use(&rel));
        // Descend into the parent-scope sub-expressions the chain carries.
        for sub in chain.sub_expressions() {
            walk_aggregations(sub, shape, table, analysis)?;
        }
        return Ok(());
    }
    for child in child_nodes(ast) {
        walk_aggregations(child, shape, table, analysis)?;
    }
    Ok(())
}

/// The direct sub-expressions of a node, for the aggregation walk.
pub(crate) fn child_nodes(ast: &Ast) -> Vec<&Ast> {
    match ast {
        Ast::Ident(_) | Ast::Str(_) | Ast::Num(_) | Ast::Bool(_) | Ast::Null => Vec::new(),
        Ast::Member { obj, prop, .. } => {
            let mut v = vec![&**obj];
            if let MemberProp::Computed(e) = prop {
                v.push(e);
            }
            v
        }
        Ast::Call { callee, args, .. } => {
            let mut v = vec![&**callee];
            v.extend(args.iter());
            v
        }
        Ast::Unary { expr, .. } => vec![expr],
        Ast::Binary { l, r, .. } => vec![l, r],
        Ast::Cond { test, cons, alt } => vec![test, cons, alt],
        Ast::Array(elems) => elems.iter().collect(),
        Ast::Template { exprs, .. } => exprs.iter().collect(),
        Ast::Arrow { body, .. } => vec![body],
    }
}

/// Resolve one Ⱶ-identifier into a [`JoinPath`]: every segment but the last
/// must be a Key field, followed link by link; the last must be a field of the
/// table the chain lands on.
fn resolve_join_path(shape: &SchemaShape, table: &str, ident: &str) -> Result<JoinPath> {
    let segments: Vec<String> = ident.split(JOIN).map(str::to_string).collect();
    if segments.iter().any(String::is_empty) {
        return Err(invalid(
            table,
            format_args!("`{ident}`: empty segment around Ⱶ"),
        ));
    }
    let mut current = table.to_string();
    for (i, segment) in segments.iter().enumerate() {
        let table_shape = shape.tables.get(&current).ok_or_else(|| {
            invalid(
                table,
                format_args!("`{ident}`: table `{current}` is not in the schema shape"),
            )
        })?;
        let field = table_shape.fields.get(segment).ok_or_else(|| {
            invalid(
                table,
                format_args!("`{ident}`: `{segment}` is not a field of table `{current}`"),
            )
        })?;
        let last = i == segments.len() - 1;
        if last {
            break;
        }
        let Some(key) = &field.key else {
            return Err(invalid(
                table,
                format_args!("`{ident}`: `{segment}` is not a Key field on `{current}`"),
            ));
        };
        current = key.target_table.clone();
    }
    Ok(JoinPath {
        ident: ident.to_string(),
        segments,
    })
}

fn invalid(table: &str, msg: std::fmt::Arguments<'_>) -> Error {
    Error::invalid(format!("formula on `{table}`: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::TableShape;

    /// books(id, title, owner, publisher→publishers.id);
    /// publishers(id, name, country→countries.code); countries(code, name).
    fn shape() -> SchemaShape {
        SchemaShape::new()
            .table(
                "books",
                TableShape::new()
                    .field("id")
                    .field("title")
                    .field("owner")
                    .key_field("publisher", "publishers", "id"),
            )
            .table(
                "publishers",
                TableShape::new().field("id").field("name").key_field(
                    "country",
                    "countries",
                    "code",
                ),
            )
            .table("countries", TableShape::new().field("code").field("name"))
            .user_fields(["id", "role", "email"])
    }

    fn validate(src: &str) -> Result<Analysis> {
        Formula::parse(src).unwrap().validate(&shape(), "books")
    }

    /// The shape a trigger's formula is validated against: `row`/`old` in scope
    /// with the triggering table's fields (decision 7).
    fn trigger_shape() -> SchemaShape {
        let event_fields = ["id", "title", "pages", "owner"];
        shape()
            .ambient_fields(Ambient::Row, Some(event_fields))
            .ambient_fields(Ambient::Old, Some(event_fields))
    }

    fn validate_trigger(src: &str) -> Result<Analysis> {
        Formula::parse(src)
            .unwrap()
            .validate(&trigger_shape(), "books")
    }

    #[test]
    fn ambient_row_and_old_are_in_scope_only_where_declared() {
        // Declared: classified as ambient objects, with their properties recorded
        // and the bare field scope untouched.
        let a = validate_trigger("title !== old.title && row.owner === user.id").unwrap();
        assert_eq!(a.fields, BTreeSet::from(["title".to_string()]));
        assert!(a.uses(Ambient::Old) && a.uses(Ambient::Row) && a.uses(Ambient::User));
        assert_eq!(a.ambient_props(Ambient::Old).collect::<Vec<_>>(), ["title"]);
        assert_eq!(a.ambient_props(Ambient::Row).collect::<Vec<_>>(), ["owner"]);

        // Undeclared (an ownership formula's shape): the same identifier is the
        // unknown identifier it always was — not a silent null.
        let err = validate("row.owner === user.id").unwrap_err().to_string();
        assert!(err.contains("unknown identifier `row`"), "got: {err}");
        assert!(!validate("old !== null").is_ok());
    }

    #[test]
    fn context_is_in_scope_only_inside_a_run_and_has_no_declared_fields() {
        // A workflow step's scope (§10.3, decision 8): the trigger's scope plus
        // `context`, declared with no fields because nothing knows what a run
        // has accumulated.
        let workflow_shape = trigger_shape().ambient_fields(Ambient::Context, None::<Vec<String>>);
        let a = Formula::parse("context.total > 100 && row.owner === user.id")
            .unwrap()
            .validate(&workflow_shape, "books")
            .unwrap();
        assert!(a.uses(Ambient::Context));
        assert_eq!(
            a.ambient_props(Ambient::Context).collect::<Vec<_>>(),
            ["total"]
        );
        // Fieldless: a key the step before this one has not written yet
        // resolves, and reads null, rather than being refused on save.
        assert!(
            Formula::parse("context.nothing_yet")
                .unwrap()
                .validate(&workflow_shape, "books")
                .is_ok()
        );
        // And outside a run it is the unknown identifier it should be — a
        // trigger's `only_if` has no context to read.
        let err = validate_trigger("context.total > 100")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown identifier `context`"), "got: {err}");
    }

    #[test]
    fn an_unknown_ambient_member_is_refused_by_name() {
        let err = validate_trigger("old.shoe_size === 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`old.shoe_size`"), "got: {err}");
        assert!(err.contains("has no field `shoe_size`"), "got: {err}");
        // `user`'s check is the same one, worded the same way.
        let err = validate_trigger("user.shoe_size === 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`user.shoe_size`"), "got: {err}");
    }

    #[test]
    fn ambient_outside_the_allowed_set_is_reported_for_the_caller_to_refuse() {
        let a = validate_trigger("row.title === title").unwrap();
        // A calc field allows no ambient object at all; an ownership formula only
        // `user`; a trigger formula all three.
        assert_eq!(a.ambient_outside(&[]), Some(Ambient::Row));
        assert_eq!(a.ambient_outside(&[Ambient::User]), Some(Ambient::Row));
        assert_eq!(a.ambient_outside(&Ambient::ALL), None);
        let user_only = validate("owner === user.id").unwrap();
        assert_eq!(user_only.ambient_outside(&[Ambient::User]), None);
        assert_eq!(user_only.ambient_outside(&[]), Some(Ambient::User));
    }

    #[test]
    fn an_ambient_object_shadows_a_field_of_the_same_name_where_it_is_in_scope() {
        // A table with a field called `row` keeps it in an ownership formula…
        let with_row_field = SchemaShape::new().table("t", TableShape::new().field("row"));
        let a = Formula::parse("row === 1")
            .unwrap()
            .validate(&with_row_field, "t")
            .unwrap();
        assert_eq!(a.fields, BTreeSet::from(["row".to_string()]));
        assert!(!a.uses(Ambient::Row));
        // …and loses it to the ambient object inside a trigger formula, which is
        // the same rule `user` has always had. Documented, not accidental.
        let shadowed = with_row_field.ambient_fields(Ambient::Row, None::<Vec<String>>);
        let a = Formula::parse("row === 1")
            .unwrap()
            .validate(&shadowed, "t")
            .unwrap();
        assert!(a.fields.is_empty() && a.uses(Ambient::Row));
    }

    #[test]
    fn fields_user_and_flags_classify() {
        let a = validate("_read || (owner === user.id && title !== '')").unwrap();
        assert_eq!(
            a.fields,
            BTreeSet::from(["owner".to_string(), "title".to_string()])
        );
        assert_eq!(a.flags, BTreeSet::from([OpFlag::Read]));
        assert!(a.uses(Ambient::User));
        assert_eq!(
            a.ambient_props(Ambient::User).collect::<Vec<_>>(),
            vec!["id"]
        );
        assert!(a.join_paths.is_empty());
    }

    #[test]
    fn every_operation_flag_is_recognised() {
        let a = validate("_read || _insert || _update || _delete || _write").unwrap();
        assert_eq!(
            a.flags,
            BTreeSet::from([
                OpFlag::Read,
                OpFlag::Insert,
                OpFlag::Update,
                OpFlag::Delete,
                OpFlag::Write,
            ])
        );
    }

    #[test]
    fn a_join_path_resolves_through_key_fields() {
        let a = validate("publisherⱵname === 'ACME'").unwrap();
        let path = a.join_paths.first().unwrap();
        assert_eq!(path.ident, "publisherⱵname");
        assert_eq!(path.segments, vec!["publisher", "name"]);
    }

    #[test]
    fn a_join_path_chains_to_any_depth() {
        let a = validate("publisherⱵcountryⱵname === user.email").unwrap();
        let path = a.join_paths.first().unwrap();
        assert_eq!(path.segments, vec!["publisher", "country", "name"]);
    }

    #[test]
    fn an_unknown_identifier_is_named_with_its_table() {
        let err = validate("writer === user.id").unwrap_err().to_string();
        assert!(err.contains("unknown identifier `writer`"), "got: {err}");
        assert!(err.contains("`books`"), "got: {err}");
    }

    #[test]
    fn a_join_through_a_non_key_field_is_refused() {
        let err = validate("titleⱵname === 'x'").unwrap_err().to_string();
        assert!(
            err.contains("`title` is not a Key field on `books`"),
            "got: {err}"
        );
    }

    #[test]
    fn a_join_to_a_missing_target_field_is_refused() {
        let err = validate("publisherⱵcity === 'x'").unwrap_err().to_string();
        assert!(
            err.contains("`city` is not a field of table `publishers`"),
            "got: {err}"
        );
    }

    #[test]
    fn an_empty_join_segment_is_refused() {
        // `Ⱶname` is a lone identifier starting with the join character.
        let err = validate("Ⱶname === 'x'").unwrap_err().to_string();
        assert!(err.contains("empty segment"), "got: {err}");
    }

    #[test]
    fn an_unknown_user_field_is_refused_only_when_fields_are_known() {
        let err = validate("owner === user.shoe_size")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no field `shoe_size`"), "got: {err}");
        // With user fields undeclared, the same formula passes — the caller
        // said it does not know, so nothing can be contradicted.
        let unknowing = shape().ambient_fields(Ambient::User, None::<Vec<String>>);
        let a = Formula::parse("owner === user.shoe_size")
            .unwrap()
            .validate(&unknowing, "books")
            .unwrap();
        assert!(a.uses(Ambient::User));
    }

    #[test]
    fn dynamic_user_access_is_recorded_not_rejected() {
        let a = validate("owner === user[title]").unwrap();
        assert!(a.ambient[&Ambient::User].dynamic);
        // The computed index is itself a free variable and classified.
        assert!(a.fields.contains("title"));
    }

    #[test]
    fn globals_pass_validation_and_fields_shadow_them() {
        // `Math` and `undefined` are JavaScript's own, not unknown identifiers.
        assert!(validate("Math.abs(owner) > 0 && title !== undefined").is_ok());
        // A field named like a global classifies as the field.
        let s = SchemaShape::new().table("t", TableShape::new().field("Math"));
        let a = Formula::parse("Math === 1")
            .unwrap()
            .validate(&s, "t")
            .unwrap();
        assert!(a.fields.contains("Math"));
    }

    #[test]
    fn arrow_parameters_are_not_free_variables() {
        let s = SchemaShape::new().table("t", TableShape::new().field("groups").field("dept"));
        let a = Formula::parse("groups.some(g => g === dept)")
            .unwrap()
            .validate(&s, "t")
            .unwrap();
        // `g` is bound by the arrow; only the two fields are free.
        assert_eq!(
            a.fields,
            BTreeSet::from(["groups".to_string(), "dept".to_string()])
        );
    }

    #[test]
    fn an_arrow_parameter_shadows_only_inside_its_body() {
        let s = SchemaShape::new().table("t", TableShape::new().field("xs"));
        // Outside the arrow, `g` is free again — and unknown.
        let err = Formula::parse("xs.some(g => g > 0) && g")
            .unwrap()
            .validate(&s, "t")
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown identifier `g`"),
            "got: {err}"
        );
    }

    // ---------------------------------------------------------------------
    // Module functions in a formula (§4b)
    // ---------------------------------------------------------------------

    /// The books shape, with `@saltcorn/markdown` installed.
    fn with_markdown() -> SchemaShape {
        shape().module_function("md_to_html", "@saltcorn/markdown")
    }

    fn validate_with_modules(shape: &SchemaShape, src: &str) -> Result<Analysis> {
        Formula::parse(src).unwrap().validate(shape, "books")
    }

    #[test]
    fn a_hoistable_module_call_is_collected_with_its_module_and_its_arguments() {
        let a = validate_with_modules(&with_markdown(), "md_to_html(title) !== ''").unwrap();
        let call = a.module_calls.first().unwrap();
        assert_eq!(call.function, "md_to_html");
        assert_eq!(call.module, "@saltcorn/markdown");
        assert_eq!(call.args, vec![ModuleArg::Binding("title".to_owned())]);
        assert_eq!(call.key, "md_to_html(title)");
        // The argument is still a field of the formula's own table, classified
        // as one — the prefetch reads it from the row it was given.
        assert!(a.fields.contains("title"));
        // And the same key comes back out of the AST alone, which is what lets
        // the renderer find the binding without knowing what a module is.
        let ast = Formula::parse("md_to_html(title)").unwrap();
        assert_eq!(
            hoisted_call_key(ast.ast()).as_deref(),
            Some("md_to_html(title)")
        );
    }

    #[test]
    fn literals_and_join_values_hoist_and_a_computed_argument_does_not() {
        let shape = with_markdown().module_function("pad", "@saltcorn/text");
        // A literal, a Ⱶ-join value and a column all hoist.
        let a = validate_with_modules(&shape, "pad(publisherⱵname, 3) !== ''").unwrap();
        let call = a.module_calls.first().unwrap();
        assert_eq!(
            call.args,
            vec![
                ModuleArg::Binding("publisherⱵname".to_owned()),
                ModuleArg::Literal("3".to_owned()),
            ]
        );
        // The join path is resolved beside it, so the prefetch has the value the
        // argument reads before it makes the call.
        assert_eq!(a.join_paths.first().unwrap().ident, "publisherⱵname");

        // An argument the formula itself computes is refused, naming the call
        // and what would have worked.
        let err = validate_with_modules(&shape, "md_to_html(title + '!') !== ''")
            .unwrap_err()
            .to_string();
        assert!(err.contains("md_to_html"), "{err}");
        assert!(err.contains("Ⱶ-join value or a literal"), "{err}");
        assert!(err.contains("code body"), "{err}");
    }

    #[test]
    fn a_call_inside_a_lambda_is_refused_and_a_conditional_one_is_not() {
        let shape = with_markdown().table(
            "books",
            TableShape::new()
                .field("id")
                .field("title")
                .field("owner")
                .field("notes")
                .field("chapters")
                .key_field("publisher", "publishers", "id"),
        );
        // §4b: the arity is not known until the formula runs.
        let err = validate_with_modules(&shape, "chapters.map(c => md_to_html(c)).length > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside a `=>` function"), "{err}");
        assert!(err.contains("md_to_html"), "{err}");
        // Passing it as a value has nothing to hoist either, and says so rather
        // than reading as an unknown identifier.
        let err = validate_with_modules(&shape, "chapters.map(md_to_html).length > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a module function"), "{err}");
        // A call inside a conditional hoists: both branches are resolved, which
        // is wasted work for a pure function and never a wrong answer.
        let a =
            validate_with_modules(&shape, "owner === null ? md_to_html(notes) : title").unwrap();
        assert_eq!(a.module_calls.len(), 1);
    }

    #[test]
    fn a_name_two_modules_supply_is_refused_in_a_formula_naming_both() {
        let shape = shape()
            .module_function("geocode_lat", "@saltcorn/nominatim-geocode")
            .module_function("geocode_lat", "@saltcorn/other-geocode");
        let err = validate_with_modules(&shape, "geocode_lat(title) > 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("@saltcorn/nominatim-geocode"), "{err}");
        assert!(err.contains("@saltcorn/other-geocode"), "{err}");
        // A formula has no spelling for "the one from that module"; a body has.
        assert!(err.contains("modfn("), "{err}");
    }

    #[test]
    fn a_column_of_that_name_still_wins_and_no_modules_is_the_old_error() {
        // The scope rule is one rule: a field shadows a module function exactly
        // as it shadows a global.
        let shaded = SchemaShape::new()
            .table("t", TableShape::new().field("md_to_html"))
            .module_function("md_to_html", "@saltcorn/markdown");
        let a = Formula::parse("md_to_html === 1")
            .unwrap()
            .validate(&shaded, "t")
            .unwrap();
        assert!(a.fields.contains("md_to_html") && a.module_calls.is_empty());
        // And on a server with no modules the name is what it has always been.
        let err = validate("md_to_html(title) !== ''")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown identifier `md_to_html`"), "{err}");
    }

    #[test]
    fn an_ownership_formula_is_told_about_the_call_it_may_not_make() {
        // Validation itself does not refuse it — a calc field and an `only_if`
        // both may — so the caller that must fail closed is handed the call by
        // name, exactly as `ambient_outside` hands it an ambient object.
        let a = validate_with_modules(&with_markdown(), "md_to_html(title) !== ''").unwrap();
        assert_eq!(a.first_module_call().unwrap().function, "md_to_html");
        assert!(
            validate("owner === user.id")
                .unwrap()
                .first_module_call()
                .is_none()
        );
    }

    // ---------------------------------------------------------------------
    // `predict("…")` in a formula (milestone 31 §4)
    // ---------------------------------------------------------------------

    #[test]
    fn a_predict_call_is_hoisted_under_its_own_key() {
        let a = validate("predict(\"House prices\") > 100000 && title !== ''").unwrap();
        let call = a.first_model_call().unwrap();
        assert_eq!(call.model, "House prices");
        assert_eq!(call.key, "predict(\"House prices\")");
        // The renderer computes the same key from the AST alone.
        let ast = Formula::parse("predict(\"House prices\")").unwrap();
        assert_eq!(
            hoisted_call_key(ast.ast()).as_deref(),
            Some(call.key.as_str())
        );
        // `predict` is not a module call and not a field.
        assert!(a.module_calls.is_empty());
        assert!(!a.fields.contains("predict"));
        // The same model twice is one call.
        let a = validate("predict('M') + predict(\"M\")").unwrap();
        assert_eq!(a.model_calls.len(), 1);
        // A conditional one hoists, as a module call does.
        let a = validate("owner === null ? predict('M') : 0").unwrap();
        assert_eq!(a.model_calls.len(), 1);
    }

    #[test]
    fn a_predict_call_of_the_wrong_shape_is_refused_by_name() {
        for (src, says) in [
            ("predict(title)", "its argument is computed by the formula"),
            (
                "predict('M' + '1')",
                "its argument is computed by the formula",
            ),
            ("predict()", "it has no argument"),
            ("predict('M', 'fit-1')", "it has more than one argument"),
        ] {
            let err = validate(src).unwrap_err().to_string();
            assert!(err.contains("`predict` takes one argument"), "{src}: {err}");
            assert!(err.contains(says), "{src}: {err}");
        }
        let s = SchemaShape::new().table("t", TableShape::new().field("xs"));
        let err = Formula::parse("xs.map(x => predict('M'))")
            .unwrap()
            .validate(&s, "t")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`predict` is called inside a `=>` function"),
            "{err}"
        );
        assert!(err.contains("models.get"), "{err}");
        let err = Formula::parse("xs.map(predict)")
            .unwrap()
            .validate(&s, "t")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be passed as a value"), "{err}");
    }

    #[test]
    fn a_predict_column_shadows_the_built_in_which_wins_over_a_module_function() {
        // A column called `predict` is the field, and calling it is not a
        // prediction.
        let shaded = SchemaShape::new().table("t", TableShape::new().field("predict"));
        let a = Formula::parse("predict === 1")
            .unwrap()
            .validate(&shaded, "t")
            .unwrap();
        assert!(a.fields.contains("predict") && a.model_calls.is_empty());
        // A module supplying `predict` loses to the built-in.
        let with_module = shape().module_function("predict", "@saltcorn/ml");
        let a = validate_with_modules(&with_module, "predict('M') > 0").unwrap();
        assert_eq!(a.model_calls.len(), 1);
        assert!(a.module_calls.is_empty());
    }

    #[test]
    fn an_ownership_formula_is_told_about_the_prediction_it_may_not_make() {
        let a = validate("predict('M') > 0 && owner === user.id").unwrap();
        assert_eq!(a.first_model_call().unwrap().model, "M");
        assert!(
            validate("owner === user.id")
                .unwrap()
                .first_model_call()
                .is_none()
        );
    }

    #[test]
    fn validating_against_a_missing_table_fails() {
        let err = validate_on_missing().unwrap_err().to_string();
        assert!(err.contains("unknown table"), "got: {err}");
    }

    fn validate_on_missing() -> Result<Analysis> {
        Formula::parse("a === 1")
            .unwrap()
            .validate(&SchemaShape::new(), "absent")
    }
}
