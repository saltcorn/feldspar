//! [`Scoped`]: a catalog with an authority bound to it (code-quality review
//! §4.4, TODO-code-quality Q3.3).
//!
//! An action, an agent's tool or a code body used to be handed `&Catalog` — the
//! power to read and write every row — next to the caller's role and user as
//! plain data, and had to *remember* to act as that caller (security review #3,
//! #37). A `Scoped` carries both, and its row operations are only the
//! authorised ones:
//!
//! - for a **caller** below the administrator, the ownership rule's guarded
//!   functions ([`ownership`](crate::ownership)): the table's floors, its
//!   ownership formula, its RLS policies;
//! - for the **system** or an **administrator**, who meet every floor, the row
//!   layer itself, with the [`Unchecked`] token their authority makes.
//!
//! The unchecked row layer ([`rows`](crate::rows)) is still public, but every
//! entry point takes an [`Unchecked`], which a non-admin [`Principal`] cannot
//! produce without writing down which check vouches for it.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_auth::User;
use sc_catalog::{Authority, Catalog, Principal, Table, Unchecked};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_query::Value;
use sc_types::Role;
use serde_json::Value as Json;

use crate::ownership;
use crate::rows::{self, Executor, RowQuery};

/// A catalog and the authority its row operations run under.
///
/// Cheap to build — two references, an executor and the formula engine handle
/// — so a context hands one out per use rather than storing one.
#[derive(Clone)]
pub struct Scoped<'a> {
    catalog: &'a Catalog,
    who: Authority<'a>,
    executor: Executor,
    evaluator: Option<Arc<dyn JsEvaluator>>,
}

/// How one operation is decided: unchecked (system, administrator) or by the
/// ownership rule for a caller, whose user is read back from the principal.
enum Decided<'s> {
    Unchecked(Unchecked<'s>),
    Checked(&'s Principal, Option<User>),
}

impl<'a> Scoped<'a> {
    /// `catalog` under `who`, on the pool, with the catalog's formula engine.
    pub fn new(catalog: &'a Catalog, who: Authority<'a>) -> Scoped<'a> {
        Scoped {
            catalog,
            who,
            executor: Executor::Pooled,
            evaluator: catalog.formula_evaluator(),
        }
    }

    /// `catalog` under the server's own authority, for `reason` — a greppable
    /// `Scoped::system(` listed in the system-authority inventory, as
    /// `Authority::system(` is.
    pub fn system(catalog: &'a Catalog, reason: &'static str) -> Scoped<'a> {
        Scoped::new(catalog, Authority::system(reason))
    }

    /// The same, running its statements on `executor` (a workflow step's
    /// transaction).
    #[must_use]
    pub fn in_executor(mut self, executor: Executor) -> Scoped<'a> {
        self.executor = executor;
        self
    }

    /// The same, deciding an ownership formula the translator refuses with
    /// `evaluator` rather than the catalog's.
    #[must_use]
    pub fn with_evaluator(mut self, evaluator: Option<Arc<dyn JsEvaluator>>) -> Scoped<'a> {
        self.evaluator = evaluator;
        self
    }

    /// The catalog: the schema, the configuration, the services. Its rows are
    /// reached through this handle's operations, or through an [`Unchecked`]
    /// token somebody had to make.
    pub fn catalog(&self) -> &'a Catalog {
        self.catalog
    }

    /// Whose authority the operations run under.
    pub fn who(&self) -> Authority<'a> {
        self.who
    }

    /// The caller, or `None` for the system.
    pub fn principal(&self) -> Option<&'a Principal> {
        self.who.principal()
    }

    /// The role access is checked at (the administrator's for the system).
    pub fn role(&self) -> Role {
        self.who.role()
    }

    /// Where the statements run.
    pub fn executor(&self) -> &Executor {
        &self.executor
    }

    /// The formula engine a reified ownership check runs on.
    pub fn evaluator(&self) -> Option<&Arc<dyn JsEvaluator>> {
        self.evaluator.as_ref()
    }

    /// This handle's authority as an [`Unchecked`] token, when it is the
    /// system's or an administrator's — for the operations this handle does not
    /// wrap (a CSV import, a schema check). `None` for any other caller.
    pub fn unchecked(&self) -> Option<Unchecked<'a>> {
        self.who.unchecked()
    }

    /// The table named `name`.
    pub fn table(&self, name: &str) -> Result<Table> {
        self.catalog.require(name)
    }

    fn decided(&self) -> Result<Decided<'a>> {
        if let Some(unchecked) = self.who.unchecked() {
            return Ok(Decided::Unchecked(unchecked));
        }
        let Some(principal) = self.who.principal() else {
            // `unchecked` answers for every system authority.
            return Err(Error::msg("a system authority without an unchecked token"));
        };
        let user = User::from_json(principal.role, principal.user.as_ref())?;
        Ok(Decided::Checked(principal, user))
    }

    /// The rows of `table` `query` asks for that this authority may read, as
    /// typed values (with the query's extra projections).
    pub async fn read_values(
        &self,
        table: &Table,
        query: &RowQuery,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        match self.decided()? {
            Decided::Unchecked(access) => {
                if let Some(search) = &query.search {
                    crate::search::require_answerable(self.catalog, table, search).await?;
                }
                rows::list_row_values_in(self.catalog, table, query, access, &self.executor).await
            }
            Decided::Checked(principal, user) => {
                ownership::read_row_values_by(
                    self.catalog,
                    table,
                    query,
                    principal,
                    user.as_ref(),
                    self.evaluator.as_ref(),
                    &self.executor,
                )
                .await
            }
        }
    }

    /// [`read_values`](Scoped::read_values) as the JSON rows a client is sent:
    /// a secret field is its sentinel.
    pub async fn read(&self, table: &Table, query: &RowQuery) -> Result<Json> {
        let values = self.read_values(table, query).await?;
        Ok(Json::Array(
            values
                .into_iter()
                .map(|mut values| {
                    sc_catalog::secrets::redact_values(table, &mut values);
                    ownership::table_row_json(table, &values)
                })
                .collect(),
        ))
    }

    /// Insert `body` into `table`, answering the inserted row.
    pub async fn insert(&self, table: &Table, body: &Json) -> Result<Json> {
        match self.decided()? {
            Decided::Unchecked(access) => {
                rows::create_row_in(self.catalog, table, body, access, &self.executor).await
            }
            Decided::Checked(principal, user) => {
                ownership::insert_row_by(
                    self.catalog,
                    table,
                    body,
                    principal,
                    user.as_ref(),
                    self.evaluator.as_ref(),
                    &self.executor,
                )
                .await
            }
        }
    }

    /// Update the row of `table` keyed `id` with `body`, answering the updated
    /// row. A row this authority may not reach is the same not-found a missing
    /// row is.
    pub async fn update(&self, table: &Table, id: &str, body: &Json) -> Result<Json> {
        match self.decided()? {
            Decided::Unchecked(access) => {
                rows::update_row_in(self.catalog, table, id, body, access, &self.executor).await
            }
            Decided::Checked(principal, user) => {
                ownership::update_row_by(
                    self.catalog,
                    table,
                    id,
                    body,
                    principal,
                    user.as_ref(),
                    self.evaluator.as_ref(),
                    &self.executor,
                )
                .await
            }
        }
    }

    /// Delete the row of `table` keyed `id`, answering the deleted row.
    pub async fn delete(&self, table: &Table, id: &str) -> Result<Json> {
        match self.decided()? {
            Decided::Unchecked(access) => {
                rows::delete_row_in(self.catalog, table, id, access, &self.executor).await
            }
            Decided::Checked(principal, user) => {
                ownership::delete_row_by(
                    self.catalog,
                    table,
                    id,
                    principal,
                    user.as_ref(),
                    self.evaluator.as_ref(),
                    &self.executor,
                )
                .await
            }
        }
    }
}
