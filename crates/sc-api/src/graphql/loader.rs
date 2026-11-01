//! Child lists, batched: one `SELECT … WHERE key IN (…)` per relation per
//! level, never one per parent.
//!
//! A GraphQL selection like
//!
//! ```graphql
//! { departments { name employees(limit: 3) { name } } }
//! ```
//!
//! resolves the `employees` field once per department row, and the naive
//! reading of that is a query per department — the N+1 problem the design
//! (§6) answers with `async_graphql::dataloader::DataLoader`. The loader
//! collects the sibling loads issued while the executor works through the
//! parents and answers them together.
//!
//! **What makes two loads the same batch** is the [`ChildKey`]: the relation,
//! the field's own arguments and the parent's key. Two siblings under one
//! selection share a relation and arguments, so they differ only in the parent
//! and collapse into one read; the *same* relation asked twice under different
//! `where` arguments does not, and gets its own read, because it is a different
//! question.
//!
//! **The child's rules are the child's.** The read is
//! [`ownership::read_row_values_as`] over the *child* table, so its role floor,
//! its ownership formula and its RLS routing decide — the parent's permission
//! to be read says nothing about the child, and a caller who may not read the
//! child table gets an error on that field with the parents still returned.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_graphql::dataloader::Loader;
use sc_error::{Error, Result};
use sc_query::{Expr, InSet, Value};

use super::context::RequestContext;
use super::resolve::RowValue;
use sc_access::ownership;
use sc_access::rows::{self, RowQuery};

/// One child-list read, minus the parents it is for: the relation it follows
/// and the query the field's arguments lowered to.
///
/// Every sibling builds an identical one, and they are recognised as identical
/// by [`fingerprint`](ChildRequest::fingerprint) rather than by pointer — the
/// resolvers cannot share a value, since each is a separate call.
#[derive(Debug)]
pub struct ChildRequest {
    /// The child table being read.
    child_table: String,
    /// The child's key column — the one referencing the parent.
    key_field: String,
    /// The caller's `where`/`order_by`, their per-parent bound as a
    /// [`Partition`](sc_access::rows::Partition), and the Ⱶ-join projections their
    /// selection set implies. The `key IN (…)` is the loader's to add.
    query: RowQuery,
    /// What "the same request" means, as text.
    ///
    /// A `RowQuery` is not `Hash` (it carries an `Expr`, which carries floats
    /// and decimals), so the batch key is the query's `Debug` rendering — a
    /// structural, deterministic description of exactly the fields that decide
    /// whether one read can answer both loads.
    fingerprint: String,
}

impl ChildRequest {
    /// The request for one child-list field's arguments.
    pub fn new(
        child_table: impl Into<String>,
        key_field: impl Into<String>,
        query: RowQuery,
    ) -> ChildRequest {
        let (child_table, key_field) = (child_table.into(), key_field.into());
        let fingerprint = format!("{child_table}\u{1}{key_field}\u{1}{query:?}");
        ChildRequest {
            child_table,
            key_field,
            query,
            fingerprint,
        }
    }
}

/// One parent's children under one [`ChildRequest`] — the unit the
/// [`DataLoader`](async_graphql::dataloader::DataLoader) batches.
#[derive(Clone, Debug)]
pub struct ChildKey {
    /// The read this load belongs to.
    request: Arc<ChildRequest>,
    /// The parent row's value of the column the child key references.
    parent: Value,
    /// That value as the text the loader buckets by (see
    /// [`rows::group_key`]) — and, with the request's fingerprint, what this
    /// key hashes and compares as.
    parent_key: String,
}

impl ChildKey {
    /// The load of `parent`'s children under `request`.
    pub fn new(request: Arc<ChildRequest>, parent: Value) -> ChildKey {
        let parent_key = rows::group_key(Some(&parent));
        ChildKey {
            request,
            parent,
            parent_key,
        }
    }
}

impl PartialEq for ChildKey {
    fn eq(&self, other: &ChildKey) -> bool {
        self.request.fingerprint == other.request.fingerprint && self.parent_key == other.parent_key
    }
}

impl Eq for ChildKey {}

impl Hash for ChildKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.request.fingerprint.hash(state);
        self.parent_key.hash(state);
    }
}

/// The loader behind every child-list field of one request.
///
/// It holds the request's own [`RequestContext`] — the same catalog, the same
/// caller, the same evaluator — because a batched read is not a different kind
/// of read: it goes through the identical entry point with the identical
/// caller, and only the *shape* of the statement differs.
pub struct ChildLoader {
    ctx: RequestContext,
}

impl ChildLoader {
    /// The loader for one request.
    pub fn new(ctx: RequestContext) -> ChildLoader {
        ChildLoader { ctx }
    }

    /// One relation's read: every parent in `keys` answered by one statement.
    async fn load_relation(
        &self,
        keys: &[&ChildKey],
        out: &mut HashMap<ChildKey, Arc<Vec<RowValue>>>,
    ) -> Result<()> {
        let Some(first) = keys.first() else {
            return Ok(());
        };
        let request = &first.request;
        let table = self.ctx.table(&request.child_table)?;
        // One batch is one statement, however many parents asked for it —
        // which is exactly why the budget is charged here and not in the
        // resolver that queued the load.
        self.ctx.charge(&request.child_table)?;

        // One membership test over the distinct parents, so a parent that two
        // fields of the document asked about is still one bound value.
        let mut seen: BTreeMap<&str, &Value> = BTreeMap::new();
        for key in keys {
            seen.insert(&key.parent_key, &key.parent);
        }
        let query = request.query.clone().and_filter(Expr::In {
            e: Box::new(Expr::col(request.key_field.clone())),
            set: InSet::List(seen.values().map(|v| Expr::lit((*v).clone())).collect()),
        });

        let rows = ownership::read_row_values_as(
            &self.ctx.catalog,
            &table,
            &query,
            self.ctx.role(),
            self.ctx.user(),
            self.ctx.evaluator(),
        )
        .await?;

        // Without a per-parent bound the cap is shared between the parents, so
        // reaching it means somebody's list was cut short and nobody can tell
        // whose. Saying so is the only honest answer.
        if request.query.partition.is_none() && rows.len() as u64 >= self.ctx.limits.row_cap {
            return Err(Error::invalid(format!(
                "reading `{}` for {} parent rows reached this application's row cap of {}; \
                 give the field a `limit`, which bounds each parent's list on its own",
                request.child_table,
                seen.len(),
                self.ctx.limits.row_cap
            )));
        }

        let mut buckets: HashMap<String, Vec<RowValue>> = HashMap::new();
        for values in rows {
            let bucket = rows::group_key(values.get(&request.key_field));
            buckets
                .entry(bucket)
                .or_default()
                .push(RowValue::new(&table.name, values));
        }
        let buckets: HashMap<String, Arc<Vec<RowValue>>> =
            buckets.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        for key in keys {
            // A parent with no children is an empty list, not a missing one.
            let rows = buckets.get(&key.parent_key).cloned().unwrap_or_default();
            out.insert((*key).clone(), rows);
        }
        Ok(())
    }
}

impl Loader<ChildKey> for ChildLoader {
    type Value = Arc<Vec<RowValue>>;
    /// Shared because the loader answers every key of a failed batch with the
    /// same error, and `Loader::Error` must be `Clone`.
    type Error = Arc<Error>;

    async fn load(&self, keys: &[ChildKey]) -> Result<HashMap<ChildKey, Self::Value>, Self::Error> {
        // The batch may hold more than one relation — two child lists under one
        // parent, or one relation asked twice under different arguments — so it
        // is grouped first. One statement per group, still never one per parent.
        let mut groups: BTreeMap<&str, Vec<&ChildKey>> = BTreeMap::new();
        for key in keys {
            groups
                .entry(&key.request.fingerprint)
                .or_default()
                .push(key);
        }
        let mut out = HashMap::new();
        for group in groups.values() {
            self.load_relation(group, &mut out)
                .await
                .map_err(Arc::new)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_access::rows::Partition;

    fn request(table: &str, key: &str, query: RowQuery) -> Arc<ChildRequest> {
        Arc::new(ChildRequest::new(table, key, query))
    }

    #[test]
    fn siblings_of_one_field_share_a_batch_and_differ_only_by_parent() {
        // The whole point: two parents, one read. The keys are equal as far as
        // the request goes and distinct as far as the parent goes.
        let a = ChildKey::new(
            request("employees", "department", RowQuery::new()),
            Value::Int(1),
        );
        let b = ChildKey::new(
            request("employees", "department", RowQuery::new()),
            Value::Int(2),
        );
        assert_eq!(a.request.fingerprint, b.request.fingerprint);
        assert_ne!(a, b);
        assert_eq!(
            a,
            ChildKey::new(
                request("employees", "department", RowQuery::new()),
                Value::Int(1)
            )
        );
    }

    #[test]
    fn the_same_relation_under_different_arguments_is_a_different_read() {
        // Otherwise one `where` would answer the other's question.
        let plain = ChildKey::new(
            request("employees", "department", RowQuery::new()),
            Value::Int(1),
        );
        let filtered = ChildKey::new(
            request(
                "employees",
                "department",
                RowQuery::new().where_(Some(Expr::col("active").eq(Expr::lit(true)))),
            ),
            Value::Int(1),
        );
        assert_ne!(plain, filtered);

        // …and so is the same relation with a different per-parent bound.
        let bounded = ChildKey::new(
            request(
                "employees",
                "department",
                RowQuery::new().per_partition(Partition {
                    by: "department".into(),
                    limit: Some(3),
                    offset: None,
                }),
            ),
            Value::Int(1),
        );
        assert_ne!(plain, bounded);
    }

    #[test]
    fn a_parents_key_is_grouped_by_type_as_well_as_by_text() {
        // `1` and `"1"` are different parents, and a bucket that collapsed them
        // would hand one parent the other's children.
        let int = ChildKey::new(
            request("employees", "department", RowQuery::new()),
            Value::Int(1),
        );
        let text = ChildKey::new(
            request("employees", "department", RowQuery::new()),
            Value::Text("1".into()),
        );
        assert_ne!(int, text);
    }
}
