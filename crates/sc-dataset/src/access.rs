//! Reading a dataset **as its caller** (analytics TODO A9.1; §7.3).
//!
//! A dataset compiles to one statement, and that statement can read many
//! tables: the base, a Join's or a Union's other side, the target of a `Ⱶ`
//! path, the children a `Ↄ` aggregation counts, the rows a Complete takes its
//! values from. Each of them has to be read under §7.3's rule — *meets the
//! table's `min_role_read`, or its ownership formula grants the row* — and the
//! operations that build the statement have no business knowing that.
//!
//! So the rule is applied to the statement **after** it is built, in one place:
//! [`Reader::guard`] walks it and replaces every table it reads with the rows of
//! that table the caller may read — `(SELECT * FROM t AS a WHERE <formula>) AS
//! a` — under the alias the statement already reads it by, so nothing else in
//! the statement changes. The formula is the table's ownership formula
//! translated for this caller, the same translation the REST provider ANDs into
//! its reads, so a histogram counts exactly the rows a list would have listed.
//! Because the replacement is a derived table rather than a `WHERE`, it reaches
//! the places a `WHERE` cannot: a `Ⱶ` join's correlated subquery reads the
//! target through it too, so a joined value the caller may not see is missing,
//! as it is under row-level security.
//!
//! The answers for one table, in order:
//!
//! - **Not one of the application's tables** (`Caller::tables`): refused. This
//!   is how a self-serve application's table subset is enforced on reads — at
//!   the statement, whatever the client sent (A9.4).
//! - **Row-level security** on the table: the database decides, inside a
//!   transaction that carries the caller ([`sc_catalog::set_caller_context`]).
//! - **The caller meets `min_role_read`**: read as it is.
//! - **An ownership formula**: the rows it grants. A formula only the
//!   JavaScript evaluator can decide is refused, as an aggregate is in the REST
//!   provider: a dataset is counted, binned and summed by the database, and a
//!   number computed over rows nobody could filter is a leak that a plausible
//!   number hides.
//! - Otherwise: refused, naming the table.
//!
//! The users table is never read with its password hash, by anybody but the
//! admin: the derived table names its other columns one by one.
//!
//! A refusal is an error naming the table — a person reads it under the plot
//! that did not draw — and it is the same whether the table is the base or
//! three joins away.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sc_auth::{COL_ID, COL_PASSWORD_HASH, COL_ROLE, ROLE_ADMIN, ROLE_PUBLIC, USERS_TABLE, User};
use sc_catalog::{CallerContext, Catalog, Table};
use sc_db::{DatabaseDriver, Row};
use sc_error::{Error, Result};
use sc_expr::{Env, Operation, SchemaShape, TranslateError, UserEnv, translate_rooted};
use sc_query::{Expr, InSet, Join, OrderBy, Projection, Select, Source, Statement, Value};
use serde_json::Value as Json;

/// Who reads: a role, the user's fields an ownership formula reads, and — in
/// an application — the tables it may read at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    /// The role: the user's, or public for nobody.
    pub role: u8,
    /// The user, as the formula's `user` sees it: `id`, `role` and every other
    /// column. `None` for nobody, and for the server reading on its own
    /// authority.
    pub user: Option<BTreeMap<String, Value>>,
    /// The tables an application lets its users read (A9.4); `None` is every
    /// table, which is the unrestricted Analytics UI.
    pub tables: Option<Arc<BTreeSet<String>>>,
}

impl Caller {
    /// The server itself, at the admin role and with no user: what a model fit
    /// reads its training rows as (fits are the admin's to start), and what a
    /// test reads as.
    pub fn admin() -> Caller {
        Caller {
            role: ROLE_ADMIN,
            user: None,
            tables: None,
        }
    }

    /// The caller a request's user is, or nobody's (the public role).
    pub fn of_user(user: Option<&User>) -> Caller {
        Caller {
            role: user.map_or(ROLE_PUBLIC, |u| u.role),
            user: user.map(|u| {
                let mut fields = u.extra.clone();
                fields.insert(COL_ID.to_owned(), Value::Uuid(u.id));
                fields.insert(COL_ROLE.to_owned(), Value::Int(i64::from(u.role)));
                fields
            }),
            tables: None,
        }
    }

    /// The same caller, reading only `tables`.
    #[must_use]
    pub fn within(mut self, tables: impl IntoIterator<Item = String>) -> Caller {
        self.tables = Some(Arc::new(tables.into_iter().collect()));
        self
    }

    /// Whether this is the admin role.
    pub fn is_admin(&self) -> bool {
        self.role <= ROLE_ADMIN
    }

    /// The user's id, if a user is reading.
    pub fn user_id(&self) -> Option<uuid::Uuid> {
        match self.user.as_ref()?.get(COL_ID)? {
            Value::Uuid(id) => Some(*id),
            _ => None,
        }
    }

    /// Whether `table` is one this caller's application lets it read.
    pub fn may_name(&self, table: &str) -> bool {
        self.tables.as_ref().is_none_or(|t| t.contains(table))
    }

    /// The context a row-level-security policy reads the caller from.
    pub fn context(&self) -> CallerContext {
        let user = self.user.as_ref().map(|fields| {
            Json::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), sc_types::value_to_json(v)))
                    .collect(),
            )
        });
        CallerContext::new(self.role, user)
    }
}

/// Who owns a dataset or a workspace, and the roles it is shared with
/// (analytics TODO A9.1).
///
/// Sharing is by **role floor**, the one way this system says "these people":
/// shared with role 40, a thing is seen by every user whose role is 40 or more
/// privileged, as a table's `min_role_read` admits them. Only its owner and the
/// admin change it, rename it, delete it or share it. A thing nobody owns was
/// made before owners existed, by the admin, and is the admin's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sharing {
    /// The user who made it; `None` for the admin's.
    pub owner: Option<uuid::Uuid>,
    /// The least privileged role it is shared with; `None` for nobody but its
    /// owner.
    pub share_role: Option<u8>,
}

impl Sharing {
    /// Owned by `owner`, shared with nobody.
    pub fn owned_by(owner: Option<uuid::Uuid>) -> Sharing {
        Sharing {
            owner,
            share_role: None,
        }
    }

    /// Whether `caller` sees it: the admin, its owner, or a role it is shared
    /// with.
    pub fn admits(&self, caller: &Caller) -> bool {
        caller.is_admin()
            || (self.owner.is_some() && self.owner == caller.user_id())
            || self.share_role.is_some_and(|floor| caller.role <= floor)
    }

    /// Whether `caller` may change it: the admin, or its owner.
    pub fn may_change(&self, caller: &Caller) -> bool {
        caller.is_admin() || (self.owner.is_some() && self.owner == caller.user_id())
    }

    /// Refuse a role to share with that is not one.
    pub fn check(&self) -> Result<()> {
        match self.share_role {
            Some(role) if !(ROLE_ADMIN..=ROLE_PUBLIC).contains(&role) => Err(Error::invalid(
                format!("{role} is not a role to share with: roles run from 1 to 100"),
            )),
            _ => Ok(()),
        }
    }
}

/// The primary database as one caller may read it: every statement it runs is
/// [guarded](Reader::guard) first.
///
/// Holds what the guard needs — the tables' access rules and the formula shape —
/// as a snapshot rather than a reference to the catalog, so a renderer can keep
/// one for the length of a render without borrowing it.
#[derive(Clone)]
pub struct Reader {
    inner: Arc<ReaderInner>,
}

struct ReaderInner {
    db: Arc<dyn DatabaseDriver>,
    caller: Caller,
    tables: BTreeMap<String, Table>,
    shape: SchemaShape,
    row_level_security: bool,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("caller", &self.inner.caller)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// `caller`'s view of the catalog's primary database, as the catalog is now.
    pub fn new(catalog: &Catalog, caller: &Caller) -> Result<Reader> {
        let db = Arc::clone(catalog.primary());
        let row_level_security = db.capabilities().row_level_security;
        Ok(Reader {
            inner: Arc::new(ReaderInner {
                db,
                caller: caller.clone(),
                tables: catalog
                    .tables()?
                    .into_iter()
                    .map(|t| (t.name.clone(), t))
                    .collect(),
                shape: catalog.schema_shape()?,
                row_level_security,
            }),
        })
    }

    /// Who reads.
    pub fn caller(&self) -> &Caller {
        &self.inner.caller
    }

    /// Replace every table `select` reads with the rows of it the caller may
    /// read (see the module docs). Answers whether the statement must run in a
    /// transaction that carries the caller, because a table it reads has
    /// row-level security.
    pub fn guard(&self, select: &mut Select) -> Result<bool> {
        let mut in_context = false;
        visit_select(select, &mut |source| {
            let Source::Table { name, alias } = source else {
                return Ok(false);
            };
            let root = alias.clone().unwrap_or_else(|| name.clone());
            let read = self.rows_of(name, &root)?;
            in_context |= read.in_context;
            match read.rows {
                Some(rows) => {
                    *source = Source::Subquery {
                        query: Box::new(rows),
                        alias: root,
                    };
                    Ok(true)
                }
                None => Ok(false),
            }
        })?;
        Ok(in_context)
    }

    /// Guard `select` and run it.
    pub async fn query(&self, mut select: Select) -> Result<Vec<Row>> {
        let in_context = self.guard(&mut select)?;
        self.run_guarded(Statement::from(select), in_context).await
    }

    /// Run a statement [`guard`](Self::guard) has already rewritten.
    async fn run_guarded(&self, statement: Statement, in_context: bool) -> Result<Vec<Row>> {
        let db = &self.inner.db;
        if !(in_context && self.inner.row_level_security) {
            return db.query(&statement).await?.try_collect().await;
        }
        let mut tx = db.begin().await?;
        tx.set_read_only().await?;
        sc_catalog::set_caller_context(tx.as_mut(), &self.inner.caller.context()).await?;
        let outcome = match tx.query(&statement).await {
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
                Err(e)
            }
        }
    }

    /// What reading `name` under the alias `root` means for this caller.
    fn rows_of(&self, name: &str, root: &str) -> Result<TableRead> {
        let caller = &self.inner.caller;
        if !caller.may_name(name) {
            return Err(Error::auth(format!(
                "`{name}` is not one of the tables this application reads"
            )));
        }
        let Some(table) = self.inner.tables.get(name) else {
            // Not a table of the catalog: nothing a dataset compiles to, so
            // only the server reading on its own authority gets past here.
            return if caller.is_admin() {
                Ok(TableRead::as_it_is())
            } else {
                Err(Error::auth(format!("you may not read `{name}`")))
            };
        };
        let hide_password = name == USERS_TABLE && !caller.is_admin();
        let only = |filter: Option<Expr>| -> Option<Select> {
            if filter.is_none() && !hide_password {
                return None;
            }
            let mut rows = Select::from(Source::table_as(name, root));
            if hide_password {
                rows = rows.columns(
                    table
                        .fields
                        .iter()
                        .filter(|f| f.base.name != COL_PASSWORD_HASH)
                        .map(|f| Projection::expr(Expr::qcol(root, f.base.name.clone())))
                        .collect(),
                );
            }
            if let Some(filter) = filter {
                rows = rows.filter(filter);
            }
            Some(rows)
        };
        // The policies decide, in a transaction that carries the caller; the
        // password is still left out.
        if table.rls_enabled {
            return Ok(TableRead {
                rows: only(None),
                in_context: true,
            });
        }
        if caller.role <= table.access.min_role_read {
            return Ok(TableRead {
                rows: only(None),
                in_context: false,
            });
        }
        let Some(formula) = &table.ownership else {
            return Err(Error::auth(format!("you may not read `{name}`")));
        };
        let user = UserEnv::Inline(caller.user.clone());
        let calc = table.calc_formulas();
        match translate_rooted(
            formula,
            Operation::Read,
            &Env::new(&user).with_calc(&calc),
            &self.inner.shape,
            name,
            root,
        ) {
            Ok(pred) => Ok(TableRead {
                rows: only(Some(pred)),
                in_context: false,
            }),
            Err(TranslateError::Untranslatable(what)) => Err(Error::invalid(format!(
                "`{name}` cannot be analysed by you: its ownership formula ({what}) has to be \
                 decided row by row, and a count, a sum or a plot over rows nobody could filter \
                 would show rows you may not read"
            ))),
            Err(e) => Err(e.into()),
        }
    }
}

/// What a caller reads of one table.
struct TableRead {
    /// The rows to read in its place, as a derived table under its alias;
    /// `None` reads the table as it is.
    rows: Option<Select>,
    /// Whether the database's own policies decide, in a transaction that
    /// carries the caller.
    in_context: bool,
}

impl TableRead {
    fn as_it_is() -> TableRead {
        TableRead {
            rows: None,
            in_context: false,
        }
    }
}

/// The tables `select` reads, anywhere in it.
pub fn tables_read(select: &Select) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut copy = select.clone();
    // The visitor never fails and replaces nothing here.
    let _ = visit_select(&mut copy, &mut |source| {
        if let Source::Table { name, .. } = source {
            out.insert(name.clone());
        }
        Ok(false)
    });
    out
}

/// Call `f` on every source `select` reads from, at any depth: its `FROM`, its
/// joins, unions, derived tables and the subqueries inside its expressions.
/// `f` answers whether it replaced the source, in which case the replacement is
/// not descended into.
fn visit_select(select: &mut Select, f: &mut dyn FnMut(&mut Source) -> Result<bool>) -> Result<()> {
    visit_source(&mut select.from, f)?;
    for Join { source, on, .. } in &mut select.joins {
        visit_source(source, f)?;
        if let Some(on) = on {
            visit_expr(on, f)?;
        }
    }
    for p in &mut select.columns {
        if let Projection::Expr { expr, .. } = p {
            visit_expr(expr, f)?;
        }
    }
    for e in select
        .filter
        .iter_mut()
        .chain(select.group.iter_mut())
        .chain(select.having.iter_mut())
    {
        visit_expr(e, f)?;
    }
    for OrderBy { expr, .. } in &mut select.order {
        visit_expr(expr, f)?;
    }
    Ok(())
}

fn visit_source(source: &mut Source, f: &mut dyn FnMut(&mut Source) -> Result<bool>) -> Result<()> {
    if f(source)? {
        return Ok(());
    }
    match source {
        Source::Subquery { query, .. } | Source::Lateral { query, .. } => visit_select(query, f),
        Source::UnionAll { parts, .. } => {
            for part in parts {
                visit_select(part, f)?;
            }
            Ok(())
        }
        Source::Table { .. } | Source::Nothing => Ok(()),
    }
}

/// The subqueries inside `e`: a `Ⱶ` path's, a `Ↄ` aggregation's, an `IN`'s.
fn visit_expr(e: &mut Expr, f: &mut dyn FnMut(&mut Source) -> Result<bool>) -> Result<()> {
    let mut failed: Option<Error> = None;
    crate::walk::rewrite(e, &mut |node| {
        if failed.is_some() {
            return None;
        }
        let rebuilt = match node {
            Expr::Subquery(q) => {
                let mut q = (**q).clone();
                visit_select(&mut q, f).map(|()| Expr::Subquery(Box::new(q)))
            }
            Expr::In {
                e: inner,
                set: InSet::Subquery(q),
            } => {
                let mut q = (**q).clone();
                let mut inner = (**inner).clone();
                visit_select(&mut q, f)
                    .and_then(|()| visit_expr(&mut inner, f))
                    .map(|()| Expr::In {
                        e: Box::new(inner),
                        set: InSet::Subquery(Box::new(q)),
                    })
            }
            _ => return None,
        };
        match rebuilt {
            Ok(new) => Some(new),
            Err(err) => {
                failed = Some(err);
                None
            }
        }
    });
    match failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_a_statement_reads_is_found_at_any_depth() {
        let joined = Select::from(Source::table_as("hoods", "_fd_j1"))
            .columns(vec![Projection::expr(Expr::qcol("_fd_j1", "name"))])
            .filter(Expr::qcol("_fd_j1", "id").eq(Expr::qcol("_fd_b", "hood")));
        let keys = Select::from(Source::table("owners"))
            .columns(vec![Projection::expr(Expr::qcol("owners", "id"))]);
        let inner = Select::from(Source::table_as("houses", "_fd_b"))
            .columns(vec![Projection::expr_as(
                Expr::Subquery(Box::new(joined)),
                "hood_name",
            )])
            .filter(Expr::In {
                e: Box::new(Expr::qcol("_fd_b", "owner")),
                set: InSet::Subquery(Box::new(keys)),
            });
        let outer = Select::from(Source::union_all(
            vec![inner, Select::from(Source::table("extra"))],
            "_fd_u",
        ));
        assert_eq!(
            tables_read(&outer),
            BTreeSet::from(["extra", "hoods", "houses", "owners"].map(str::to_owned))
        );
    }

    #[test]
    fn a_thing_is_seen_by_its_owner_the_roles_it_is_shared_with_and_the_admin() {
        let alice = User::new(uuid::Uuid::new_v4(), 40).unwrap();
        let bob = User::new(uuid::Uuid::new_v4(), 80).unwrap();
        let (alice, bob) = (Caller::of_user(Some(&alice)), Caller::of_user(Some(&bob)));
        let private = Sharing::owned_by(alice.user_id());
        assert!(private.admits(&alice) && private.may_change(&alice));
        assert!(!private.admits(&bob) && !private.may_change(&bob));
        assert!(private.admits(&Caller::admin()) && private.may_change(&Caller::admin()));
        assert!(!private.admits(&Caller::of_user(None)));
        // Shared with role 80: bob sees it and still may not change it.
        let shared = Sharing {
            share_role: Some(80),
            ..private
        };
        assert!(shared.admits(&bob) && !shared.may_change(&bob));
        // Shared with role 40 is not shared with 80.
        let staff = Sharing {
            share_role: Some(40),
            ..private
        };
        assert!(!staff.admits(&bob));
        // The admin's own, from before owners: nobody else's to change.
        let old = Sharing::default();
        assert!(!old.admits(&alice) && !old.may_change(&alice));
        assert!(
            Sharing {
                share_role: Some(0),
                ..old
            }
            .check()
            .is_err()
        );
        assert!(
            Sharing {
                share_role: Some(100),
                ..old
            }
            .check()
            .is_ok()
        );
    }

    #[test]
    fn a_caller_knows_its_user_and_its_tables() {
        let user = User::new(uuid::Uuid::new_v4(), 40).unwrap();
        let caller = Caller::of_user(Some(&user)).within(["houses".to_owned()]);
        assert_eq!(caller.role, 40);
        assert_eq!(caller.user_id(), Some(user.id));
        assert!(caller.may_name("houses"));
        assert!(!caller.may_name("incidents"));
        assert!(!caller.is_admin());
        let nobody = Caller::of_user(None);
        assert_eq!(nobody.role, ROLE_PUBLIC);
        assert!(nobody.user.is_none());
        assert!(nobody.may_name("anything"));
        assert!(Caller::admin().is_admin());
        // The policy context carries the user's fields as JSON.
        let context = caller.context();
        assert_eq!(context.role, 40);
        assert_eq!(
            context
                .user
                .as_ref()
                .and_then(|u| u.get("id"))
                .and_then(Json::as_str),
            Some(user.id.to_string().as_str())
        );
    }
}
