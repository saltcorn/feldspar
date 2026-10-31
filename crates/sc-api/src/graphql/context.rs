//! What a GraphQL resolver knows that the schema does not.
//!
//! The schema is built once, at mount, from the application's tables. Everything
//! else a resolver needs — the catalog to ask, who is asking, the evaluator an
//! untranslatable ownership formula runs on, the cap a list is bounded by —
//! arrives with the *request*, and `async-graphql`'s resolvers are `'static`
//! closures, so it arrives as request [`Data`](async_graphql::Data) rather than
//! as a captured reference.
//!
//! That is the whole reason [`ApiProvider::handle`](crate::ApiProvider::handle)
//! takes the catalog as the `Arc` the server already holds: a provider that only
//! reads it borrows and pays nothing, and this one clones it into one
//! [`RequestContext`] per request.
//!
//! Nothing here decides access. The context carries the caller; the *rule* is
//! [`ownership`](crate::ownership)'s, and this provider reaches it through the
//! same entry points every other reader does.

use sc_types::Role;
use std::sync::Arc;

use async_graphql::dynamic::ResolverContext;
use sc_auth::User;
use sc_catalog::{Catalog, Table};
use sc_error::Result;
use sc_expr::JsEvaluator;

use super::limits::{GraphqlLimits, StatementBudget};
use super::names::SchemaNames;

/// The mount an application's `File` bytes are served from when nobody says
/// otherwise — the REST provider's own default. A GraphQL `File` field is a path
/// and *that* URL; it never becomes a second download path.
pub const DEFAULT_FILE_MOUNT: &str = "/api";

/// Everything one GraphQL request resolves against.
///
/// Cloneable because the child-list [`DataLoader`](async_graphql::dataloader::DataLoader)
/// holds one too: batching changes the shape of a statement, never who is
/// asking, so the loader reads through the same context its resolvers do.
#[derive(Clone)]
pub struct RequestContext {
    /// The catalog every read goes through.
    pub catalog: Arc<Catalog>,
    /// The names this schema was built from — shared, not rebuilt, because the
    /// schema a caller is querying is the one those names describe.
    ///
    /// A resolver reaches for them for one thing: a selection set names a
    /// relation (`employees_aggregate`), and only the derivation knows which
    /// relation that is. Deriving it again per request would be a second rule
    /// for the same question, which is what [`names`](super::names)' whole
    /// module doc argues against.
    pub names: Arc<SchemaNames>,
    /// The authenticated caller, or `None` for an anonymous one.
    pub user: Option<User>,
    /// The engine an untranslatable ownership formula needs (§7.3). Absent is
    /// not "allow": the read fails closed with a configuration error.
    pub evaluator: Option<Arc<dyn JsEvaluator>>,
    /// What this application allows one operation to cost.
    pub limits: GraphqlLimits,
    /// The statements *this request* has left to spend — shared, because the
    /// budget is the operation's and not a field's, and `Arc` because the child
    /// loader holds the same one.
    pub budget: Arc<StatementBudget>,
    /// The REST mount a `File` field's URL is built against.
    pub file_mount: String,
}

impl RequestContext {
    /// The caller's role — theirs, or public when nobody is logged in.
    pub fn role(&self) -> Role {
        sc_auth::role_of(self.user.as_ref())
    }

    /// The caller as the ownership layer wants them.
    pub fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }

    /// The evaluator seam, as the ownership layer wants it.
    pub fn evaluator(&self) -> Option<&Arc<dyn JsEvaluator>> {
        self.evaluator.as_ref()
    }

    /// One of the application's tables, resolved against the **live** catalog.
    ///
    /// Resolved per request rather than captured at mount, because the catalog
    /// is what a schema edit changes: a column added this morning is in the row
    /// the resolver reads, without the schema having to be rebuilt for it.
    pub fn table(&self, name: &str) -> Result<Table> {
        self.catalog.require(name)
    }

    /// Charge one statement about to be issued for `what` against this request's
    /// budget, or refuse it. Called at the point a read is about to run, so the
    /// refusal is the work not done.
    pub fn charge(&self, what: &str) -> Result<()> {
        self.budget.charge(what)
    }
}

/// The request context, or the GraphQL error saying the provider was executed
/// without one — a wiring mistake, not a caller's.
pub fn request_context<'a>(ctx: &ResolverContext<'a>) -> async_graphql::Result<&'a RequestContext> {
    ctx.data::<RequestContext>()
}
