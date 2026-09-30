//! The type of a formula's value, as far as it follows from the columns it
//! reads (analytics TODO A1.2).
//!
//! A [`SchemaShape`](sc_expr::SchemaShape) carries no types, so the formula
//! language cannot answer this; the stage can, for the formulas people write
//! most — a column, a join path, arithmetic, a comparison, a count over a child
//! table. Everything else is [`ColType::Unknown`], which a read fills in from
//! the values. The answer matters for what later operations offer (a mean needs
//! a number) and for keeping a foreign key a foreign key: `neighbourhood` copied
//! into a new column is still a key to `neighbourhoods`.

use sc_expr::{Ast, BinaryOp, MemberProp, UnaryOp};

use crate::shape::{ColType, ForeignKey, Schema};

/// What the compiler knows about one column when inferring: its type and where
/// it points.
pub(crate) struct Known<'a> {
    pub name: &'a str,
    pub ty: ColType,
    pub key: Option<&'a ForeignKey>,
}

/// The type, and the foreign key when the value is one, of `ast` over a row
/// with `columns`.
pub(crate) fn infer(
    ast: &Ast,
    columns: &[Known<'_>],
    schema: &Schema,
) -> (ColType, Option<ForeignKey>) {
    match ast {
        Ast::Ident(name) if name.contains(sc_expr::JOIN) => join_path(name, columns, schema),
        Ast::Ident(name) => columns
            .iter()
            .find(|c| c.name == name)
            .map_or((ColType::Unknown, None), |c| (c.ty, c.key.cloned())),
        Ast::Num(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => (ColType::Int, None),
        Ast::Num(_) => (ColType::Float, None),
        Ast::Str(_) | Ast::Template { .. } => (ColType::Text, None),
        Ast::Bool(_) => (ColType::Bool, None),
        Ast::Null | Ast::Array(_) | Ast::Arrow { .. } => (ColType::Unknown, None),
        Ast::Unary { op, expr } => match op {
            UnaryOp::Not => (ColType::Bool, None),
            UnaryOp::TypeOf => (ColType::Text, None),
            UnaryOp::Neg | UnaryOp::Pos => (numeric(infer(expr, columns, schema).0), None),
        },
        Ast::Binary { op, l, r } => {
            let lt = || infer(l, columns, schema);
            let rt = || infer(r, columns, schema);
            match op {
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::StrictEq
                | BinaryOp::StrictNotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
                | BinaryOp::And
                | BinaryOp::Or => (ColType::Bool, None),
                BinaryOp::Add => {
                    let (a, b) = (lt().0, rt().0);
                    if a == ColType::Text || b == ColType::Text {
                        (ColType::Text, None)
                    } else {
                        (arithmetic(a, b), None)
                    }
                }
                BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Mod => (arithmetic(lt().0, rt().0), None),
                BinaryOp::Div => (ColType::Float, None),
                BinaryOp::Nullish => {
                    let (a, key) = lt();
                    let b = rt().0;
                    (a.unify(b), key.filter(|_| a == b || b == ColType::Unknown))
                }
            }
        }
        Ast::Cond { cons, alt, .. } => {
            let (a, key) = infer(cons, columns, schema);
            let b = infer(alt, columns, schema).0;
            (a.unify(b), key)
        }
        Ast::Member { obj, prop, .. } => match (&**obj, prop) {
            // `viewingsↃhouse.length`
            (Ast::Ident(rel), MemberProp::Static(p)) if is_relation(rel) && p == "length" => {
                (ColType::Int, None)
            }
            _ => (ColType::Unknown, None),
        },
        Ast::Call { callee, args, .. } => call(callee, args, schema),
    }
}

/// The type of a call: an aggregation over a child table, or a global the
/// formula language knows.
fn call(callee: &Ast, args: &[Ast], schema: &Schema) -> (ColType, Option<ForeignKey>) {
    let Ast::Member {
        obj,
        prop: MemberProp::Static(method),
        ..
    } = callee
    else {
        return match callee {
            Ast::Ident(f) if f == "String" => (ColType::Text, None),
            Ast::Ident(f) if f == "Number" || f == "parseFloat" => (ColType::Float, None),
            Ast::Ident(f) if f == "parseInt" => (ColType::Int, None),
            Ast::Ident(f) if f == "Boolean" => (ColType::Bool, None),
            _ => (ColType::Unknown, None),
        };
    };
    if let Ast::Ident(root) = &**obj {
        if root == "Math" {
            return (ColType::Float, None);
        }
        if let Some((child, _)) = root.split_once(sc_expr::INVERSE) {
            // The child field an aggregation's selector names, if it names one
            // by a string: `ordersↃcustomer.max("placed")`.
            let field = args.first().and_then(|a| match a {
                Ast::Str(s) => schema.tables.get(child).and_then(|t| t.column(s)),
                _ => None,
            });
            return match method.as_str() {
                "count" => (ColType::Int, None),
                "sum" => (field.map_or(ColType::Float, |f| numeric(f.ty)), None),
                "avg" => (ColType::Float, None),
                "min" | "max" => field.map_or((ColType::Unknown, None), |f| (f.ty, f.key.clone())),
                _ => (ColType::Unknown, None),
            };
        }
    }
    (ColType::Unknown, None)
}

/// A `Ⱶ`-path's type: the field it ends on, reached through the keys it
/// follows.
fn join_path(name: &str, columns: &[Known<'_>], schema: &Schema) -> (ColType, Option<ForeignKey>) {
    let mut segments = name.split(sc_expr::JOIN);
    let Some(first) = segments.next() else {
        return (ColType::Unknown, None);
    };
    let Some(mut key) = columns
        .iter()
        .find(|c| c.name == first)
        .and_then(|c| c.key.cloned())
    else {
        return (ColType::Unknown, None);
    };
    let segments: Vec<&str> = segments.collect();
    for (i, segment) in segments.iter().enumerate() {
        let Some(column) = schema
            .tables
            .get(&key.table)
            .and_then(|t| t.column(segment))
        else {
            return (ColType::Unknown, None);
        };
        if i + 1 == segments.len() {
            return (column.ty, column.key.clone());
        }
        match &column.key {
            Some(next) => key = next.clone(),
            None => return (ColType::Unknown, None),
        }
    }
    (ColType::Unknown, None)
}

fn is_relation(name: &str) -> bool {
    name.contains(sc_expr::INVERSE)
}

/// The result of `+ - * %` over two numbers.
fn arithmetic(a: ColType, b: ColType) -> ColType {
    match (a, b) {
        (ColType::Int, ColType::Int) => ColType::Int,
        (ColType::Unknown, _) | (_, ColType::Unknown) => ColType::Unknown,
        (ColType::Decimal, ColType::Decimal | ColType::Int) | (ColType::Int, ColType::Decimal) => {
            ColType::Decimal
        }
        _ => ColType::Float,
    }
}

/// A number's own type, anything else as a plain number.
fn numeric(t: ColType) -> ColType {
    if t.is_numeric() || t == ColType::Unknown {
        t
    } else {
        ColType::Float
    }
}

#[cfg(test)]
mod tests {
    use sc_expr::{Formula, SchemaShape};

    use super::*;
    use crate::shape::{StageColumn, TableInfo};

    fn schema() -> Schema {
        Schema::new(
            SchemaShape::new(),
            [TableInfo {
                name: "neighbourhoods".into(),
                columns: vec![
                    StageColumn {
                        name: "name".into(),
                        ty: ColType::Text,
                        key: None,
                    },
                    StageColumn {
                        name: "city".into(),
                        ty: ColType::Int,
                        key: Some(ForeignKey {
                            table: "cities".into(),
                            field: "id".into(),
                        }),
                    },
                ],
                calc: Default::default(),
                primary_key: Some("id".into()),
            }],
        )
    }

    fn ty(src: &str) -> (ColType, Option<ForeignKey>) {
        let hood = ForeignKey {
            table: "neighbourhoods".into(),
            field: "id".into(),
        };
        let cols = [
            Known {
                name: "price",
                ty: ColType::Float,
                key: None,
            },
            Known {
                name: "rooms",
                ty: ColType::Int,
                key: None,
            },
            Known {
                name: "neighbourhood",
                ty: ColType::Int,
                key: Some(&hood),
            },
        ];
        infer(Formula::parse(src).expect("parse").ast(), &cols, &schema())
    }

    #[test]
    fn arithmetic_comparison_and_join_paths_have_types() {
        assert_eq!(ty("price / rooms").0, ColType::Float);
        assert_eq!(ty("rooms + 1").0, ColType::Int);
        assert_eq!(ty("price > 100").0, ColType::Bool);
        assert_eq!(ty("neighbourhoodⱵname").0, ColType::Text);
        assert_eq!(ty("`${rooms} rooms`").0, ColType::Text);
        assert_eq!(ty("Math.log(price)").0, ColType::Float);
        assert_eq!(ty("viewingsↃhouse.length").0, ColType::Int);
    }

    #[test]
    fn a_key_stays_a_key_when_copied_or_followed_to_another_key() {
        assert_eq!(
            ty("neighbourhood").1.expect("a key").table,
            "neighbourhoods"
        );
        assert_eq!(ty("neighbourhoodⱵcity").1.expect("a key").table, "cities");
        assert_eq!(ty("price").1, None);
    }
}
