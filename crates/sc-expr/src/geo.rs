//! The **`Geo` functions** (analytics TODO A5.3): geometry in a formula,
//! computed by PostGIS.
//!
//! They are spelled as methods of one global, `Geo.distance(a, b)`, the way
//! JavaScript's own functions are spelled `Math.round(x)`. The language's scope
//! rule is that a bare identifier names a field of the row, so a bare
//! `distance(a, b)` would claim a name a table may well have a column called;
//! `Geo` claims one name, and like `Math` it gives way to a field called `Geo`.
//!
//! **Only the database computes them.** A formula's symbolic translation turns
//! each call into PostGIS SQL ([`sql`]); the JavaScript evaluator has no `Geo`
//! and says so by name if a formula that needs one reaches it. Distances and
//! areas are in metres and square metres on the WGS84 spheroid (`geography`
//! casts); the predicates compare the geometries as stored, in longitude and
//! latitude; the grid cells are laid out in the UTM zone of the geometry (see
//! `sc-db-postgres`'s `_fd_square_cell` and `_fd_hex_cell`).

use std::collections::BTreeSet;

use sc_error::Result;
use sc_query::{Expr as QExpr, Value};

use crate::ast::{Ast, MemberProp};

/// The global the functions are methods of.
pub const GEO: &str = "Geo";

/// What a `Geo` function returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoResult {
    /// A geometry (a GeoJSON object when read).
    Geometry,
    /// A number: metres or square metres.
    Number,
    /// True or false.
    Bool,
}

/// One `Geo` function: its name, its parameters and what it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeoFunction {
    /// The method name, as in `Geo.<name>(…)`.
    pub name: &'static str,
    /// The parameters' names, for a refusal that shows how to call it.
    pub params: &'static [&'static str],
    /// What it returns.
    pub result: GeoResult,
    /// One sentence saying what it computes, for completions and docs.
    pub doc: &'static str,
}

/// Every `Geo` function.
pub const GEO_FUNCTIONS: &[GeoFunction] = &[
    GeoFunction {
        name: "point",
        params: &["longitude", "latitude"],
        result: GeoResult::Geometry,
        doc: "the point at a longitude and latitude in degrees",
    },
    GeoFunction {
        name: "buffer",
        params: &["geometry", "metres"],
        result: GeoResult::Geometry,
        doc: "the area within a distance in metres of a geometry",
    },
    GeoFunction {
        name: "centroid",
        params: &["geometry"],
        result: GeoResult::Geometry,
        doc: "the centre of a geometry",
    },
    GeoFunction {
        name: "area",
        params: &["geometry"],
        result: GeoResult::Number,
        doc: "the area of a polygon in square metres",
    },
    GeoFunction {
        name: "length",
        params: &["geometry"],
        result: GeoResult::Number,
        doc: "the length of a line in metres",
    },
    GeoFunction {
        name: "distance",
        params: &["a", "b"],
        result: GeoResult::Number,
        doc: "the shortest distance between two geometries in metres",
    },
    GeoFunction {
        name: "intersects",
        params: &["a", "b"],
        result: GeoResult::Bool,
        doc: "whether two geometries share any point",
    },
    GeoFunction {
        name: "contains",
        params: &["a", "b"],
        result: GeoResult::Bool,
        doc: "whether the first geometry contains the second",
    },
    GeoFunction {
        name: "within",
        params: &["a", "b"],
        result: GeoResult::Bool,
        doc: "whether the first geometry is inside the second",
    },
    GeoFunction {
        name: "squareCell",
        params: &["geometry", "metres"],
        result: GeoResult::Geometry,
        doc: "the square grid cell, of sides in metres, that a geometry's point is in",
    },
    GeoFunction {
        name: "hexCell",
        params: &["geometry", "metres"],
        result: GeoResult::Geometry,
        doc: "the hexagonal grid cell, of edges in metres, that a geometry's point is in",
    },
    GeoFunction {
        name: "intersection",
        params: &["a", "b"],
        result: GeoResult::Geometry,
        doc: "the part two geometries share",
    },
    GeoFunction {
        name: "fromGeoJSON",
        params: &["text"],
        result: GeoResult::Geometry,
        doc: "the geometry a GeoJSON text describes, in longitude and latitude",
    },
];

/// The function called `name`.
pub fn geo_function(name: &str) -> Option<&'static GeoFunction> {
    GEO_FUNCTIONS.iter().find(|f| f.name == name)
}

/// How a function is called, for a refusal: `Geo.distance(a, b)`.
fn usage(f: &GeoFunction) -> String {
    format!("`Geo.{}({})`", f.name, f.params.join(", "))
}

/// The function and arguments of a `Geo.<name>(…)` call, when `ast` is one
/// whose name is a `Geo` function. Arity is not checked here (validation does).
pub(crate) fn as_geo_call(ast: &Ast) -> Option<(&'static GeoFunction, &[Ast])> {
    let Ast::Call { callee, args, .. } = ast else {
        return None;
    };
    let Ast::Member {
        obj,
        prop: MemberProp::Static(name),
        ..
    } = &**callee
    else {
        return None;
    };
    if !matches!(&**obj, Ast::Ident(g) if g == GEO) {
        return None;
    }
    geo_function(name).map(|f| (f, args.as_slice()))
}

/// Check every use of `Geo` in `ast` — each one a call of a `Geo` function
/// with the right number of arguments — and collect the functions called.
/// `locals` are the `=>` parameters in scope, which shadow `Geo`.
pub(crate) fn walk_geo_calls(
    ast: &Ast,
    locals: &mut Vec<String>,
    called: &mut BTreeSet<String>,
) -> std::result::Result<(), String> {
    let shadowed = |locals: &Vec<String>| locals.iter().any(|l| l == GEO);
    match ast {
        Ast::Call { callee, args, .. }
            if matches!(&**callee, Ast::Member { obj, .. } if matches!(&**obj, Ast::Ident(g) if g == GEO))
                && !shadowed(locals) =>
        {
            let Ast::Member { prop, .. } = &**callee else {
                return Ok(());
            };
            let MemberProp::Static(name) = prop else {
                return Err(
                    "a `Geo` function is named, as `Geo.distance(a, b)`, not computed".to_owned(),
                );
            };
            let Some(f) = geo_function(name) else {
                return Err(format!(
                    "`Geo.{name}` is not a geometry function; they are {}",
                    GEO_FUNCTIONS
                        .iter()
                        .map(usage)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            if args.len() != f.params.len() {
                return Err(format!(
                    "{} takes {} argument{}, and is given {}",
                    usage(f),
                    f.params.len(),
                    if f.params.len() == 1 { "" } else { "s" },
                    args.len()
                ));
            }
            called.insert(f.name.to_owned());
            for a in args {
                walk_geo_calls(a, locals, called)?;
            }
            Ok(())
        }
        Ast::Ident(g) if g == GEO && !shadowed(locals) => Err(
            "`Geo` holds the geometry functions and is only called, as `Geo.distance(a, b)`"
                .to_owned(),
        ),
        Ast::Arrow { params, body } => {
            let depth = locals.len();
            locals.extend(params.iter().cloned());
            let out = walk_geo_calls(body, locals, called);
            locals.truncate(depth);
            out
        }
        other => {
            for child in crate::analyze::child_nodes(other) {
                walk_geo_calls(child, locals, called)?;
            }
            Ok(())
        }
    }
}

fn func(name: &str, args: Vec<QExpr>) -> QExpr {
    QExpr::Func {
        name: name.to_owned(),
        args,
    }
}

fn cast(expr: QExpr, type_name: &str) -> QExpr {
    QExpr::Cast {
        expr: Box::new(expr),
        type_name: type_name.to_owned(),
    }
}

fn geography(expr: QExpr) -> QExpr {
    cast(expr, "geography")
}

fn number(expr: QExpr) -> QExpr {
    cast(expr, "double precision")
}

/// The PostGIS SQL for a call of `f` on already-translated `args` (as many as
/// `f` has parameters; validation has made sure of that).
pub(crate) fn sql(f: &GeoFunction, args: Vec<QExpr>) -> Result<QExpr> {
    let mut args = args.into_iter();
    let mut next = || {
        args.next()
            .ok_or_else(|| sc_error::Error::invalid(format!("{} is missing an argument", usage(f))))
    };
    Ok(match f.name {
        "point" => {
            let (lon, lat) = (number(next()?), number(next()?));
            func(
                "ST_SetSRID",
                vec![
                    func("ST_MakePoint", vec![lon, lat]),
                    QExpr::Lit(Value::Int(4326)),
                ],
            )
        }
        "buffer" => {
            let (g, m) = (next()?, number(next()?));
            cast(func("ST_Buffer", vec![geography(g), m]), "geometry")
        }
        "centroid" => cast(func("ST_Centroid", vec![geography(next()?)]), "geometry"),
        "area" => func("ST_Area", vec![geography(next()?)]),
        "length" => func("ST_Length", vec![geography(next()?)]),
        "distance" => {
            let (a, b) = (geography(next()?), geography(next()?));
            func("ST_Distance", vec![a, b])
        }
        "intersects" | "contains" | "within" => {
            let name = match f.name {
                "intersects" => "ST_Intersects",
                "contains" => "ST_Contains",
                _ => "ST_Within",
            };
            let (a, b) = (next()?, next()?);
            func(name, vec![a, b])
        }
        "intersection" => {
            let (a, b) = (next()?, next()?);
            func("ST_Intersection", vec![a, b])
        }
        // A map's lasso and its selection by location (analytics TODO A5.10)
        // name a shape drawn on the screen this way.
        "fromGeoJSON" => func(
            "ST_SetSRID",
            vec![
                func("ST_GeomFromGeoJSON", vec![cast(next()?, "text")]),
                QExpr::Lit(Value::Int(4326)),
            ],
        ),
        "squareCell" | "hexCell" => {
            let name = if f.name == "squareCell" {
                "_fd_square_cell"
            } else {
                "_fd_hex_cell"
            };
            let (g, m) = (next()?, number(next()?));
            func(name, vec![g, m])
        }
        other => {
            return Err(sc_error::Error::invalid(format!(
                "`Geo.{other}` has no SQL translation"
            )));
        }
    })
}
