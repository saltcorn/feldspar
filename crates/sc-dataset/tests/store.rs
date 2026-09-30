//! `_fd_datasets` on both backends (analytics TODO A1.1): what a save writes, a
//! load reads back — for every kind of operation — and what a save refuses.

use sc_dataset::{
    AggregateOp, Base, CompleteColumn, CompleteOp, CompleteValues, DatasetDef, FillValue, GroupKey,
    JoinKey, JoinKind, JoinOp, LimitMode, LimitOp, Op, Operation, OrderKey, Other, SelectColumn,
    SelectOp, SortKey, SortOp, SplitOp, SplitSummary, StackOp, Summary, SummaryFunction, UnionOp,
    WindowFunction, WindowOp, clone_dataset, delete_dataset, list_datasets, load_dataset,
    load_dataset_by_name, save_dataset,
};
use sc_error::Result;
use serde_json::json;

use crate::fixture::both;

/// One of every kind of operation.
fn every_kind() -> DatasetDef {
    let ops = vec![
        Op::calculated("ppm", "price / area"),
        Op::filter("price > 100000"),
        Op::Select(SelectOp {
            columns: vec![
                SelectColumn::keep("price"),
                SelectColumn::renamed("area", "m2"),
            ],
        }),
        Op::Sort(SortOp {
            keys: vec![SortKey {
                formula: "price".into(),
                descending: true,
            }],
        }),
        Op::Window(WindowOp {
            name: "previous".into(),
            function: WindowFunction::Lag,
            column: Some("price".into()),
            offset: Some(2),
            partition: vec!["neighbourhood".into()],
            order: vec![OrderKey::desc("year_built")],
        }),
        Op::Aggregate(AggregateOp {
            group_by: vec![GroupKey::column("neighbourhood")],
            summaries: vec![
                Summary::count("n"),
                Summary {
                    name: "last".into(),
                    function: SummaryFunction::Last,
                    column: Some("price".into()),
                    order: Some(OrderKey::asc("year_built")),
                },
            ],
        }),
        Op::Limit(LimitOp {
            mode: LimitMode::Sample,
            n: 10,
            seed: 7,
            group_by: Vec::new(),
            order: Vec::new(),
        }),
        Op::Stack(StackOp {
            columns: vec!["a".into(), "b".into()],
            names_to: "name".into(),
            values_to: "value".into(),
        }),
        Op::Split(SplitOp {
            names_from: "quarter".into(),
            values_from: "amount".into(),
            id_columns: vec!["region".into()],
            values: vec!["q1".into(), "q2".into()],
            summary: SplitSummary::Sum,
        }),
        Op::Complete(CompleteOp {
            columns: vec![
                CompleteColumn {
                    column: "month".into(),
                    values: CompleteValues::Range {
                        from: json!("2024-01-01"),
                        to: json!("2024-06-01"),
                        step: Some(json!("month")),
                    },
                },
                CompleteColumn {
                    column: "district".into(),
                    values: CompleteValues::Table,
                },
            ],
            fill: vec![FillValue {
                column: "n".into(),
                value: json!(0),
            }],
        }),
        Op::Join(JoinOp {
            with: Other::Table {
                table: "rates".into(),
            },
            kind: JoinKind::Left,
            on: vec![JoinKey::new("a", "b")],
            asof: Some(JoinKey::new("viewed_on", "valid_from")),
            columns: Some(vec!["rate".into()]),
            suffix: "_r".into(),
        }),
        Op::Union(UnionOp {
            with: Other::Table {
                table: "houses".into(),
            },
            source_column: Some("source".into()),
            source_labels: vec!["mine".into(), "theirs".into()],
        }),
    ];
    let mut def = DatasetDef::over_table("every kind", "houses");
    def.description = "one of each".into();
    def.operations = ops
        .into_iter()
        .enumerate()
        .map(|(i, op)| Operation::new(format!("o{i}"), op))
        .collect();
    def.operations[1].enabled = false;
    def
}

#[tokio::test]
async fn the_store_round_trips_every_operation_kind() -> Result<()> {
    for fx in both().await? {
        let def = every_kind();
        save_dataset(&fx.cat, &def).await?;
        let back = load_dataset(&fx.cat, def.id).await?.expect("stored");
        assert_eq!(back, def, "on {}", fx.backend);
        assert_eq!(
            load_dataset_by_name(&fx.cat, "every kind")
                .await?
                .map(|d| d.id),
            Some(def.id)
        );

        // An update replaces the operations and keeps the id.
        let mut edited = back.clone();
        edited.operations.truncate(3);
        edited.name = "fewer".into();
        save_dataset(&fx.cat, &edited).await?;
        let back = load_dataset(&fx.cat, def.id).await?.expect("stored");
        assert_eq!(back.operations.len(), 3);
        assert_eq!(back.name, "fewer");
        assert_eq!(list_datasets(&fx.cat).await?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_duplicate_name_is_refused_with_a_sentence() -> Result<()> {
    for fx in both().await? {
        save_dataset(&fx.cat, &DatasetDef::over_table("prices", "houses")).await?;
        let err = save_dataset(&fx.cat, &DatasetDef::over_table("prices", "viewings"))
            .await
            .expect_err("a second `prices`");
        assert!(
            err.to_string()
                .contains("a dataset called `prices` already exists"),
            "{err}"
        );
        let err = save_dataset(&fx.cat, &DatasetDef::over_table("  ", "houses"))
            .await
            .expect_err("no name");
        assert!(err.to_string().contains("needs a name"), "{err}");
    }
    Ok(())
}

#[tokio::test]
async fn a_base_is_fixed_must_exist_and_must_not_lead_back() -> Result<()> {
    for fx in both().await? {
        let err = save_dataset(&fx.cat, &DatasetDef::over_table("x", "no_such_table"))
            .await
            .expect_err("unknown table");
        assert!(
            err.to_string().contains("no table called `no_such_table`"),
            "{err}"
        );
        let err = save_dataset(&fx.cat, &DatasetDef::over_table("x", "_fd_datasets"))
            .await
            .expect_err("a system table");
        assert!(err.to_string().contains("server's own tables"), "{err}");

        let a = DatasetDef::over_table("a", "houses");
        save_dataset(&fx.cat, &a).await?;
        let mut changed = a.clone();
        changed.base = Base::table("viewings");
        let err = save_dataset(&fx.cat, &changed)
            .await
            .expect_err("changed base");
        assert!(err.to_string().contains("cannot be changed"), "{err}");

        // b on a is fine; a dataset on itself is not.
        let b = DatasetDef::new("b", Base::dataset(a.id));
        save_dataset(&fx.cat, &b).await?;
        let mut selfish = DatasetDef::over_table("c", "houses");
        selfish.base = Base::dataset(selfish.id);
        let err = save_dataset(&fx.cat, &selfish).await.expect_err("itself");
        assert!(
            err.to_string().contains("does not exist") || err.to_string().contains("itself"),
            "{err}"
        );

        // Deleting `a` while `b` reads it is refused; deleting `b` then `a` is not.
        let err = delete_dataset(&fx.cat, a.id).await.expect_err("in use");
        assert!(err.to_string().contains("`b`"), "{err}");
        assert!(delete_dataset(&fx.cat, b.id).await?);
        assert!(delete_dataset(&fx.cat, a.id).await?);
        assert!(!delete_dataset(&fx.cat, a.id).await?);
    }
    Ok(())
}

#[tokio::test]
async fn operation_ids_are_required_and_unique() -> Result<()> {
    for fx in both().await? {
        let mut def = DatasetDef::over_table("d", "houses")
            .then(Op::filter("price > 1"))
            .then(Op::filter("price > 2"));
        def.operations[1].id = def.operations[0].id.clone();
        let err = save_dataset(&fx.cat, &def).await.expect_err("repeated id");
        assert!(err.to_string().contains("two operations"), "{err}");
    }
    Ok(())
}

#[tokio::test]
async fn a_clone_is_a_new_dataset_with_a_free_name() -> Result<()> {
    for fx in both().await? {
        let def = DatasetDef::over_table("prices", "houses").then(Op::filter("price > 1"));
        save_dataset(&fx.cat, &def).await?;
        let copy = clone_dataset(&fx.cat, def.id, None).await?;
        assert_ne!(copy.id, def.id);
        assert_eq!(copy.name, "prices (copy)");
        assert_eq!(copy.operations, def.operations);
        let again = clone_dataset(&fx.cat, def.id, None).await?;
        assert_eq!(again.name, "prices (copy 2)");
        let named = clone_dataset(&fx.cat, def.id, Some("mine")).await?;
        assert_eq!(named.name, "mine");
        assert_eq!(list_datasets(&fx.cat).await?.len(), 4);
    }
    Ok(())
}
