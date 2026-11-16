//! R1.2–R1.3: formulas parsed and resolved, their model-matrix columns, and
//! the priors they create. The expected terms and column names are taken from
//! R's documented formula semantics (`?formula`, `?model.matrix`; Chambers &
//! Hastie 1992) and checked by hand — not from running brms.

use sc_bayesreg::parse::{ATerm, Bound, Intercept, Lhs};
use sc_bayesreg::prior::{Source, TableRow};
use sc_bayesreg::{Data, PriorRow, Settings, parse_formula, resolve, strip_priors};
use sc_model::{Column, ColumnType, DatasetColumnShape, DatasetShape, Frame};

fn terms(text: &str) -> (Vec<String>, Intercept) {
    let f = parse_formula(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    let main = f.main().unwrap();
    (
        main.rhs.terms.iter().map(|t| t.label()).collect(),
        main.rhs.intercept,
    )
}

#[test]
fn the_population_level_expansion_is_rs() {
    let cases: &[(&str, &[&str], Intercept)] = &[
        ("y ~ x", &["x"], Intercept::On),
        ("y ~ a * b", &["a", "b", "a:b"], Intercept::On),
        ("y ~ a:b + a", &["a", "a:b"], Intercept::On),
        ("y ~ b:a + a", &["a", "b:a"], Intercept::On),
        (
            "y ~ (a + b + c)^2",
            &["a", "b", "c", "a:b", "a:c", "b:c"],
            Intercept::On,
        ),
        (
            "y ~ a * b * c - a:b:c",
            &["a", "b", "c", "a:b", "a:c", "b:c"],
            Intercept::On,
        ),
        ("y ~ a / b", &["a", "a:b"], Intercept::On),
        ("y ~ (a + b) / c", &["a", "b", "a:b:c"], Intercept::On),
        ("y ~ b %in% a", &["b:a"], Intercept::On),
        ("y ~ a * b - a:b", &["a", "b"], Intercept::On),
        ("y ~ x - 1", &["x"], Intercept::Off),
        ("y ~ 0 + x", &["x"], Intercept::Off),
        ("y ~ -1 + x", &["x"], Intercept::Off),
        ("y ~ x + 0", &["x"], Intercept::Off),
        ("y ~ 1", &[], Intercept::On),
        ("y ~ 0 + Intercept + x", &["x"], Intercept::Coefficient),
        (
            "y ~ log(z) + I(x^2) + poly(x, 2) + scale(w) + log(z + 1)",
            &["log(z)", "I(x^2)", "poly(x, 2)", "scale(w)", "log(z + 1)"],
            Intercept::On,
        ),
        ("y ~ x + x", &["x"], Intercept::On),
        ("y ~ a:a", &["a"], Intercept::On),
    ];
    for (text, want, intercept) in cases {
        let (got, i) = terms(text);
        assert_eq!(got, *want, "{text}");
        assert_eq!(i, *intercept, "{text}");
    }
}

#[test]
fn lines_comments_continuations_and_bf_are_read_as_one_model() {
    let text = "# the radon model\n\
                log_radon ~ floor +\n    (1 | county)   # partial pooling\n\
                \n\
                sigma ~ floor\n";
    let f = parse_formula(text).unwrap();
    assert_eq!(f.lines.len(), 2);
    assert_eq!(f.main().unwrap().rhs.groups.len(), 1);
    assert_eq!(f.lines[1].lhs, Lhs::Dpar("sigma".into()));

    let pasted = "bf(y ~ x + (1 | g),\n   sigma ~ x,\n   family = student(link_sigma = \"identity\"),\n   center = FALSE)";
    let f = parse_formula(pasted).unwrap();
    assert_eq!(f.lines.len(), 2);
    let family = f.overrides.family.as_ref().unwrap();
    assert_eq!(family.name, "student");
    assert_eq!(family.links, [("sigma".to_owned(), "identity".to_owned())]);
    assert_eq!(f.overrides.center, Some(false));
    assert!(f.overrides.notes[0].contains("`student`"));
}

#[test]
fn group_level_terms_are_lme4s_and_brmss() {
    let f = parse_formula(
        "y ~ x + (1 + x | g) + (1 + x || h) + (1 | g1:g2) + (1 | s/c) + (x | p | k) \
         + (1 | gr(m, cor = FALSE, id = \"q\")) + (0 + x | g)",
    )
    .unwrap();
    let groups = &f.main().unwrap().rhs.groups;
    type Summary = (String, bool, Option<String>, Vec<String>, Intercept);
    let summary: Vec<Summary> = groups
        .iter()
        .map(|g| {
            (
                g.group(),
                g.correlated,
                g.id.clone(),
                g.terms.iter().map(|t| t.label()).collect(),
                g.intercept,
            )
        })
        .collect();
    let o = |s: &str| s.to_owned();
    assert_eq!(
        summary,
        vec![
            (o("g"), true, None, vec![o("x")], Intercept::On),
            (o("h"), false, None, vec![o("x")], Intercept::On),
            (o("g1:g2"), true, None, vec![], Intercept::On),
            (o("s"), true, None, vec![], Intercept::On),
            (o("s:c"), true, None, vec![], Intercept::On),
            (o("k"), true, Some(o("p")), vec![o("x")], Intercept::On),
            (o("m"), false, Some(o("q")), vec![], Intercept::On),
            (o("g"), true, None, vec![o("x")], Intercept::Off),
        ]
    );
}

#[test]
fn the_responses_addition_terms_and_offsets_are_read() {
    let f =
        parse_formula("deaths | trials(n) + weights(w) ~ dose + offset(log(exposure))\n").unwrap();
    let main = f.main().unwrap();
    let Lhs::Response { column, aterms } = &main.lhs else {
        panic!("a response");
    };
    assert_eq!(column, "deaths");
    assert_eq!(aterms[0].0, ATerm::Trials(Bound::Column("n".into())));
    assert_eq!(aterms[1].0, ATerm::Weights(Bound::Column("w".into())));
    assert_eq!(main.rhs.offsets[0].label(), "log(exposure)");

    let f = parse_formula("time | cens(censored, upper) + trunc(lb = 0) ~ treatment").unwrap();
    let Lhs::Response { aterms, .. } = &f.main().unwrap().lhs else {
        panic!("a response");
    };
    assert!(matches!(&aterms[0].0, ATerm::Cens(c) if c.upper.as_deref() == Some("upper")));
    assert_eq!(
        aterms[1].0,
        ATerm::Trunc {
            lb: Some(Bound::Number(0.0)),
            ub: None
        }
    );
    let f = parse_formula("y | se(s, sigma = TRUE) ~ 1").unwrap();
    let Lhs::Response { aterms, .. } = &f.main().unwrap().lhs else {
        panic!("a response");
    };
    assert_eq!(
        aterms[0].0,
        ATerm::Se {
            column: "s".into(),
            sigma: true
        }
    );
}

#[test]
fn every_error_is_a_sentence_at_its_span() {
    // (text, sentence fragment, the text the span covers)
    let cases: &[(&str, &str, &str)] = &[
        ("y ~ s(x)", "a smoothing spline", "s"),
        ("y ~ x + gp(t)", "Gaussian process", "gp"),
        ("y ~ mo(a + b)", "one ordinal column", "mo(a + b)"),
        ("y ~ 2 + x", "only `1` (an intercept) and `0` (none)", "2"),
        ("log(y) ~ x", "calculated column", "log(y)"),
        ("y ~ x | g", "parentheses of its own", "x | g"),
        ("mvbind(a) ~ x", "two responses or more", "mvbind(a)"),
        ("y ~ .", "`.` (every other column)", "."),
        ("y ~ x + (1 | g", "never closed", "("),
        ("y ~ x == 1", "comparison", "=="),
        ("y ~ factor(g)", "calculated column", "factor"),
        ("y ~ sin(x)", "not a transform", "sin"),
        ("y ~ poly(x, 11)", "from 1 to 10", "poly(x, 11)"),
        ("y ~ log(x, 2)", "one argument", "log(x, 2)"),
        ("y | foo(z) ~ x", "not an addition term", "foo"),
        ("y ~ (1 | mm(a, b))", "multi-membership", "mm"),
        (
            "y ~ (1 | gr(g, by = h + 1))",
            "`by` is a column",
            "by = h + 1",
        ),
        ("y ~ x * (1 | g)", "part of an interaction", "(1 | g)"),
        ("y ~ x\nsigma = x", "fixes `sigma` to a number", "x"),
        (
            "y ~ x\nprior(normal(0, -1), class = b)",
            "σ must be positive",
            "normal(0, -1)",
        ),
        ("~ x", "no left-hand side", "~ x"),
    ];
    for (text, want, covers) in cases {
        let errs = parse_formula(text).expect_err(text).0;
        let e = &errs[0];
        assert!(e.message.contains(want), "{text}: {}", e.message);
        assert_eq!(
            &text[e.span.start..e.span.end],
            *covers,
            "{text}: {}",
            e.message
        );
        assert!(e.line >= 1 && e.column >= 1, "{text}: located");
    }
    // Located by line and character column, for the editor.
    let errs = parse_formula("y ~ x\nsigma ~ é + s(t)").unwrap_err().0;
    assert_eq!(
        (errs[0].line, errs[0].column, errs[0].end_column),
        (2, 13, 14)
    );
    // Every bad line is reported, not only the first.
    let errs = parse_formula("y ~ s(x)\nsigma ~ gp(t)").unwrap_err().0;
    assert_eq!(errs.len(), 2);
}

#[test]
fn pasted_priors_give_the_rows_the_table_holds_and_leave_the_formula() {
    let text = "y ~ x + (1 | g)\n\
                prior(normal(0, 1), class = b, coef = x)\n\
                c(set_prior(\"student_t(3, 0, 2.5)\", class = \"sd\", group = \"g\"),\n  \
                prior(exponential(1), class = sigma))\n\
                set_prior(\"normal(0, 5)\") + prior_string(\"normal(0, 2)\", class = \"b\", coef = \"x\", lb = 0)\n";
    let f = parse_formula(text).unwrap();
    let rows: Vec<PriorRow> = f.priors.iter().map(|p| p.row.clone()).collect();
    let table: Vec<PriorRow> = serde_json::from_value(serde_json::json!([
        {"class": "b", "coef": "x", "prior": "normal(0, 1)"},
        {"class": "sd", "group": "g", "prior": "student_t(3, 0, 2.5)"},
        {"class": "sigma", "prior": "exponential(1)"},
        {"class": "b", "prior": "normal(0, 5)"},
        {"class": "b", "coef": "x", "prior": "normal(0, 2)", "lb": 0.0},
    ]))
    .unwrap();
    assert_eq!(rows, table);
    assert_eq!(strip_priors(text, &f), "y ~ x + (1 | g)");
    // `check = FALSE` is refused: nothing pasted reaches Stan unchecked.
    let err = parse_formula("y ~ x\nset_prior(\"normal(0, 1)\", check = FALSE)").unwrap_err();
    assert!(err.to_string().contains("`check = FALSE` is not supported"));
}

// ---------------------------------------------------------------------------
// Against the dataset.

fn houses() -> Frame {
    let f = |v: &[f64]| Column::Float(v.iter().map(|x| Some(*x)).collect());
    let s = |v: &[&str]| Column::Str(v.iter().map(|x| Some((*x).to_owned())).collect());
    Frame::new(
        vec![
            (
                "price".into(),
                f(&[200.0, 340.0, 150.0, 410.0, 275.0, 300.0]),
            ),
            ("area".into(), f(&[50.0, 80.0, 40.0, 120.0, 65.0, 70.0])),
            (
                "region".into(),
                s(&["South", "North", "East", "North", "South", "East"]),
            ),
            ("county".into(), s(&["a", "a", "b", "b", "c", "c"])),
            (
                "visits".into(),
                Column::Int(vec![Some(1), Some(0), Some(3), Some(2), Some(5), Some(1)]),
            ),
            ("sold".into(), Column::Date(vec![Some(0); 6])),
        ],
        Vec::new(),
    )
    .unwrap()
}

fn shape() -> DatasetShape {
    DatasetShape::of_frame("houses", &houses())
}

fn resolved(text: &str, family: &str, data: Data<'_>) -> sc_bayesreg::Resolved {
    let f = parse_formula(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    let settings = Settings {
        family: family.into(),
        ..Settings::default()
    };
    resolve(text, &f, &settings, data).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn refusal(text: &str, family: &str, data: Data<'_>) -> String {
    let f = parse_formula(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    let settings = Settings {
        family: family.into(),
        ..Settings::default()
    };
    resolve(text, &f, &settings, data)
        .expect_err(text)
        .to_string()
}

#[test]
fn the_model_matrix_columns_are_rs() {
    let frame = houses();
    let cases: &[(&str, &[&str])] = &[
        ("price ~ area", &["area"]),
        ("price ~ region", &["regionNorth", "regionSouth"]),
        (
            "price ~ 0 + region",
            &["regionEast", "regionNorth", "regionSouth"],
        ),
        (
            "price ~ area * region",
            &[
                "area",
                "regionNorth",
                "regionSouth",
                "area:regionNorth",
                "area:regionSouth",
            ],
        ),
        // No main effect of `region`: a slope per level.
        (
            "price ~ area + area:region",
            &["area", "area:regionNorth", "area:regionSouth"],
        ),
        (
            "price ~ region:area",
            &["regionEast:area", "regionNorth:area", "regionSouth:area"],
        ),
        // Nested: a slope within each region.
        (
            "price ~ region / area",
            &[
                "regionNorth",
                "regionSouth",
                "regionEast:area",
                "regionNorth:area",
                "regionSouth:area",
            ],
        ),
        (
            "price ~ poly(area, 2) + log(area)",
            &["poly(area, 2)1", "poly(area, 2)2", "log(area)"],
        ),
        ("price ~ 0 + Intercept + area", &["Intercept", "area"]),
        ("price ~ 1", &[]),
    ];
    for (text, want) in cases {
        let r = resolved(text, "gaussian", Data::Frame(&frame));
        let names = r.names(&frame).unwrap();
        assert_eq!(names.b["mu"], *want, "{text}");
    }
    // A group-level term's effects: `Intercept`, then its terms' columns.
    let r = resolved(
        "price ~ area + (1 + area | county)",
        "gaussian",
        Data::Frame(&frame),
    );
    assert_eq!(
        r.names(&frame).unwrap().blocks[&(0, "mu".to_owned())],
        ["Intercept", "area"]
    );
}

#[test]
fn the_resolver_names_what_does_not_fit_the_dataset_or_the_family() {
    let shape = shape();
    let cases: &[(&str, &str, &str)] = &[
        (
            "price ~ arae",
            "gaussian",
            "`arae` is not a column of the dataset; did you mean `area`?",
        ),
        (
            "price ~ area + (1 | countyy)",
            "gaussian",
            "did you mean `county`?",
        ),
        (
            "price ~ area",
            "poisson",
            "`poisson` models whole numbers of zero or more, and `price` is a decimal-number column",
        ),
        ("region ~ area", "gaussian", "`region` is a text column"),
        ("price ~ sold", "gaussian", "`sold` is a date"),
        ("price ~ log(region)", "gaussian", "`region` is categorical"),
        (
            "price ~ area\nnu ~ area",
            "gaussian",
            "the family `gaussian` has no `nu` (it has `sigma`)",
        ),
        (
            "visits ~ area\nsigma ~ area",
            "poisson",
            "no distributional parameters besides the mean",
        ),
        (
            "price ~ area\nvisits ~ area\nsigma ~ area",
            "gaussian",
            "belongs inside the `bf(…)` of its response",
        ),
        (
            "price | trials(visits) ~ area",
            "gaussian",
            "does not take `trials()` (it is for `binomial` and `beta_binomial`)",
        ),
        ("visits ~ area", "binomial", "needs its number of trials"),
        (
            "price | rate(area) ~ 1",
            "gaussian",
            "does not take `rate()`",
        ),
        (
            "visits | cens(region) ~ 1",
            "bernoulli",
            "does not take `cens()`",
        ),
        (
            "price ~ area + (1 | p | county) + (1 | p | region)",
            "gaussian",
            "must have the same grouping factor",
        ),
        (
            "price ~ area",
            "cumulative",
            "models ordered categories numbered 1, 2, 3, …, and `price` is a decimal-number",
        ),
        ("price ~ area", "gausian", "there is no family `gausian`"),
    ];
    for (text, family, want) in cases {
        let got = refusal(text, family, Data::Shape(&shape));
        assert!(got.contains(want), "{text}: {got}");
    }
    // With the names only, names are checked and types are not.
    let names: Vec<String> = shape.columns.iter().map(|c| c.name.clone()).collect();
    assert!(refusal("price ~ arae", "gaussian", Data::Names(&names)).contains("`arae`"));
    // The underline is under the column, not the line.
    let f = parse_formula("price ~ area + arae").unwrap();
    let err = resolve(
        "price ~ area + arae",
        &f,
        &Settings::default(),
        Data::Names(&names),
    )
    .unwrap_err();
    assert_eq!((err.0[0].span.start, err.0[0].span.end), (15, 19));
    resolved("region ~ area", "gaussian", Data::Names(&names));
    // A link the parameter does not take, from the form or from the text.
    let f = parse_formula("bf(price ~ area, family = gaussian(link = \"logit\"))").unwrap();
    let err = resolve("", &f, &Settings::default(), Data::Shape(&shape)).unwrap_err();
    assert!(
        err.to_string()
            .contains("`logit` is not a link `mu` of `gaussian` can take"),
        "{err}"
    );
}

#[test]
fn the_links_and_the_blocks_are_resolved() {
    let shape = shape();
    let r = resolved(
        "price ~ area + (1 + area | p | county)\nsigma ~ region + (1 | p | county)",
        "gaussian",
        Data::Shape(&shape),
    );
    assert_eq!(r.links["mu"].name(), "identity");
    assert_eq!(r.links["sigma"].name(), "log");
    assert_eq!(r.predictors.len(), 2);
    assert!(r.scalars.is_empty());
    // The shared ID makes one block across `mu` and `sigma`.
    assert_eq!(r.blocks.len(), 1);
    assert_eq!(r.blocks[0].parts.len(), 2);

    let r = resolved(
        "price ~ area + (1 | county) + (0 + area | county)",
        "student",
        Data::Shape(&shape),
    );
    assert_eq!(r.blocks.len(), 2);
    assert_eq!(r.scalars, ["sigma", "nu"]);
}

fn row<'a>(table: &'a [TableRow], class: &str, coef: &str, group: &str) -> &'a TableRow {
    table
        .iter()
        .find(|r| r.class == class && r.coef == coef && r.group == group)
        .unwrap_or_else(|| panic!("no row {class} {coef} {group}: {table:#?}"))
}

#[test]
fn the_prior_table_lists_every_parameter_with_its_default() {
    let text = "price ~ area + region + (1 + area | county)";
    let shape = shape();
    let r = resolved(text, "gaussian", Data::Shape(&shape));
    let table = sc_bayesreg::prior::resolve(&r.prior_slots(None), &[], None, &r.response, r.center);
    assert!(table.unmatched.is_empty());
    let t = &table.rows;
    assert_eq!(row(t, "b", "", "").prior, "flat");
    assert!(row(t, "b", "", "").whole_class);
    assert_eq!(row(t, "b", "area", "").prior, "flat");
    // The levels are not known before the data is read.
    assert_eq!(row(t, "b", "region…", "").source, Source::Default);
    let intercept = row(t, "Intercept", "", "");
    assert_eq!(intercept.source, Source::DefaultFromData);
    assert_eq!(
        intercept.rule.as_deref(),
        Some("student_t(3, median, scale) of `price`")
    );
    assert_eq!(
        row(t, "sd", "Intercept", "county").rule.as_deref(),
        Some("student_t(3, 0, scale) of `price`")
    );
    assert_eq!(row(t, "sd", "area", "county").prior, "student_t(3, 0, 2.5)");
    assert_eq!(row(t, "cor", "", "county").prior, "lkj(1)");
    assert_eq!(row(t, "sigma", "", "").source, Source::DefaultFromData);

    // After the data: the levels, and the numbers. The median price is 287.5,
    // the median absolute deviation from it 70, and the MAD 1.4826 × 70.
    let frame = houses();
    let r = resolved(text, "gaussian", Data::Frame(&frame));
    let names = r.names(&frame).unwrap();
    let scale = r.data_scale(&frame);
    let table = sc_bayesreg::prior::resolve(
        &r.prior_slots(Some(&names)),
        &[],
        Some(scale),
        &r.response,
        r.center,
    );
    let t = &table.rows;
    assert_eq!(row(t, "b", "regionNorth", "").prior, "flat");
    assert_eq!(
        row(t, "Intercept", "", "").prior,
        "student_t(3, 287.5, 103.8)"
    );
    assert!(row(t, "Intercept", "", "").rule.is_none());
    assert_eq!(row(t, "sigma", "", "").prior, "student_t(3, 0, 103.8)");

    // A log link: the scale is the log response's. The family-specific
    // defaults are brms's documented ones.
    let r = resolved("visits ~ area", "negbinomial", Data::Frame(&frame));
    let table = sc_bayesreg::prior::resolve(
        &r.prior_slots(None),
        &[],
        Some(r.data_scale(&frame)),
        &r.response,
        r.center,
    );
    assert_eq!(
        row(&table.rows, "shape", "", "").prior,
        "inv_gamma(0.4, 0.3)"
    );
    assert_eq!(
        row(&table.rows, "Intercept", "", "").prior,
        "student_t(3, 0.7, 2.5)"
    );
    let r = resolved("visits ~ area", "student", Data::Frame(&frame));
    let table = sc_bayesreg::prior::resolve(&r.prior_slots(None), &[], None, &r.response, r.center);
    assert_eq!(row(&table.rows, "nu", "", "").prior, "gamma(2, 0.1)");
}

fn rows(json: serde_json::Value) -> Vec<PriorRow> {
    serde_json::from_value(json).unwrap()
}

#[test]
fn a_row_left_unmatched_by_a_formula_edit_is_kept_and_flagged() {
    let shape = shape();
    let explicit = rows(serde_json::json!([
        {"class": "b", "coef": "area", "prior": "normal(0, 1)"},
        {"class": "b", "coef": "regionNorth", "prior": "normal(0, 2)"},
        {"class": "sd", "group": "county", "prior": "exponential(1)"},
    ]));
    // With `region` and the group term: every row targets something.
    let r = resolved(
        "price ~ area + region + (1 | county)",
        "gaussian",
        Data::Shape(&shape),
    );
    let table =
        sc_bayesreg::prior::resolve(&r.prior_slots(None), &explicit, None, &r.response, r.center);
    assert!(table.unmatched.is_empty(), "{:?}", table.unmatched);
    assert_eq!(
        row(&table.rows, "sd", "Intercept", "county").prior,
        "exponential(1)"
    );

    // `region` and the group term removed: the two rows stay, flagged.
    let r = resolved("price ~ area", "gaussian", Data::Shape(&shape));
    let table =
        sc_bayesreg::prior::resolve(&r.prior_slots(None), &explicit, None, &r.response, r.center);
    let flagged: Vec<&PriorRow> = table.unmatched.iter().map(|u| &u.row).collect();
    assert_eq!(flagged, [&explicit[1], &explicit[2]]);
    assert!(
        table.unmatched[0]
            .reason
            .contains("no term of the formula gives a coefficient `regionNorth`")
    );
    assert!(table.unmatched[1].reason.contains("no group-level terms"));
    // The area row still applies; a save is refused until the others go.
    assert_eq!(row(&table.rows, "b", "area", "").prior, "normal(0, 1)");
    let err = table.require_matched().unwrap_err().to_string();
    assert!(err.contains("the prior row `b` of `regionNorth`"), "{err}");
}

#[test]
fn a_level_row_is_matched_at_fit_not_at_save() {
    let explicit = rows(serde_json::json!([
        {"class": "b", "coef": "regionWest", "prior": "normal(0, 1)"},
    ]));
    let text = "price ~ region";
    // At save, `regionWest` could be a level of `region`.
    let shape = shape();
    let r = resolved(text, "gaussian", Data::Shape(&shape));
    let table =
        sc_bayesreg::prior::resolve(&r.prior_slots(None), &explicit, None, &r.response, r.center);
    assert!(table.unmatched.is_empty());
    // At fit, the levels are East, North and South: refused by name.
    let frame = houses();
    let r = resolved(text, "gaussian", Data::Frame(&frame));
    let names = r.names(&frame).unwrap();
    let table = sc_bayesreg::prior::resolve(
        &r.prior_slots(Some(&names)),
        &explicit,
        None,
        &r.response,
        r.center,
    );
    assert_eq!(table.unmatched.len(), 1);
    let err = table.require_matched().unwrap_err().to_string();
    assert!(
        err.contains("`regionWest`") && err.contains("`regionNorth`"),
        "{err}"
    );
}

#[test]
fn rows_that_name_what_the_formula_does_not_have_are_refused_with_a_sentence() {
    let shape = shape();
    let r = resolved(
        "visits ~ area + (1 | county)",
        "poisson",
        Data::Shape(&shape),
    );
    let cases = [
        (
            serde_json::json!({"class": "sigma", "prior": "exponential(1)"}),
            "the family has no `sigma`",
        ),
        (
            serde_json::json!({"class": "sd", "group": "region", "prior": "exponential(1)"}),
            "`region` is not a grouping factor here (they are `county`)",
        ),
        (
            serde_json::json!({"class": "bb", "prior": "normal(0, 1)"}),
            "there is no class `bb`",
        ),
        (
            serde_json::json!({"class": "b", "dpar": "sigma", "prior": "normal(0, 1)"}),
            "`sigma` has no formula",
        ),
        (
            serde_json::json!({"class": "Intercept", "coef": "x", "prior": "normal(0, 1)"}),
            "no coefficients, so `coef` must be empty",
        ),
        (
            serde_json::json!({"class": "sd", "prior": "flat"}),
            "`flat` is allowed on `b` only",
        ),
        (
            serde_json::json!({"class": "b", "prior": "gamma(1, 1)"}),
            "positive numbers only",
        ),
        (
            serde_json::json!({"class": "b", "nlpar": "a", "prior": "normal(0, 1)"}),
            "`a` is not a non-linear parameter",
        ),
        (
            serde_json::json!({"class": "b", "prior": "normal(0, -2)"}),
            "σ must be positive",
        ),
    ];
    for (json, want) in cases {
        let explicit = rows(serde_json::json!([json]));
        let table = sc_bayesreg::prior::resolve(
            &r.prior_slots(None),
            &explicit,
            None,
            &r.response,
            r.center,
        );
        assert_eq!(table.unmatched.len(), 1, "{json}");
        assert!(
            table.unmatched[0].reason.contains(want),
            "{json}: {}",
            table.unmatched[0].reason
        );
    }
}

#[test]
fn the_response_values_are_checked_against_the_family_at_fit() {
    let frame = houses();
    let r = resolved("visits ~ area", "poisson", Data::Frame(&frame));
    r.check_response(&frame).unwrap();
    let r = resolved("visits ~ area", "bernoulli", Data::Frame(&frame));
    let err = r.check_response(&frame).unwrap_err().to_string();
    assert!(err.contains("`visits` is 3 in row 3"), "{err}");
    // The shape alone does not see values, only types.
    let shape = DatasetShape {
        table: "t".into(),
        columns: vec![DatasetColumnShape {
            name: "y".into(),
            ty: ColumnType::Int,
        }],
    };
    resolved("y ~ 1", "bernoulli", Data::Shape(&shape));
}

/// R1.4: what the resolver hands the binder — each linear predictor's terms
/// as a `terms` binding, sized and labelled by a `names` dimension — binds to
/// the matrix and the names of the cases above.
#[test]
fn a_terms_binding_of_a_resolved_formula_binds_rs_columns() {
    use sc_model::{Declaration, Element, Interface, SizeExpr};
    let frame = houses();
    let program = Interface {
        data: vec![
            Declaration::new("N", Element::Int, vec![], "int"),
            Declaration::new("K", Element::Int, vec![], "int"),
            Declaration::new(
                "X",
                Element::Real,
                vec![SizeExpr::var("N"), SizeExpr::var("K")],
                "matrix[N, K]",
            ),
        ],
        ..Interface::default()
    };
    for (text, want) in [
        (
            "price ~ area * region",
            vec![
                "area",
                "regionNorth",
                "regionSouth",
                "area:regionNorth",
                "area:regionSouth",
            ],
        ),
        (
            "price ~ 0 + region",
            vec!["regionEast", "regionNorth", "regionSouth"],
        ),
        (
            "price ~ region / area",
            vec![
                "regionNorth",
                "regionSouth",
                "regionEast:area",
                "regionNorth:area",
                "regionSouth:area",
            ],
        ),
    ] {
        let r = resolved(text, "gaussian", Data::Frame(&frame));
        let terms = &r.predictor("mu").unwrap().terms;
        let mut config = sc_types::Attrs::new();
        config.insert(
            sc_model::BINDINGS_KEY.to_owned(),
            serde_json::json!({
                "N": {"kind": "count", "dataset": "main"},
                "K": {"kind": "size", "dimension": "b_mu"},
                "X": {
                    "kind": "terms",
                    "dataset": "main",
                    "terms": terms.terms,
                    "intercept": terms.intercept,
                },
            }),
        );
        config.insert(
            sc_model::DIMENSIONS_KEY.to_owned(),
            serde_json::json!({"b_mu": {"kind": "names", "of": "X"}}),
        );
        let bound = sc_model::bind_data(
            &program,
            &config,
            &[("main".to_owned(), frame.clone())],
            sc_model::DEFAULT_MAX_DATA_VALUES,
        )
        .unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(bound.coordinates.designs["X"].columns, want, "{text}");
        assert_eq!(
            bound.coordinates.dimension("b_mu").unwrap().labels,
            want,
            "{text}"
        );
        assert_eq!(bound.json["K"], serde_json::json!(want.len()), "{text}");
    }
}
