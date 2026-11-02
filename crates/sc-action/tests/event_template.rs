//! Rendering an action's template against a real event, over a real database
//! and a real V8 (TODO "Email" Phase 1).
//!
//! What is asserted here and cannot be asserted in a unit test: that a `{{ }}`
//! token naming a **Ⱶ-join path** is *prefetched* — the whole reason
//! [`render_event_template`] exists rather than the templates riding on
//! `event_formula_value`, which does not prefetch and would render the customer's
//! address as an unbound-name error. Also that the bare scope of a template is
//! the event's row, that a null column renders as nothing, and that a token
//! naming a field the table does not have is refused by [`check_template`] while
//! the admin is still looking at the form.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::Arc;

use sc_action::{
    ActionContext, EVENT_SCOPE, Event, EventKind, action_shape, check_template,
    render_event_template, template_scope,
};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator, RenderMode, Template};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::json;

fn int() -> TypeRef {
    TypeRef::Basic(BasicType::Int)
}

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

/// `customers(id, email)` and `orders(id, total, note, customer → customers.id)`,
/// with one customer to join to.
async fn shop(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    cat.create_table(
        "customers",
        &[
            DataField::plain("id", int()).required().primary_key(),
            DataField::plain("email", text()),
        ],
    )
    .await?;
    let mut customer = DataField::plain("customer", int());
    customer.kind = DataFieldKind::Key {
        target_table: "customers".to_owned(),
        target_field: FieldId("id".to_owned()),
        summary_field: None,
    };
    cat.create_table(
        "orders",
        &[
            DataField::plain("id", int()).required().primary_key(),
            DataField::plain("total", int()),
            DataField::plain("note", text()),
            customer,
        ],
    )
    .await?;
    db.pool()
        .get()
        .await
        .expect("client")
        .batch_execute("INSERT INTO customers VALUES (7, 'ada@example.com')")
        .await
        .expect("insert");
    Ok(cat)
}

/// The event a row-scoped trigger on `orders` would carry: the order, and the
/// admin who pressed the button.
fn order_event() -> Event {
    Event::new(EventKind::None)
        .on("orders")
        .row(json!({ "id": 42, "total": 250, "note": null, "customer": 7 }))
        .caller(Role::ADMIN, Some(json!({ "email": "admin@example.com" })))
}

#[tokio::test]
async fn a_template_renders_the_rows_own_values_and_prefetches_a_join() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = shop(&db).await?;
    let event = order_event();
    let config = Attrs::new();
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let ctx = ActionContext::new(&cat, &event, &config, "receipt").with_evaluator(&evaluator);

    // The bare scope is the event's row, so a subject line reads like prose.
    let subject = Template::parse("Receipt for order {{ id }}").unwrap();
    assert_eq!(
        render_event_template(&ctx, &subject, "`subject`", RenderMode::Text).await?,
        "Receipt for order 42"
    );

    // The recipient is a Ⱶ-path: nothing in the event holds the address, so this
    // only renders if the value was fetched from `customers`.
    let to = Template::parse("{{ customerⱵemail }}").unwrap();
    assert_eq!(
        render_event_template(&ctx, &to, "`to`", RenderMode::Text).await?,
        "ada@example.com"
    );

    // `row.x` is the row's own column spelled the other way, `user` is the
    // caller, and an expression is an expression.
    let body =
        Template::parse("<p>#{{ row.id }} — {{ total * 2 }} from {{ user.email }}</p>").unwrap();
    assert_eq!(
        render_event_template(&ctx, &body, "`html`", RenderMode::Html).await?,
        "<p>#42 — 500 from admin@example.com</p>"
    );
    Ok(())
}

#[tokio::test]
async fn a_null_column_renders_as_nothing_and_a_constant_needs_no_engine() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = shop(&db).await?;
    let event = order_event();
    let config = Attrs::new();
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let ctx = ActionContext::new(&cat, &event, &config, "receipt").with_evaluator(&evaluator);

    // A null column is data, not a mistake (decision 4).
    let note = Template::parse("Note: [{{ note }}]").unwrap();
    assert_eq!(
        render_event_template(&ctx, &note, "`text`", RenderMode::Text).await?,
        "Note: []"
    );

    // A template with no token never reaches the isolate — asserted by rendering
    // one through a context that has no evaluator at all.
    let plain = ActionContext::new(&cat, &event, &config, "receipt");
    let constant = Template::parse("Your receipt").unwrap();
    assert_eq!(
        render_event_template(&plain, &constant, "`subject`", RenderMode::Text).await?,
        "Your receipt"
    );
    // One with a token, in that same context, says so rather than sending an
    // email with a hole in it.
    let err = render_event_template(&plain, &note, "`text`", RenderMode::Text)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("receipt"), "{err}");
    Ok(())
}

#[tokio::test]
async fn check_template_refuses_a_missing_field_at_save_time() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = shop(&db).await?;

    let shape = action_shape(&cat, Some("orders"))?;
    let scope = template_scope(Some("orders"));
    assert_eq!(scope, "orders");

    let ok = Template::parse("Order {{ id }} for {{ customerⱵemail }}").unwrap();
    check_template(&shape, scope, &ok, "`subject`")?;

    let bad = Template::parse("Order {{ id }} for {{ custmerⱵemail }}").unwrap();
    let msg = check_template(&shape, scope, &bad, "`subject`")
        .unwrap_err()
        .to_string();
    assert!(msg.contains("`subject`"), "the setting: {msg}");
    assert!(msg.contains("custmer"), "the identifier: {msg}");
    assert!(msg.contains("{{ custmerⱵemail }}"), "the token: {msg}");

    // A trigger with no table has no row to range over, so a bare identifier is
    // refused there — the same answer a configured formula gets.
    let shape = action_shape(&cat, None)?;
    let scope = template_scope(None);
    assert_eq!(scope, EVENT_SCOPE);
    let msg = check_template(&shape, scope, &ok, "`subject`")
        .unwrap_err()
        .to_string();
    assert!(msg.contains("unknown identifier"), "{msg}");
    // The payload is in scope for every kind, so a table-less trigger is not
    // left with nothing to say.
    let payload = Template::parse("Ran for {{ payload.who }}").unwrap();
    check_template(&shape, scope, &payload, "`subject`")?;
    Ok(())
}
