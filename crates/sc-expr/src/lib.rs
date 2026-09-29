//! JavaScript formula expressions for authorization and calculated fields
//! (technical design §7.3; TODO "Ownership Formulae & RLS" Phase 1).
//!
//! An ownership formula is a single JavaScript expression over a row's fields,
//! the current `user`, the operation flags (`_read`, `_insert`, `_update`,
//! `_delete`, `_write`) and Ⱶ-joinfields (`publisherⱵname` — one *identifier*,
//! because Ⱶ is a letter to JavaScript). This crate owns that language:
//!
//! - [`Formula::parse`] runs swc over the source, requires a single pure
//!   expression, and lowers it into the crate's own [`Ast`] — the one tree both
//!   later evaluators consume, which is what makes their parity provable.
//! - [`Formula::validate`] classifies every free variable against a
//!   [`SchemaShape`] (the caller's projection of its tables — this crate sits
//!   below `sc-catalog` and never sees a `Table`), yielding an [`Analysis`] or
//!   an error naming the identifier an admin has to fix.
//!
//! - [`Template`] is the same language in a **string**: `{{ }}` tokens, each one
//!   a `Formula`, validated against the same shape and evaluated by the same
//!   evaluator. An email's subject, a `fetch` URL and a message body are
//!   interpolated by this and nothing else, so a token means in a template
//!   exactly what it means in a calculated field.
//!
//! - [`translate`] is the **symbolic evaluator**: it turns a formula into a
//!   boolean `sc_query::Expr` for one [`Operation`] (the flags fold to
//!   constants) under one [`UserEnv`] — `Inline` (user values as literals, for
//!   runtime WHERE injection) or `Guc` (`current_setting('sc.user', true)`
//!   JSON extraction, for RLS policies). What has no SQL counterpart with
//!   matching semantics is a [`TranslateError::Untranslatable`] naming the
//!   construct, which the runtime path answers by falling back to the reified
//!   evaluator — Phase 3, on `deno_core`, consuming this same [`Ast`].
//!
//! - [`CodeRuntime`] is the other engine: opaque JavaScript **code bodies**
//!   (`run_js_code`), on their own pool of isolates, with one op reaching one
//!   [`CodeHost`]. A code body may query and write tables; a formula never can,
//!   and the two isolates are separate so that a blocking host call can never sit
//!   in front of an authorization decision. This crate does not learn what a
//!   table is — the host takes JSON plans and answers JSON.

mod agg;
mod analyze;
mod ast;
mod code;
mod eval;
mod formula;
#[cfg(feature = "eval")]
mod normalise;
mod shape;
mod subquery;
mod template;
mod translate;

pub use agg::{AggUse, INVERSE};
pub use analyze::{
    Ambient, AmbientUse, Analysis, FreeVars, JOIN, JoinPath, ModelCall, ModuleArg, ModuleCall,
    OpFlag, PREDICT, hoisted_call_key,
};
pub use ast::{Ast, BinaryOp, MemberProp, UnaryOp};
#[cfg(feature = "eval")]
pub use code::CodeRuntime;
pub use code::{
    CodeAdapter, CodeCall, CodeHost, CodeHosts, ConsoleLine, ConsoleSink, DEFAULT_CODE_TIMEOUT,
    DEFAULT_CODE_WORKERS, DEFAULT_FETCH_TIMEOUT, DEFAULT_MAX_FETCHES, DEFAULT_MAX_FILE_OPS,
    DEFAULT_MAX_HOST_CALLS, DEFAULT_MAX_INFLIGHT, DEFAULT_MAX_MODULE_CALLS,
    DEFAULT_MAX_TRIGGER_RUNS, FETCH_MARGIN, FetchHost, FileHost, MAX_CODE_TIMEOUT,
    MAX_CONSOLE_LINES, MIN_FETCH_WINDOW, MIN_MODULE_FN_WINDOW, MIN_TRIGGER_WINDOW,
    MODULE_FN_MARGIN, ModuleFnArg, ModuleFnHost, ModuleFnHosts, ModuleFunction, PYTHON,
    SchemaSnapshot, TRIGGER_MARGIN, TriggerHost, V1_API_JS, console_sink, set_isolate_prime,
    take_console,
};
#[cfg(feature = "eval")]
pub use eval::DenoEvaluator;
pub use eval::{FormulaCall, JsEvaluator, value_from_json, value_to_json};
pub use formula::Formula;
pub use shape::{FieldShape, KeyShape, SchemaShape, TableShape};
pub use subquery::{AggFunc, AggregateSpec, aggregate_expr, correlated_aggregate, correlation};
pub use template::{Escape, MAX_PASSES, RenderMode, Template, Token};
pub use translate::{
    AmbientValues, CalcFields, Env, Operation, TranslateError, USER_GUC, UserEnv, join_path_expr,
    join_path_expr_rooted, translate, translate_rooted, translate_value,
};
