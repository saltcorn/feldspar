//! Walking an `sc_query` tree: what the compiler needs to merge an operation
//! into the query before it rather than nesting it one level deeper.
//!
//! A formula is translated as though it read its row from an alias, `_fd_row`;
//! merging it is replacing each `"_fd_row"."price"` with the expression the
//! stage computes `price` by. That replacement has to reach every position a
//! column can appear in — inside a correlated subquery a join path became, an
//! aggregation's filter, a `CASE` — so it is written once, here.

use std::collections::BTreeSet;

use sc_query::{CaseArm, ColRef, Expr, InSet, Join, OrderBy, Projection, Select, Source};

/// Rewrite `e` bottom-up wherever `f` answers a replacement: a node `f`
/// replaces is not descended into.
pub(crate) fn rewrite(e: &mut Expr, f: &mut dyn FnMut(&Expr) -> Option<Expr>) {
    if let Some(new) = f(e) {
        *e = new;
        return;
    }
    match e {
        Expr::Col(_) | Expr::Lit(_) | Expr::Param(_) => {}
        Expr::Binary { l, r, .. } => {
            rewrite(l, f);
            rewrite(r, f);
        }
        Expr::Unary { e, .. } => rewrite(e, f),
        Expr::Func { args, .. } | Expr::Agg { args, .. } => {
            for a in args {
                rewrite(a, f);
            }
        }
        Expr::Window {
            args,
            partition,
            order,
            ..
        } => {
            for a in args.iter_mut().chain(partition.iter_mut()) {
                rewrite(a, f);
            }
            for o in order {
                rewrite(&mut o.expr, f);
            }
        }
        Expr::In { e, set } => {
            rewrite(e, f);
            match set {
                InSet::List(items) => {
                    for i in items {
                        rewrite(i, f);
                    }
                }
                InSet::Subquery(q) => rewrite_select(q, f),
            }
        }
        Expr::Json { target, .. } => rewrite(target, f),
        Expr::Case {
            operand,
            arms,
            else_result,
        } => {
            if let Some(o) = operand {
                rewrite(o, f);
            }
            for CaseArm { when, then } in arms {
                rewrite(when, f);
                rewrite(then, f);
            }
            if let Some(e) = else_result {
                rewrite(e, f);
            }
        }
        Expr::Subquery(q) => rewrite_select(q, f),
        Expr::Cast { expr, .. } => rewrite(expr, f),
    }
}

/// [`rewrite`] over every expression of a select.
pub(crate) fn rewrite_select(s: &mut Select, f: &mut dyn FnMut(&Expr) -> Option<Expr>) {
    rewrite_source(&mut s.from, f);
    for p in &mut s.columns {
        if let Projection::Expr { expr, .. } = p {
            rewrite(expr, f);
        }
    }
    for Join { source, on, .. } in &mut s.joins {
        rewrite_source(source, f);
        if let Some(on) = on {
            rewrite(on, f);
        }
    }
    for e in s
        .filter
        .iter_mut()
        .chain(s.group.iter_mut())
        .chain(s.having.iter_mut())
    {
        rewrite(e, f);
    }
    for OrderBy { expr, .. } in &mut s.order {
        rewrite(expr, f);
    }
}

fn rewrite_source(src: &mut Source, f: &mut dyn FnMut(&Expr) -> Option<Expr>) {
    match src {
        Source::Subquery { query, .. } | Source::Lateral { query, .. } => rewrite_select(query, f),
        Source::UnionAll { parts, .. } => {
            for p in parts {
                rewrite_select(p, f);
            }
        }
        Source::Table { .. } | Source::Nothing => {}
    }
}

/// The columns of `alias` that `e` reads, anywhere inside it.
pub(crate) fn columns_of(e: &Expr, alias: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut copy = e.clone();
    rewrite(&mut copy, &mut |node| {
        if let Expr::Col(ColRef {
            table: Some(t),
            column,
        }) = node
            && t == alias
        {
            out.insert(column.clone());
        }
        None
    });
    out
}

/// Whether `e` holds a literal (a bound placeholder) anywhere inside it.
pub(crate) fn has_literal(e: &Expr) -> bool {
    let mut found = false;
    let mut copy = e.clone();
    rewrite(&mut copy, &mut |node| {
        found |= matches!(node, Expr::Lit(_) | Expr::Param(_));
        None
    });
    found
}

/// `e` with every column of `alias` replaced by what `lookup` says it is.
pub(crate) fn substitute(e: &mut Expr, alias: &str, lookup: &dyn Fn(&str) -> Option<Expr>) {
    rewrite(e, &mut |node| match node {
        Expr::Col(ColRef {
            table: Some(t),
            column,
        }) if t == alias => lookup(column),
        _ => None,
    });
}

/// Whether `e` calls a window or an aggregate function **at its own level** —
/// not inside a subquery, which is a scope of its own. Such an expression
/// cannot go into a `WHERE`, a `GROUP BY` or another window, so an operation
/// that would put it there reads the stage as a subquery instead.
pub(crate) fn has_window_or_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Window { .. } | Expr::Agg { .. } => true,
        Expr::Col(_) | Expr::Lit(_) | Expr::Param(_) | Expr::Subquery(_) => false,
        Expr::Binary { l, r, .. } => has_window_or_aggregate(l) || has_window_or_aggregate(r),
        Expr::Unary { e, .. } | Expr::Json { target: e, .. } | Expr::Cast { expr: e, .. } => {
            has_window_or_aggregate(e)
        }
        Expr::Func { args, .. } => args.iter().any(has_window_or_aggregate),
        Expr::In { e, set } => {
            has_window_or_aggregate(e)
                || match set {
                    InSet::List(items) => items.iter().any(has_window_or_aggregate),
                    InSet::Subquery(_) => false,
                }
        }
        Expr::Case {
            operand,
            arms,
            else_result,
        } => {
            operand.as_deref().is_some_and(has_window_or_aggregate)
                || arms
                    .iter()
                    .any(|a| has_window_or_aggregate(&a.when) || has_window_or_aggregate(&a.then))
                || else_result.as_deref().is_some_and(has_window_or_aggregate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_reaches_into_a_correlated_subquery() {
        let sub = Select::from(Source::table_as("n", "_fd_j1"))
            .columns(vec![Projection::expr(Expr::qcol("_fd_j1", "name"))])
            .filter(Expr::qcol("_fd_j1", "id").eq(Expr::qcol("_fd_row", "hood")));
        let mut e = Expr::Subquery(Box::new(sub));
        assert_eq!(
            columns_of(&e, "_fd_row"),
            BTreeSet::from(["hood".to_owned()])
        );
        substitute(&mut e, "_fd_row", &|c| Some(Expr::qcol("_fd_b", c)));
        assert_eq!(columns_of(&e, "_fd_row"), BTreeSet::new());
        assert_eq!(columns_of(&e, "_fd_b"), BTreeSet::from(["hood".to_owned()]));
    }

    #[test]
    fn a_window_counts_at_its_own_level_and_a_subquery_does_not() {
        let w = Expr::row_number(Vec::new(), Vec::new());
        assert!(has_window_or_aggregate(&w));
        assert!(has_window_or_aggregate(&Expr::binary(
            sc_query::BinOp::Add,
            w,
            Expr::lit(1_i64)
        )));
        let sub = Select::from(Source::table("t")).columns(vec![Projection::expr(Expr::Agg {
            func: "count".into(),
            distinct: false,
            args: Vec::new(),
        })]);
        assert!(!has_window_or_aggregate(&Expr::Subquery(Box::new(sub))));
    }
}
