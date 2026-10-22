//! `send_email`'s **SMTP override** (TODO-imap M3): a trigger that sends
//! through an account's own server, read from the row through a Ⱶ-join.
//!
//! What is asserted: the five `smtp_*` settings render from
//! `{{ accountⱵ… }}` — the password from a **Secret** field, revealed for the
//! template and nowhere else — and the message goes through that server
//! (recorded, since there is no public SMTP server to send it to); an account
//! with no server of its own sends through the installation's; a host that is
//! private, loopback or link-local, a port other than 465 and 587, and an
//! unencrypted connection are refused, at save time when they are typed in and
//! at send time when they are rendered, with the host named and the password
//! never; and the password is masked wherever the trigger is shown, and kept
//! when the mask is saved back.

use std::sync::Arc;

use sc_action::{ActionContext, Event, EventKind, Trigger, validate_trigger};
use sc_catalog::{
    Catalog, DataField, DataFieldKind, FieldId, FieldMeta, SecretKey, TableId,
    bootstrap_field_meta, save_field_meta,
};
use sc_core_actions::builtin_actions;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_email::{Mailer, RecordingMailer, SettingsMailer, SmtpSecurity, parse_mailbox};
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, SECRET_SENTINEL, TypeRef};
use serde_json::{Value as Json, json};

/// An account password with characters a careless encoder would mangle.
const PLAINTEXT: &str = "Grüße-€-pässwort";

fn int() -> TypeRef {
    TypeRef::Basic(BasicType::Int)
}

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

/// `mail_accounts(id, smtp_host, smtp_port, smtp_user, smtp_password secret)`
/// and `shipments(id, recipient, account → mail_accounts.id)`, with:
///
/// - account 1, `orders@example.com` on `smtp.example.com:465`, whose stored
///   password is the envelope of [`PLAINTEXT`];
/// - account 2, with no server of its own;
/// - account 3, whose server is `localhost`;
/// - account 4, on port 25.
async fn mailroom(db: &TestDb) -> Result<Arc<Catalog>> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    cat.set_secret_key(SecretKey::generate()?);
    sc_config::bootstrap_config(&cat).await?;
    bootstrap_field_meta(&cat).await?;
    cat.create_table(
        "mail_accounts",
        &[
            DataField::plain("id", int()).required().primary_key(),
            DataField::plain("smtp_host", text()),
            DataField::plain("smtp_port", int()),
            DataField::plain("smtp_user", text()),
            DataField::plain("smtp_password", text()),
        ],
    )
    .await?;
    save_field_meta(
        &cat,
        &FieldMeta::new("mail_accounts", "smtp_password").kind(DataFieldKind::Secret),
    )
    .await?;
    let mut account = DataField::plain("account", int());
    account.kind = DataFieldKind::Key {
        target_table: TableId("mail_accounts".to_owned()),
        target_field: FieldId("id".to_owned()),
        summary_field: None,
    };
    cat.create_table(
        "shipments",
        &[
            DataField::plain("id", int()).required().primary_key(),
            DataField::plain("recipient", text()),
            account,
        ],
    )
    .await?;
    assert!(
        cat.require("mail_accounts")?
            .field("smtp_password")
            .unwrap()
            .is_secret()
    );
    let envelope = cat.secret_key()?.seal(PLAINTEXT)?;
    db.pool()
        .get()
        .await
        .expect("client")
        .batch_execute(&format!(
            "INSERT INTO mail_accounts VALUES \
             (1, 'smtp.example.com', 465, 'orders@example.com', '{envelope}'), \
             (2, NULL, NULL, NULL, NULL), \
             (3, 'localhost', 587, 'orders@example.com', '{envelope}'), \
             (4, 'smtp.example.com', 25, 'orders@example.com', '{envelope}')"
        ))
        .await
        .expect("insert");
    Ok(Arc::new(cat))
}

/// The settings that send a shipment notice through the shipment's account.
fn relayed() -> Attrs {
    config(&[
        ("to", json!("{{ recipient }}")),
        ("subject", json!("Shipment {{ id }} is on its way")),
        ("text", json!("Your order has shipped.")),
        ("smtp_host", json!("{{ accountⱵsmtp_host }}")),
        ("smtp_port", json!("{{ accountⱵsmtp_port }}")),
        ("smtp_user", json!("{{ accountⱵsmtp_user }}")),
        ("smtp_password", json!("{{ accountⱵsmtp_password }}")),
    ])
}

fn shipment(account: i64) -> Event {
    Event::new(EventKind::Insert)
        .on("shipments")
        .row(json!({ "id": 9, "recipient": "ada@example.com", "account": account }))
}

fn config(entries: &[(&str, Json)]) -> Attrs {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

async fn run(
    catalog: &Catalog,
    event: &Event,
    config: &Attrs,
    mailer: &Arc<dyn Mailer>,
) -> Result<Json> {
    let engine: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let registry = builtin_actions()?;
    let action = registry.require("send_email")?.clone();
    let mut ctx = ActionContext::new(catalog, event, config, "shipped")
        .with_evaluator(&engine)
        .with_mailer(mailer);
    action.run(&mut ctx).await
}

fn recording_mailer() -> (Arc<RecordingMailer>, Arc<dyn Mailer>) {
    let recorder = Arc::new(RecordingMailer::sending_as(
        parse_mailbox("Saltcorn <saltcorn@example.com>").unwrap(),
    ));
    let mailer: Arc<dyn Mailer> = Arc::clone(&recorder) as Arc<dyn Mailer>;
    (recorder, mailer)
}

#[tokio::test]
async fn the_accounts_server_is_used_with_its_password_revealed_for_the_template() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = mailroom(&db).await?;
    let registry = builtin_actions()?;

    // `{{ accountⱵsmtp_password }}` is a join to a Secret field, and it saves:
    // a template sees secrets (M3.4).
    let trigger = Trigger::new("shipped", EventKind::Insert, "send_email")
        .on("shipments")
        .with_configuration(relayed());
    validate_trigger(&catalog, &registry, &trigger).await?;

    let (recorder, mailer) = recording_mailer();
    let result = run(&catalog, &shipment(1), &relayed(), &mailer).await?;

    let sent = recorder.only()?;
    // No `from`, so the account's login is the sender, not the installation.
    assert_eq!(sent.from.address, "orders@example.com");
    assert_eq!(sent.to[0].address, "ada@example.com");
    let server = recorder
        .servers()
        .remove(0)
        .expect("sent through the override");
    assert_eq!(server.host, "smtp.example.com");
    assert_eq!(server.port, 465);
    // 465 implies implicit TLS.
    assert_eq!(server.security, SmtpSecurity::Tls);
    assert_eq!(server.username, "orders@example.com");
    // The column holds the envelope; the template got the plaintext.
    assert_eq!(server.password, PLAINTEXT);
    // …and the settings' own debug output does not.
    assert!(!format!("{server:?}").contains(PLAINTEXT));
    assert_eq!(result["smtp_host"], json!("smtp.example.com"));
    assert!(!result.to_string().contains(PLAINTEXT), "{result}");
    Ok(())
}

#[tokio::test]
async fn an_account_with_no_server_sends_through_the_installations() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = mailroom(&db).await?;
    let (recorder, mailer) = recording_mailer();

    let result = run(&catalog, &shipment(2), &relayed(), &mailer).await?;
    assert!(matches!(recorder.servers().as_slice(), [None]));
    assert_eq!(recorder.only()?.from.address, "saltcorn@example.com");
    assert_eq!(result["smtp_host"], Json::Null);

    // And with no override settings at all, nothing changes from before.
    let plain = config(&[
        ("to", json!("{{ recipient }}")),
        ("subject", json!("hi")),
        ("text", json!("hi")),
    ]);
    run(&catalog, &shipment(1), &plain, &mailer).await?;
    assert!(matches!(recorder.servers().as_slice(), [None, None]));
    Ok(())
}

/// The relay rules, at send time: through the **real** transport (the
/// installation has no SMTP settings, and needs none for an override), so the
/// refusal is the one a server would give, before anything is connected to.
#[tokio::test]
async fn a_private_server_or_a_wrong_port_is_refused_naming_the_host_and_not_the_password()
-> Result<()> {
    let db = TestDb::new().await?;
    let catalog = mailroom(&db).await?;
    let mailer: Arc<dyn Mailer> = Arc::new(SettingsMailer::new(Arc::clone(&catalog)));

    let msg = run(&catalog, &shipment(3), &relayed(), &mailer)
        .await
        .unwrap_err()
        .to_string();
    assert!(msg.contains("localhost"), "the host is named: {msg}");
    assert!(msg.contains("private or reserved"), "{msg}");
    assert!(msg.contains("shipped"), "the trigger is named: {msg}");
    assert!(!msg.contains(PLAINTEXT), "{msg}");

    let msg = run(&catalog, &shipment(4), &relayed(), &mailer)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("smtp.example.com") && msg.contains("25"),
        "{msg}"
    );
    assert!(msg.contains("not allowed"), "{msg}");
    assert!(!msg.contains(PLAINTEXT), "{msg}");
    Ok(())
}

#[tokio::test]
async fn an_override_typed_into_the_form_is_checked_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = mailroom(&db).await?;
    let registry = builtin_actions()?;

    type Case<'a> = (&'a [(&'a str, Json)], &'a [&'a str]);
    let cases: &[Case<'_>] = &[
        (
            &[("smtp_host", json!("10.0.0.5"))],
            &["smtp_host", "private"],
        ),
        (&[("smtp_host", json!("[::1]"))], &["smtp_host", "private"]),
        (
            &[
                ("smtp_host", json!("smtp.example.com")),
                ("smtp_port", json!("25")),
            ],
            &["smtp_port", "25"],
        ),
        (
            &[
                ("smtp_host", json!("smtp.example.com")),
                ("smtp_port", json!("465")),
                ("smtp_security", json!("starttls")),
            ],
            &["smtp_port", "465", "starttls"],
        ),
        (
            &[
                ("smtp_host", json!("smtp.example.com")),
                ("smtp_security", json!("none")),
            ],
            &["smtp_security", "none"],
        ),
        // The other settings are an override of nothing without a host.
        (
            &[("smtp_user", json!("orders@example.com"))],
            &["smtp_user", "smtp_host"],
        ),
        // A token naming a field the account does not have.
        (
            &[("smtp_host", json!("{{ accountⱵsmtp_hots }}"))],
            &["smtp_host", "smtp_hots"],
        ),
    ];
    for (entries, expected) in cases {
        let mut cfg = config(&[
            ("to", json!("{{ recipient }}")),
            ("subject", json!("hi")),
            ("text", json!("hi")),
        ]);
        cfg.extend(config(entries));
        let trigger = Trigger::new("shipped", EventKind::Insert, "send_email")
            .on("shipments")
            .with_configuration(cfg);
        let msg = validate_trigger(&catalog, &registry, &trigger)
            .await
            .unwrap_err()
            .to_string();
        for fragment in *expected {
            assert!(msg.contains(fragment), "expected `{fragment}` in: {msg}");
        }
    }
    Ok(())
}

/// A trigger **on the account table itself** reads its own secret bare: the
/// event's row is revealed for the action, and the template scope has it.
#[tokio::test]
async fn a_trigger_on_the_account_table_reads_its_own_secret() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = mailroom(&db).await?;
    let registry = builtin_actions()?;
    let cfg = config(&[
        ("to", json!("ada@example.com")),
        ("subject", json!("account {{ id }}")),
        ("text", json!("hi")),
        ("smtp_host", json!("{{ smtp_host }}")),
        ("smtp_user", json!("{{ smtp_user }}")),
        ("smtp_password", json!("{{ smtp_password }}")),
    ]);
    let trigger = Trigger::new("account_check", EventKind::Update, "send_email")
        .on("mail_accounts")
        .with_configuration(cfg.clone());
    validate_trigger(&catalog, &registry, &trigger).await?;

    // What the dispatcher hands an action: the stored row, revealed.
    let event = Event::new(EventKind::Update)
        .on("mail_accounts")
        .row(json!({
            "id": 1,
            "smtp_host": "smtp.example.com",
            "smtp_port": 587,
            "smtp_user": "orders@example.com",
            "smtp_password": PLAINTEXT,
        }));
    let (recorder, mailer) = recording_mailer();
    run(&catalog, &event, &cfg, &mailer).await?;
    let server = recorder
        .servers()
        .remove(0)
        .expect("sent through the override");
    assert_eq!(server.password, PLAINTEXT);
    // No port given and no security given: submission over STARTTLS.
    assert_eq!(
        (server.port, server.security),
        (587, SmtpSecurity::StartTls)
    );
    Ok(())
}

/// `smtp_password` is a secret setting: masked wherever the trigger is shown,
/// and the mask saved back keeps what is stored.
#[tokio::test]
async fn the_password_setting_is_masked_where_the_trigger_is_shown() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = mailroom(&db).await?;
    let registry = builtin_actions()?;

    let mut cfg = relayed();
    cfg.insert("smtp_password".to_owned(), json!("typed-in secret"));
    let stored = Trigger::new("shipped", EventKind::Insert, "send_email")
        .on("shipments")
        .with_configuration(cfg);

    let shown = registry.redact_trigger(&catalog, &stored);
    let shown_config = shown.configuration().unwrap();
    assert_eq!(shown_config["smtp_password"], json!(SECRET_SENTINEL));
    // Everything else is as stored.
    assert_eq!(shown_config["smtp_host"], json!("{{ accountⱵsmtp_host }}"));
    assert!(!format!("{shown:?}").contains("typed-in secret"));

    // The editor saves back what it was shown: the stored value is kept.
    let mut submitted = shown.clone();
    registry.merge_trigger_secrets(&catalog, Some(&stored), &mut submitted)?;
    assert_eq!(
        submitted.configuration().unwrap()["smtp_password"],
        json!("typed-in secret")
    );

    // A new value replaces it.
    let mut changed = shown.clone();
    let mut changed_config = changed.configuration().unwrap().clone();
    changed_config.insert("smtp_password".to_owned(), json!("a new one"));
    changed.set_configuration(changed_config)?;
    registry.merge_trigger_secrets(&catalog, Some(&stored), &mut changed)?;
    assert_eq!(
        changed.configuration().unwrap()["smtp_password"],
        json!("a new one")
    );

    // And the mask with nothing stored under it is dropped, never saved as
    // the password.
    let mut created = shown;
    registry.merge_trigger_secrets(&catalog, None, &mut created)?;
    assert!(
        !created
            .configuration()
            .unwrap()
            .contains_key("smtp_password")
    );
    Ok(())
}
